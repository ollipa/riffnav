//! The `riffnav comment …` subcommands — the interface an AI agent uses.
//!
//! These run as their own short-lived process with no TUI. They read and write
//! the same store the running window watches ([`crate::comment::store`]), so a
//! note written here shows up on screen within a moment, and a window doesn't
//! even have to be open.
//!
//! Anchors are validated before anything is written: naming a file that isn't in
//! the diff, or a line outside every hunk, is an error that names the valid
//! ranges. Getting that wrong is the single most common way an agent's first
//! attempt fails, so the error has to be actionable rather than silent.

use std::io::Read;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::{AddArgs, Command, CommentCmd};
use crate::comment::{Comment, CommentStore, Side};
use crate::session::Session;
use crate::state;

/// Retention for a store opened by the CLI. The TUI's configured value governs
/// real garbage collection; the CLI just needs a window wide enough not to
/// discard anything the user can still see.
const CLI_RETENTION_DAYS: u64 = 365;

/// The skill text `riffnav skill` prints, teaching an agent these commands.
const SKILL: &str = include_str!("skill.md");

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Comment(cmd) => comment(cmd),
        Command::Skill { path } => skill(path),
        // Routed in `main`: these open the TUI rather than running as CLI tools.
        Command::Diff(_) | Command::Show { .. } => unreachable!(),
    }
}

fn comment(cmd: CommentCmd) -> Result<()> {
    match cmd {
        CommentCmd::Add(args) => add(args),
        CommentCmd::Apply { stdin } => apply(stdin),
        CommentCmd::List { file, author, json } => list(file, author, json),
        CommentCmd::Rm { id } => remove(&id),
        CommentCmd::Clear { file, yes } => clear(file, yes),
        CommentCmd::Context { json } => context(json),
    }
}

/// One item of a `comment apply` batch. Mirrors `comment add`'s flags so an
/// agent can move between the two without relearning the shape.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BatchItem {
    /// Omitted on a `replyTo` item, which inherits its parent's anchor.
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    new_line: Option<u32>,
    #[serde(default)]
    old_line: Option<u32>,
    body: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    comments: Vec<BatchItem>,
}

/// A validated anchor, ready to become a stored comment.
#[derive(Debug)]
struct Resolved {
    file: String,
    side: Side,
    line: u32,
    /// `None` only when inherited from a parent that has none (hand-edited).
    diff_hash: Option<String>,
}

fn add(args: AddArgs) -> Result<()> {
    let body = read_body(&args.body)?;
    // A reply validates against nothing, so it doesn't need a diff — and by the
    // time one is written there may not be one left to need.
    let session = match args.reply_to {
        Some(_) => None,
        None => Some(require_session()?),
    };
    let mut store = CommentStore::load(CLI_RETENTION_DAYS);
    let resolved = anchor(
        session.as_ref(),
        &store,
        args.file.as_deref(),
        args.old_line,
        args.new_line,
        args.reply_to.as_deref(),
        "comment add",
    )?;

    let id = store.add(build(&resolved, body, args.author, args.reply_to));
    store.save();
    println!("#{id}  {}:{}", resolved.file, resolved.line);
    Ok(())
}

fn apply(stdin: bool) -> Result<()> {
    if !stdin {
        bail!("pass --stdin: `comment apply` reads its JSON batch from stdin");
    }
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("reading the comment batch from stdin")?;
    let batch: Batch = serde_json::from_str(&text).context(
        "parsing the comment batch — expected \
         {\"comments\":[{\"file\":…,\"newLine\":…,\"body\":…}]}",
    )?;
    if batch.comments.is_empty() {
        bail!("the batch contains no comments");
    }

    // Only an item that anchors itself needs the diff; a batch of pure replies
    // applies whether or not one is still there.
    let session = match batch.comments.iter().any(|i| i.reply_to.is_none()) {
        true => Some(require_session()?),
        false => None,
    };
    let mut store = CommentStore::load(CLI_RETENTION_DAYS);

    // Resolve everything first: a batch that half-applies would leave the review
    // in a state the agent didn't ask for and can't easily reason about.
    let mut resolved = Vec::with_capacity(batch.comments.len());
    for (i, item) in batch.comments.iter().enumerate() {
        let where_ = format!("comment {} of {}", i + 1, batch.comments.len());
        let r = anchor(
            session.as_ref(),
            &store,
            item.file.as_deref(),
            item.old_line,
            item.new_line,
            item.reply_to.as_deref(),
            &where_,
        )?;
        if item.body.trim().is_empty() {
            bail!("{where_}: body is empty");
        }
        resolved.push(r);
    }

    for (r, item) in resolved.iter().zip(batch.comments) {
        let id = store.add(build(r, item.body, item.author, item.reply_to));
        println!("#{id}  {}:{}", r.file, r.line);
    }
    store.save();
    Ok(())
}

fn list(file: Option<String>, author: Option<String>, json: bool) -> Result<()> {
    let store = CommentStore::load(CLI_RETENTION_DAYS);
    let selected: Vec<&Comment> = store
        .all()
        .iter()
        // Selected by the file the note renders in, which for a reply is its
        // thread's — the same file the window would list it under.
        .filter(|c| file.as_deref().is_none_or(|f| store.root_of(c).file == f))
        .filter(|c| author.as_deref().is_none_or(|a| c.author == a))
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&selected)?);
        return Ok(());
    }
    if selected.is_empty() {
        println!("No comments.");
        return Ok(());
    }
    for c in selected {
        // Print where the note actually hangs, not what it stores: a thread sits
        // on its root's anchor, so a reply carrying a line of its own renders at
        // its parent's. Printing the stored one is what once let a detached reply
        // look correctly threaded here while the window showed it adrift.
        let root = store.root_of(c);
        let mut reply = String::new();
        if let Some(p) = &c.reply_to {
            reply = format!(" ↳#{p}");
            if store.get(p).is_none() {
                reply.push_str(" (gone — renders as its own thread)");
            } else if (&root.file, root.side, root.line) != (&c.file, c.side, c.line) {
                reply.push_str(&format!(" (stored at {}:{})", c.file, c.line));
            }
        }
        println!(
            "#{}  {}:{} ({}){reply}  — {}",
            c.id,
            root.file,
            root.line,
            root.side.as_str(),
            c.author
        );
        for line in c.body.lines() {
            println!("    {line}");
        }
    }
    Ok(())
}

fn remove(id: &str) -> Result<()> {
    let mut store = CommentStore::load(CLI_RETENTION_DAYS);
    match store.remove(id) {
        0 => bail!("no comment with id {id} (see `riffnav comment list`)"),
        n => {
            store.save();
            println!("Removed {n} comment(s).");
            Ok(())
        }
    }
}

fn clear(file: Option<String>, yes: bool) -> Result<()> {
    if !yes {
        bail!("pass --yes to confirm: clearing comments can't be undone");
    }
    let mut store = CommentStore::load(CLI_RETENTION_DAYS);
    let n = store.clear(file.as_deref());
    store.save();
    println!("Removed {n} comment(s).");
    Ok(())
}

fn context(json: bool) -> Result<()> {
    let session = require_session()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&session)?);
        return Ok(());
    }
    println!("{} ({} files)", session.source, session.files.len());
    for f in &session.files {
        println!(
            "  {} {}  +{} -{}   new: {}   old: {}",
            f.status,
            f.path,
            f.additions,
            f.deletions,
            f.ranges(Side::New),
            f.ranges(Side::Old),
        );
    }
    Ok(())
}

fn skill(as_path: bool) -> Result<()> {
    if !as_path {
        print!("{SKILL}");
        return Ok(());
    }
    let dir = state::dir("skill").context("no state directory to write the skill into")?;
    let path = dir.join("riffnav-review").join("SKILL.md");
    if !state::write_atomic(&path, SKILL.as_bytes()) {
        bail!("couldn't write the skill to {}", path.display());
    }
    println!("{}", path.display());
    Ok(())
}

/// The file set on screen, or a clear explanation of why there isn't one.
///
/// Falls back to re-deriving the diff from git when no window has published a
/// session, so an agent can leave notes before the user opens riffnav.
fn require_session() -> Result<Session> {
    if let Some(session) = Session::load() {
        return Ok(session);
    }
    if !crate::autodiff::in_repo() {
        bail!(
            "no riffnav session and not inside a git repository\n\
             open riffnav in the repo you want to comment on, or run this from inside it"
        );
    }
    let base = crate::autodiff::detect_base();
    let (source, text) = crate::autodiff::load_initial(base.as_deref())
        .context("deriving the current diff from git")?;
    let files = crate::diff::parse(&text);
    if files.is_empty() {
        bail!("no changes to comment on in this repository");
    }
    Ok(Session::new(&files, source.label(), base))
}

/// Where one comment will hang: the anchor it names, or — for a reply — the one
/// it inherits.
///
/// A reply is never anchored independently. The diff moves while a review runs
/// (comment, fix, reply is the normal loop), so a line resolved *now* is often
/// not the line the parent was written against: honoring it would file the reply
/// somewhere else and break the thread apart. Inheriting instead makes that
/// impossible, and works even when the parent's line has left the diff entirely
/// — or when there's no diff left at all, which is why `session` is optional.
fn anchor(
    session: Option<&Session>,
    store: &CommentStore,
    file: Option<&str>,
    old_line: Option<u32>,
    new_line: Option<u32>,
    reply_to: Option<&str>,
    where_: &str,
) -> Result<Resolved> {
    let Some(id) = reply_to else {
        let (Some(session), Some(file)) = (session, file) else {
            bail!(
                "{where_}: name the file to comment on, or --reply-to a comment \
                 to thread under"
            );
        };
        return resolve(session, store, file, old_line, new_line, where_);
    };
    // A `--reply-to` that names nothing would silently become a root comment.
    let Some(parent) = store.get(id) else {
        bail!("{where_}: no comment with id {id} to reply to (see `riffnav comment list`)");
    };
    if file.is_some() || old_line.is_some() || new_line.is_some() {
        bail!(
            "{where_}: a reply carries no anchor of its own — it inherits #{id}'s \
             ({}:{} on the {} side)\n  \
             drop the file and line: a reply always sits with the note it answers",
            parent.file,
            parent.line,
            parent.side.as_str(),
        );
    }
    Ok(Resolved {
        file: parent.file.clone(),
        side: parent.side,
        line: parent.line,
        // The parent's hash, not the file's current one: a reply stamped with a
        // fresher hash would render current under a parent marked stale, which
        // reads as though half the thread were about different code.
        diff_hash: parent.diff_hash.clone(),
    })
}

/// Validate one anchor against the diff, naming what would have been valid.
fn resolve(
    session: &Session,
    store: &CommentStore,
    file: &str,
    old_line: Option<u32>,
    new_line: Option<u32>,
    where_: &str,
) -> Result<Resolved> {
    let Some(entry) = session.file(file) else {
        bail!(
            "{where_}: no file `{file}` in this diff\n{}",
            file_hint(session)
        );
    };
    let (side, line) = match (old_line, new_line) {
        (None, Some(line)) => (Side::New, line),
        (Some(line), None) => (Side::Old, line),
        (None, None) => bail!("{where_}: pass one of --new-line or --old-line"),
        (Some(_), Some(_)) => bail!("{where_}: pass only one of --new-line or --old-line"),
    };
    if !entry.covers(side, line) {
        bail!(
            "{where_}: line {line} is not in {}'s diff on the {} side\n  \
             commentable {} lines: {}\n{}",
            entry.path,
            side.as_str(),
            side.as_str(),
            entry.ranges(side),
            target_hint(session, store, entry),
        );
    }
    Ok(Resolved {
        file: entry.path.clone(),
        side,
        line,
        diff_hash: Some(entry.diff_hash.clone()),
    })
}

/// Which diff the rejected line was measured against, and whether that ground
/// has moved since the existing comments on the file were written.
///
/// The anchor space is a property of the diff on screen, and that flips on its
/// own — `branch vs base` becomes `all uncommitted` the moment the tree is
/// dirty. Without this the error reads as "you picked a bad line" when the truth
/// is "the lines you were reading are no longer the ones in force".
fn target_hint(
    session: &Session,
    store: &CommentStore,
    entry: &crate::session::SessionFile,
) -> String {
    let moved = store.all().iter().any(|c| {
        c.file == entry.path && c.diff_hash.as_ref().is_some_and(|h| *h != entry.diff_hash)
    });
    let mut hint = format!("  the diff in force is `{}`", session.source);
    if moved {
        hint.push_str(
            "\n  the existing comments on this file were written against a different one, \
             so the lines you are working from have moved",
        );
    }
    hint
}

fn build(r: &Resolved, body: String, author: Option<String>, reply_to: Option<String>) -> Comment {
    Comment {
        id: String::new(),
        file: r.file.clone(),
        side: r.side,
        line: r.line,
        body: body.trim().to_string(),
        author: author.unwrap_or_else(default_author),
        created: state::now_unix(),
        reply_to,
        diff_hash: r.diff_hash.clone(),
    }
}

fn default_author() -> String {
    std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "agent".to_string())
}

/// `--body -` reads the text from stdin, so a long or quote-heavy comment
/// doesn't have to survive shell quoting.
fn read_body(arg: &str) -> Result<String> {
    let body = if arg == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .context("reading the comment body from stdin")?;
        text
    } else {
        arg.to_string()
    };
    if body.trim().is_empty() {
        bail!("the comment body is empty");
    }
    Ok(body)
}

/// The first few paths in the diff, to orient an agent that guessed wrong.
fn file_hint(session: &Session) -> String {
    let shown: Vec<&str> = session
        .files
        .iter()
        .take(5)
        .map(|f| f.path.as_str())
        .collect();
    let more = session.files.len().saturating_sub(shown.len());
    let suffix = if more > 0 {
        format!("\n  … and {more} more — see `riffnav comment context`")
    } else {
        String::new()
    };
    format!("  files in this diff: {}{suffix}", shown.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        let files = crate::diff::parse(
            "diff --git a/src/app.rs b/src/app.rs\n--- a/src/app.rs\n+++ b/src/app.rs\n\
             @@ -10,3 +12,4 @@\n ctx\n-gone\n+one\n+two\n\
             diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n\
             @@ -1,2 +1,2 @@\n-a\n+b\n c\n",
        );
        Session::new(&files, "stdin", None)
    }

    /// A store holding one comment, to reply to.
    fn store_with(file: &str, line: u32, diff_hash: Option<&str>) -> (CommentStore, String) {
        let mut store = CommentStore::disabled();
        let id = store.add(Comment {
            id: String::new(),
            file: file.to_string(),
            side: Side::New,
            line,
            body: "parent".to_string(),
            author: "claude".to_string(),
            created: 100,
            reply_to: None,
            diff_hash: diff_hash.map(str::to_string),
        });
        (store, id)
    }

    /// `anchor` with no reply target: the plain `--file`/`--line` path.
    fn plain(
        session: &Session,
        file: &str,
        old: Option<u32>,
        new: Option<u32>,
    ) -> Result<Resolved> {
        anchor(
            Some(session),
            &CommentStore::disabled(),
            Some(file),
            old,
            new,
            None,
            "test",
        )
    }

    #[test]
    fn resolves_a_line_inside_a_hunk() {
        let r = plain(&session(), "src/app.rs", None, Some(13)).unwrap();
        assert_eq!(r.file, "src/app.rs");
        assert_eq!(r.side, Side::New);
        assert_eq!(r.line, 13);
        assert_eq!(r.diff_hash.unwrap().len(), 32);
    }

    #[test]
    fn an_unknown_file_lists_what_is_actually_there() {
        let err = plain(&session(), "src/nope.rs", None, Some(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no file `src/nope.rs`"), "{err}");
        assert!(err.contains("src/app.rs"), "the error should orient: {err}");
    }

    #[test]
    fn a_line_outside_every_hunk_names_the_valid_ranges() {
        let err = plain(&session(), "src/app.rs", None, Some(999))
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 999 is not in"), "{err}");
        assert!(err.contains("12-15"), "the error should list ranges: {err}");
    }

    /// The anchor space belongs to whichever diff is on screen, and that changes
    /// under a review on its own, so a rejected line has to say which one judged
    /// it — and that the ground moved, when the stored comments prove it did.
    #[test]
    fn a_rejected_line_names_the_diff_it_was_measured_against() {
        let s = session();
        let clean = CommentStore::disabled();
        let err = resolve(&s, &clean, "src/app.rs", None, Some(999), "test")
            .unwrap_err()
            .to_string();
        assert!(err.contains("the diff in force is `stdin`"), "{err}");
        assert!(
            !err.contains("have moved"),
            "nothing says the ground moved yet: {err}"
        );

        // A comment written against a different revision of this file's diff.
        let (stale, _) = store_with("src/app.rs", 13, Some("deadbeef"));
        let err = resolve(&s, &stale, "src/app.rs", None, Some(999), "test")
            .unwrap_err()
            .to_string();
        assert!(err.contains("have moved"), "{err}");
    }

    #[test]
    fn exactly_one_side_must_be_given() {
        let s = session();
        assert!(
            plain(&s, "src/app.rs", None, None)
                .unwrap_err()
                .to_string()
                .contains("one of")
        );
        assert!(
            plain(&s, "src/app.rs", Some(10), Some(13))
                .unwrap_err()
                .to_string()
                .contains("only one")
        );
    }

    #[test]
    fn a_comment_that_is_not_a_reply_must_name_a_file() {
        let err = anchor(
            Some(&session()),
            &CommentStore::disabled(),
            None,
            None,
            Some(13),
            None,
            "test",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("name the file"), "{err}");
    }

    #[test]
    fn the_old_side_resolves_against_pre_image_numbers() {
        let r = plain(&session(), "src/app.rs", Some(11), None).unwrap();
        assert_eq!(r.side, Side::Old);
        assert_eq!(r.line, 11);
    }

    #[test]
    fn a_reply_to_an_unknown_id_is_rejected() {
        let err = anchor(
            Some(&session()),
            &CommentStore::disabled(),
            None,
            None,
            None,
            Some("nope12"),
            "test",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no comment with id nope12"), "{err}");
    }

    /// The whole point of fixing this: the parent's line has left the diff (the
    /// fix it asked for landed), and replying to it still works — at the parent's
    /// anchor, carrying the parent's hash so the thread doesn't read half-stale.
    /// It works with no diff at all (`None`), which is what a committed fix or a
    /// clean tree leaves behind.
    #[test]
    fn a_reply_inherits_its_parents_anchor_even_when_that_line_is_gone() {
        let (store, parent) = store_with("src/app.rs", 999, Some("deadbeef"));
        let r = anchor(None, &store, None, None, None, Some(&parent), "test")
            .expect("a reply is not re-validated against the current diff");
        assert_eq!(
            (r.file.as_str(), r.side, r.line),
            ("src/app.rs", Side::New, 999)
        );
        assert_eq!(r.diff_hash.as_deref(), Some("deadbeef"));
    }

    /// A reply that names its own line is refused rather than honored: honoring
    /// it is what detached replies from their thread.
    #[test]
    fn a_reply_may_not_carry_an_anchor_of_its_own() {
        let (store, parent) = store_with("src/app.rs", 13, None);
        let s = session();
        for (file, old, new) in [
            (Some("src/app.rs"), None, None),
            (None, None, Some(14)),
            (None, Some(11), None),
        ] {
            let err = anchor(Some(&s), &store, file, old, new, Some(&parent), "test")
                .unwrap_err()
                .to_string();
            assert!(err.contains("carries no anchor of its own"), "{err}");
            assert!(
                err.contains("src/app.rs:13"),
                "it names what it inherits: {err}"
            );
        }
    }

    #[test]
    fn batch_json_uses_the_documented_camel_case_shape() {
        let batch: Batch = serde_json::from_str(
            r#"{"comments":[{"file":"src/app.rs","newLine":13,"body":"why?"}]}"#,
        )
        .expect("the shape in the skill must parse");
        assert_eq!(batch.comments[0].new_line, Some(13));
        assert_eq!(batch.comments[0].old_line, None);
        // A misspelled field is rejected rather than silently ignored.
        assert!(
            serde_json::from_str::<Batch>(r#"{"comments":[{"file":"f","line":1,"body":"x"}]}"#)
                .is_err()
        );
    }

    /// The batch path takes the same anchors as `add`, so it has to refuse the
    /// same thing: a `replyTo` item may not also name a line.
    #[test]
    fn a_batch_reply_carrying_a_line_is_rejected_and_one_without_applies() {
        let (store, parent) = store_with("src/app.rs", 999, Some("deadbeef"));
        let s = session();
        let batch: Batch = serde_json::from_str(&format!(
            r#"{{"comments":[
                 {{"replyTo":"{parent}","newLine":13,"body":"done"}},
                 {{"replyTo":"{parent}","body":"done"}}
               ]}}"#
        ))
        .expect("a replyTo item needs no file");

        let resolve_item = |item: &BatchItem| {
            anchor(
                Some(&s),
                &store,
                item.file.as_deref(),
                item.old_line,
                item.new_line,
                item.reply_to.as_deref(),
                "test",
            )
        };
        assert!(
            resolve_item(&batch.comments[0])
                .unwrap_err()
                .to_string()
                .contains("carries no anchor of its own")
        );
        // The same item without the line applies, though the parent's line is no
        // longer anywhere in the diff.
        assert_eq!(resolve_item(&batch.comments[1]).unwrap().line, 999);
    }

    #[test]
    fn an_empty_body_is_refused_before_anything_is_written() {
        assert!(read_body("   ").is_err());
        assert_eq!(read_body("ok").unwrap(), "ok");
    }
}

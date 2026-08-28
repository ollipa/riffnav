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
    file: String,
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
    diff_hash: String,
}

fn add(args: AddArgs) -> Result<()> {
    let session = require_session()?;
    let body = read_body(&args.body)?;
    let resolved = resolve(
        &session,
        &args.file,
        args.old_line,
        args.new_line,
        "comment add",
    )?;

    let mut store = CommentStore::load(CLI_RETENTION_DAYS);
    check_reply_target(&store, args.reply_to.as_deref())?;
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

    let session = require_session()?;
    let mut store = CommentStore::load(CLI_RETENTION_DAYS);

    // Resolve everything first: a batch that half-applies would leave the review
    // in a state the agent didn't ask for and can't easily reason about.
    let mut resolved = Vec::with_capacity(batch.comments.len());
    for (i, item) in batch.comments.iter().enumerate() {
        let where_ = format!("comment {} of {}", i + 1, batch.comments.len());
        let r = resolve(&session, &item.file, item.old_line, item.new_line, &where_)?;
        if item.body.trim().is_empty() {
            bail!("{where_}: body is empty");
        }
        check_reply_target(&store, item.reply_to.as_deref())?;
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
        .filter(|c| file.as_deref().is_none_or(|f| c.file == f))
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
        let reply = c
            .reply_to
            .as_ref()
            .map_or(String::new(), |p| format!(" ↳#{p}"));
        println!(
            "#{}  {}:{} ({}){reply}  — {}",
            c.id,
            c.file,
            c.line,
            c.side.as_str(),
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

/// Validate one anchor against the diff, naming what would have been valid.
fn resolve(
    session: &Session,
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
             commentable {} lines: {}",
            entry.path,
            side.as_str(),
            side.as_str(),
            entry.ranges(side),
        );
    }
    Ok(Resolved {
        file: entry.path.clone(),
        side,
        line,
        diff_hash: entry.diff_hash.clone(),
    })
}

/// A `--reply-to` that names nothing would silently become a root comment, so
/// reject it instead.
fn check_reply_target(store: &CommentStore, reply_to: Option<&str>) -> Result<()> {
    if let Some(id) = reply_to
        && store.get(id).is_none()
    {
        bail!("no comment with id {id} to reply to (see `riffnav comment list`)");
    }
    Ok(())
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
        diff_hash: Some(r.diff_hash.clone()),
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

    #[test]
    fn resolves_a_line_inside_a_hunk() {
        let r = resolve(&session(), "src/app.rs", None, Some(13), "test").unwrap();
        assert_eq!(r.file, "src/app.rs");
        assert_eq!(r.side, Side::New);
        assert_eq!(r.line, 13);
        assert_eq!(r.diff_hash.len(), 32);
    }

    #[test]
    fn an_unknown_file_lists_what_is_actually_there() {
        let err = resolve(&session(), "src/nope.rs", None, Some(1), "test")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no file `src/nope.rs`"), "{err}");
        assert!(err.contains("src/app.rs"), "the error should orient: {err}");
    }

    #[test]
    fn a_line_outside_every_hunk_names_the_valid_ranges() {
        let err = resolve(&session(), "src/app.rs", None, Some(999), "test")
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 999 is not in"), "{err}");
        assert!(err.contains("12-15"), "the error should list ranges: {err}");
    }

    #[test]
    fn exactly_one_side_must_be_given() {
        let s = session();
        assert!(
            resolve(&s, "src/app.rs", None, None, "test")
                .unwrap_err()
                .to_string()
                .contains("one of")
        );
        assert!(
            resolve(&s, "src/app.rs", Some(10), Some(13), "test")
                .unwrap_err()
                .to_string()
                .contains("only one")
        );
    }

    #[test]
    fn the_old_side_resolves_against_pre_image_numbers() {
        let r = resolve(&session(), "src/app.rs", Some(11), None, "test").unwrap();
        assert_eq!(r.side, Side::Old);
        assert_eq!(r.line, 11);
    }

    #[test]
    fn a_reply_to_an_unknown_id_is_rejected() {
        let store = CommentStore::disabled();
        assert!(check_reply_target(&store, None).is_ok());
        assert!(check_reply_target(&store, Some("nope12")).is_err());
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

    #[test]
    fn an_empty_body_is_refused_before_anything_is_written() {
        assert!(read_body("   ").is_err());
        assert_eq!(read_body("ok").unwrap(), "ok");
    }
}

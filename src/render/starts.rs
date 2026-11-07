//! Where each hunk's parser really starts.
//!
//! A hunk can begin inside a block comment or a multi-line string, and only the
//! lines above it say so. When riffnav ran git itself, one blob is enough: the
//! `index` line names the old side, and the hunks turn that into the new side as
//! far down as the last of them. The work tree is never read: it rarely has a blob,
//! can change at any moment, and filters leave it in a form the diff doesn't show.
//!
//! Parsing runs at roughly 20 lines of Rust a millisecond, so a 3,500-line file
//! costs 200ms and one at the size limit most of a second. It's the first step of
//! a render, off the UI thread, so there's no second render to make.
//!
//! The same blob names the language of a file whose name doesn't and whose diff
//! doesn't show its first line, such as an extensionless script changed far down.
//!
//! The answer is kept by the hash of the diff text, which names the blob and every
//! hunk, so resizing or changing theme costs nothing. A file the diff can't be
//! matched to keeps its hunks starting afresh.

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

use twox_hash::XxHash3_128;

use super::blobs::read_blob;
use super::highlight::{self, HunkStart, Parsing};
use crate::diff::{self, FileDiff, Hunk, Row, RowKind};

/// Files to remember the hunk starts of. Refreshing adds to these without
/// removing any, so they're dropped wholesale at this many.
const MAX_KNOWN: usize = 512;

/// How to parse one file. Its starts are empty when there's nothing better than
/// starting afresh.
pub type Starts = Arc<Parsing>;

/// Works out hunk starts and remembers them. Shared by the render threads.
pub struct StartReader {
    repo: PathBuf,
    known: Mutex<HashMap<u128, Starts>>,
}

impl StartReader {
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            known: Mutex::default(),
        }
    }

    fn known(&self) -> MutexGuard<'_, HashMap<u128, Starts>> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How to parse `file`, or `None` if `cancelled` was set first. A file that
    /// panics gets what its name and rows say with no starts, as trying again
    /// would only repeat it.
    pub fn read(&self, file: &FileDiff, cancelled: &AtomicBool) -> Option<Starts> {
        let source = XxHash3_128::oneshot(file.raw.as_bytes());
        if let Some(starts) = self.known().get(&source) {
            return Some(starts.clone());
        }
        let read = AssertUnwindSafe(|| read(file, &self.repo, cancelled));
        let starts = match panic::catch_unwind(read) {
            Ok(None) => return None,
            Ok(Some(starts)) => Arc::new(starts),
            Err(_) => Arc::new(Parsing::of(file, &diff::rows(file))),
        };
        let mut known = self.known();
        if known.len() >= MAX_KNOWN {
            known.clear();
        }
        known.insert(source, starts.clone());
        Some(starts)
    }
}

/// Lines of a side above a hunk. A hunk with no lines on a side names the line
/// before it rather than its first, as `-U0` shows: `@@ -5,0 +6 @@` adds after 5.
pub(super) fn lines_above(start: u32, len: u32) -> usize {
    let start = start as usize;
    if len == 0 {
        start
    } else {
        start.saturating_sub(1)
    }
}

/// Whether reading `file` would read its old blob for the first line, to name
/// its language.
pub(super) fn reads_language(file: &FileDiff, rows: &[Row]) -> bool {
    highlight::wants_first_line(file, rows) && old_blob(&file.raw).is_some()
}

/// Work out how to parse `file`, running git in `repo`. `None` if `cancelled`.
fn read(file: &FileDiff, repo: &Path, cancelled: &AtomicBool) -> Option<Parsing> {
    #[cfg(test)]
    assert_ne!(file.path(), tests::PANIC_PATH, "injected");
    let rows = diff::rows(file);
    let hunks = diff::hunks(&file.raw);
    let stops: Vec<usize> = hunks
        .iter()
        .map(|hunk| lines_above(hunk.old_start, hunk.old_len))
        .collect();
    let headers = rows.iter().filter(|row| is_header(row)).count();
    let matched = hunks.len() == headers;
    // A hunk at the top of the file, as every hunk of an added or deleted file
    // is, has nothing above it to read.
    let needed = stops.iter().any(|&stop| stop > 0);
    let named = highlight::language(file, &rows);
    // One read serves both: a file its name and rows don't place reads the blob
    // for its first line, and keeps the rest for its hunk starts.
    let wanted = (named.is_some() && matched && needed) || reads_language(file, &rows);
    let blob = match old_blob(&file.raw).filter(|_| wanted) {
        Some(id) => read_blob(repo, id, cancelled),
        None => None,
    };
    if blob.is_none() && cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let first = blob.as_ref().and_then(|blob| blob.first.as_deref());
    let language = named.or_else(|| highlight::language_with(file, &rows, first));
    let no_starts = Parsing {
        language,
        starts: Vec::new(),
    };
    let Some(syntax) = language.filter(|_| matched && needed) else {
        return Some(no_starts);
    };
    let Some(text) = blob.and_then(|blob| blob.text) else {
        return Some(no_starts);
    };
    let old: Vec<&str> = text.lines().collect();
    if !shows(&old, &stops, &rows) {
        return Some(no_starts);
    }
    let (new, new_stops) = patched(&old, &stops, &hunks, &rows);

    // The sides don't depend on each other, so neither waits for the other.
    let (old, new) = thread::scope(|scope| {
        let old = scope.spawn(|| highlight::seeds(syntax, &old, &stops, cancelled));
        let new = highlight::seeds(syntax, &new, &new_stops, cancelled);
        (old.join().unwrap_or_default(), new)
    });
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let (mut old, mut new) = (old.into_iter(), new.into_iter());
    let starts = hunks.iter().map(|_| HunkStart {
        old: old.next().flatten(),
        new: new.next().flatten(),
    });
    Some(Parsing {
        language,
        starts: starts.collect(),
    })
}

fn is_header(row: &Row) -> bool {
    matches!(row.kind, RowKind::HunkHeader { .. })
}

/// Whether `old` is the file the diff was made from. Line numbers and text have to
/// agree, because the two can part ways: a `textconv` diff shows something other
/// than the blob. Spacing doesn't count, since `-w` shows context from the new
/// side on both.
pub(super) fn shows(old: &[&str], stops: &[usize], rows: &[Row]) -> bool {
    let line = |number: u32| old.get((number as usize).checked_sub(1)?);
    let agrees = |row: &Row| {
        row.old
            .is_none_or(|number| line(number).is_some_and(|line| same_code(line, &row.text)))
    };
    stops.iter().all(|&stop| stop <= old.len()) && rows.iter().all(agrees)
}

fn same_code(a: &str, b: &str) -> bool {
    let code = |text: &str| {
        text.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
    };
    a == b || code(a) == code(b)
}

/// The new side of the file, down to the last hunk, and how many of its lines
/// come before each hunk. Those counts are taken here rather than from the hunks'
/// headers, which also number any change a flag such as `-w` left out of the diff.
fn patched<'a>(
    old: &[&'a str],
    stops: &[usize],
    hunks: &[Hunk],
    rows: &'a [Row],
) -> (Vec<&'a str>, Vec<usize>) {
    let mut new = Vec::with_capacity(old.len());
    let mut new_stops = Vec::with_capacity(stops.len());
    let mut at = 0;
    // What comes before the first header is nothing at all.
    let bodies = rows.split(is_header).skip(1);
    for ((&stop, hunk), body) in stops.iter().zip(hunks).zip(bodies) {
        new.extend(old.get(at..stop).unwrap_or_default());
        new_stops.push(new.len());
        new.extend(
            body.iter()
                .filter(|row| row.new.is_some())
                .map(|row| row.text.as_str()),
        );
        at = stop + hunk.old_len as usize;
    }
    (new, new_stops)
}

/// The old side's id in an `index abc123..def456 100644` line, as abbreviated as
/// git left it, which git can still resolve. `None` if the line is missing, or
/// names the all-zero id of a file that wasn't there. A merge's `index a,b..c`
/// is `None` too, on purpose: no one blob turns into its result, so a combined
/// diff keeps its hunks starting afresh and looks for copies in its context alone.
pub(super) fn old_blob(raw: &str) -> Option<&str> {
    let (id, _) = raw
        .lines()
        .take_while(|line| !line.starts_with("@@"))
        .find_map(|line| line.strip_prefix("index "))?
        .split_once("..")?;
    let hex = id.bytes().all(|b| b.is_ascii_hexdigit());
    (hex && id.bytes().any(|b| b != b'0')).then_some(id)
}

#[cfg(test)]
pub(super) mod tests {
    use std::fs;

    use super::*;
    use crate::diff::parse;
    use crate::render::highlight::{MAX_FILE_BYTES, MAX_LINE_BYTES, Run, highlight};
    use crate::testing::Repo;
    use crate::theme::DiffTheme;

    /// A file whose read panics, to show what that does.
    pub const PANIC_PATH: &str = "panics-reading-starts.rs";

    impl Repo {
        /// The first file of `git <args>`.
        fn diff(&self, args: &[&str]) -> FileDiff {
            let out = self.run(args);
            parse(&String::from_utf8_lossy(&out.stdout)).remove(0)
        }

        fn parsing(&self, file: &FileDiff) -> Parsing {
            read(file, &self.0, &AtomicBool::new(false)).unwrap()
        }

        fn starts(&self, file: &FileDiff) -> Vec<HunkStart> {
            self.parsing(file).starts
        }
    }

    /// A block comment over lines 1 to 40, then code to line 80.
    fn commented() -> String {
        let mut text = "/*\n".to_string();
        text += &"prose\n".repeat(38);
        text += "*/\n";
        text += &"fn a() {}\n".repeat(40);
        text
    }

    /// `text` with line `at` (from 1) replaced.
    fn with_line(text: &str, at: usize, line: &str) -> String {
        let mut lines: Vec<&str> = text.lines().collect();
        lines[at - 1] = line;
        lines.join("\n") + "\n"
    }

    /// `file` highlighted from `starts`, as `(text, runs)` for each code row.
    fn colored(file: &FileDiff, starts: &[HunkStart]) -> HashMap<String, Vec<Run>> {
        let rows = diff::rows(file);
        let parsing = Parsing {
            starts: starts.to_vec(),
            ..Parsing::of(file, &rows)
        };
        let runs = highlight(
            &rows,
            DiffTheme::GitHubDark,
            &parsing,
            &AtomicBool::new(false),
        )
        .runs;
        rows.into_iter()
            .zip(runs)
            .filter(|(row, _)| !is_header(row))
            .map(|(row, runs)| (row.text, runs))
            .collect()
    }

    /// The one color of a line of comment, read off the same file parsed whole.
    fn single_color(runs: &[Run]) -> bool {
        runs.windows(2).all(|pair| pair[0].style == pair[1].style)
    }

    #[test]
    fn a_hunk_inside_a_comment_starts_in_the_comment() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let file = repo.diff(&["show", "HEAD"]);

        let fresh = colored(&file, &[]);
        assert!(!single_color(&fresh["let x = 1;"]), "parsed as code");
        let seeded = colored(&file, &repo.starts(&file));
        assert!(single_color(&seeded["let x = 1;"]), "it's in a comment");
    }

    #[test]
    fn a_merge_keeps_its_hunks_starting_afresh() {
        // Each hunk is inside a comment, but which blob would say so?
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.git(&["checkout", "-qb", "side"]);
        repo.commit("a.rs", &with_line(&commented(), 20, "let side = 1;"));
        repo.git(&["checkout", "-q", "-"]);
        repo.commit("a.rs", &with_line(&commented(), 20, "let main = 1;"));
        repo.run(&["merge", "side"]);
        repo.write("a.rs", &with_line(&commented(), 20, "let both = 1;"));
        repo.git(&["add", "."]);
        repo.git(&["commit", "-qm", "merge"]);

        let file = repo.diff(&["show", "HEAD"]);
        assert!(file.raw.starts_with("diff --cc a.rs"), "{}", file.raw);
        assert!(repo.starts(&file).is_empty());
        let fresh = colored(&file, &[]);
        assert!(!single_color(&fresh["let both = 1;"]), "parsed as code");
    }

    #[test]
    fn each_side_gets_its_own_state() {
        // The comment opens on the new side only, far above the second hunk.
        let code = "fn a() {}\n".repeat(60);
        let repo = Repo::new();
        repo.commit("a.rs", &with_line(&code, 40, "let old = 1;"));
        let changed = with_line(&with_line(&code, 2, "/*"), 40, "let new = 1;");
        repo.commit("a.rs", &changed);
        let file = repo.diff(&["show", "HEAD"]);

        let seeded = colored(&file, &repo.starts(&file));
        assert!(!single_color(&seeded["let old = 1;"]), "still code");
        assert!(single_color(&seeded["let new = 1;"]), "commented out now");
    }

    #[test]
    fn an_unstaged_diff_needs_only_the_old_blob() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.write("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let file = repo.diff(&["diff"]);
        // Nothing the file turns into afterwards can matter.
        repo.write("a.rs", "gone\n");

        let starts = repo.starts(&file);
        assert!(starts[0].old.is_some() && starts[0].new.is_some());
        assert!(single_color(&colored(&file, &starts)["let x = 1;"]));
    }

    #[test]
    fn the_new_side_follows_the_edits_of_the_hunks_above() {
        // Hunk one opens a comment and hunk two has to know, on the new side only,
        // with no new blob to read it from.
        let code = "fn a() {}\n".repeat(60);
        let repo = Repo::new();
        repo.commit("a.rs", &code);
        let mut lines: Vec<&str> = code.lines().collect();
        lines[2] = "/*";
        lines.insert(3, "added");
        lines[40] = "let x = 1;";
        repo.write("a.rs", &(lines.join("\n") + "\n"));
        let file = repo.diff(&["diff"]);

        let starts = repo.starts(&file);
        assert_eq!(starts.len(), 2);
        assert!(single_color(&colored(&file, &starts)["let x = 1;"]));
    }

    #[test]
    fn a_staged_diff_owes_nothing_to_the_work_tree() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.write("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        repo.git(&["add", "a.rs"]);
        // Unstaged, the comment is gone, which must not reach the staged view.
        repo.write("a.rs", &with_line(&commented(), 1, "// no comment"));
        let file = repo.diff(&["diff", "--staged"]);

        assert!(single_color(
            &colored(&file, &repo.starts(&file))["let x = 1;"]
        ));
    }

    #[test]
    fn a_smudged_work_tree_is_no_obstacle() {
        let repo = Repo::new();
        repo.write(".gitattributes", "*.rs filter=up\n");
        repo.git(&["config", "filter.up.clean", "tr A-Z a-z"]);
        repo.git(&["config", "filter.up.smudge", "tr a-z A-Z"]);
        repo.commit("a.rs", &commented());
        repo.write(
            "a.rs",
            &with_line(&commented(), 20, "let x = 1;").to_uppercase(),
        );
        let file = repo.diff(&["diff"]);

        assert!(single_color(
            &colored(&file, &repo.starts(&file))["let x = 1;"]
        ));
    }

    #[test]
    fn a_blob_that_is_not_the_text_shown_is_not_believed() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        // What a `textconv` diff looks like: real ids, some other text.
        let file = repo.diff(&["show", "HEAD"]);
        let raw = file.raw.replace("prose", "converted");
        let file = parse(&raw).remove(0);

        assert!(repo.starts(&file).is_empty());
    }

    #[test]
    fn spacing_differences_from_ignoring_whitespace_are_allowed() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        let spaced = commented().replace("prose", "  prose");
        repo.commit("a.rs", &with_line(&spaced, 20, "let x = 1;"));
        let file = repo.diff(&["show", "-w", "HEAD"]);

        let starts = repo.starts(&file);
        assert!(starts[0].old.is_some() && starts[0].new.is_some());
    }

    #[test]
    fn a_hunk_with_nothing_on_a_side_counts_from_the_line_before_it() {
        assert_eq!(lines_above(5, 0), 5);
        assert_eq!(lines_above(6, 2), 5);
        assert_eq!(lines_above(1, 3), 0);
        assert_eq!(lines_above(0, 0), 0);

        let repo = Repo::new();
        let text = commented();
        repo.commit("a.rs", &text);
        let mut lines: Vec<&str> = text.lines().collect();
        lines.insert(20, "let x = 1;");
        repo.commit("a.rs", &(lines.join("\n") + "\n"));
        let file = repo.diff(&["show", "-U0", "HEAD"]);

        assert!(file.raw.contains("@@ -20,0 +21 @@"), "{}", file.raw);
        assert!(single_color(
            &colored(&file, &repo.starts(&file))["let x = 1;"]
        ));
    }

    #[test]
    fn added_and_deleted_files_have_nothing_above_their_hunks() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        assert!(repo.starts(&repo.diff(&["show", "HEAD"])).is_empty());
        fs::remove_file(repo.0.join("a.rs")).unwrap();
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-qm", "x"]);
        assert!(repo.starts(&repo.diff(&["show", "HEAD"])).is_empty());
    }

    #[test]
    fn a_rename_reads_blobs_by_id_whatever_the_paths() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.git(&["mv", "a.rs", "b.rs"]);
        repo.commit("b.rs", &with_line(&commented(), 20, "let x = 1;"));
        let file = repo.diff(&["show", "-M", "HEAD"]);

        assert_eq!(file.old_path.as_deref(), Some("a.rs"));
        let starts = repo.starts(&file);
        assert!(starts[0].old.is_some() && starts[0].new.is_some());
    }

    #[test]
    fn a_missing_or_unknown_id_keeps_todays_behaviour() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let raw = repo.diff(&["show", "HEAD"]).raw;
        let index = raw.lines().find(|l| l.starts_with("index ")).unwrap();

        let without = parse(&raw.replace(&format!("{index}\n"), "")).remove(0);
        assert!(repo.starts(&without).is_empty());
        let unknown = parse(&raw.replace(index, "index dead00..beef00 100644")).remove(0);
        assert!(repo.starts(&unknown).is_empty());
    }

    #[test]
    fn the_id_is_read_off_the_header_only() {
        assert_eq!(
            old_blob("diff --git a/a b/a\nindex 12ab..34cd 100644\n--- a/a\n"),
            Some("12ab")
        );
        assert_eq!(old_blob("diff --git a/a b/a\nindex 0000000..34cd\n"), None);
        assert_eq!(old_blob("diff --cc a\nindex 12ab,56ef..34cd\n"), None);
        assert_eq!(
            old_blob("diff --git a/a b/a\n@@ -1 +1 @@\nindex 12ab..34cd\n"),
            None
        );
    }

    #[test]
    fn a_blob_past_the_size_limit_is_not_read() {
        let repo = Repo::new();
        let text = "x\n".repeat(MAX_FILE_BYTES / 2 + 1);
        repo.commit("a.rs", &text);
        let out = repo.run(&["rev-parse", "HEAD:a.rs"]);
        let id = String::from_utf8(out.stdout).unwrap();
        let blob = read_blob(&repo.0, id.trim(), &AtomicBool::new(false)).unwrap();
        assert_eq!(blob.text, None);
        assert_eq!(blob.first.as_deref(), Some("x"), "but its first line is");
    }

    /// A Python script with no extension: a docstring over lines 2 to 40, then
    /// code to line 80.
    fn script(first: &str) -> String {
        let mut text = format!("{first}\n\"\"\"\n");
        text += &"prose\n".repeat(37);
        text += "\"\"\"\n";
        text += &"x = 1\n".repeat(40);
        text
    }

    fn language(parsing: &Parsing) -> Option<&str> {
        parsing.language.map(|syntax| syntax.name.as_str())
    }

    #[test]
    fn a_script_changed_below_its_first_line_is_named_by_the_blob() {
        let repo = Repo::new();
        repo.commit("bin/deploy", &script("#!/usr/bin/env -S python3 -u"));
        repo.commit(
            "bin/deploy",
            &with_line(&script("#!/usr/bin/env -S python3 -u"), 20, "y = 2"),
        );
        let file = repo.diff(&["show", "HEAD"]);
        assert!(highlight::language(&file, &diff::rows(&file)).is_none());

        let parsing = repo.parsing(&file);
        assert_eq!(language(&parsing), Some("Python"));
        // The same read gives the hunk starts, so the change is inside the docstring.
        assert!(single_color(&colored(&file, &parsing.starts)["y = 2"]));
    }

    #[test]
    fn each_shell_is_named_by_the_blob() {
        for first in ["#!/bin/sh", "#!/bin/bash", "#!/usr/bin/env zsh"] {
            let repo = Repo::new();
            repo.commit("run", &script(first));
            repo.commit("run", &with_line(&script(first), 60, "y=2"));
            let file = repo.diff(&["show", "HEAD"]);
            assert!(repo.parsing(&file).language.is_some(), "{first}");
        }
    }

    #[test]
    fn a_first_line_the_diff_shows_wins_over_the_blob() {
        let repo = Repo::new();
        repo.commit("run", &script("#!/bin/sh"));
        repo.commit(
            "run",
            &with_line(&script("#!/bin/sh"), 1, "#!/usr/bin/env ruby"),
        );
        let file = repo.diff(&["show", "HEAD"]);
        let parsing = repo.parsing(&file);
        // The new side's first line is the file as it is now.
        assert_eq!(language(&parsing), Some("Ruby"));
        assert!(!reads_language(&file, &diff::rows(&file)));
    }

    #[test]
    fn a_name_that_means_something_needs_no_blob_for_its_language() {
        let repo = Repo::new();
        repo.commit("notes.txt", &script("#!/bin/sh"));
        repo.commit("notes.txt", &with_line(&script("#!/bin/sh"), 60, "y=2"));
        let file = repo.diff(&["show", "HEAD"]);
        assert!(
            !reads_language(&file, &diff::rows(&file)),
            "plain text, by name"
        );
        assert_eq!(language(&repo.parsing(&file)), None);

        repo.git(&["mv", "notes.txt", "tool.rb"]);
        repo.commit("tool.rb", &with_line(&script("#!/bin/sh"), 60, "y=3"));
        let file = repo.diff(&["show", "-M", "HEAD"]);
        assert_eq!(file.path(), "tool.rb");
        assert!(!reads_language(&file, &diff::rows(&file)));
        assert_eq!(language(&repo.parsing(&file)), Some("Ruby"));
    }

    #[test]
    fn an_added_file_shows_its_own_first_line() {
        let repo = Repo::new();
        repo.commit("other", "x\n");
        repo.commit("run", &script("#!/bin/sh"));
        let file = repo.diff(&["show", "HEAD"]);
        assert!(!reads_language(&file, &diff::rows(&file)));
        assert!(repo.parsing(&file).language.is_some());
    }

    #[test]
    fn a_blob_too_big_to_read_still_gives_its_first_line() {
        let repo = Repo::new();
        let text = script("#!/usr/bin/env python3") + &"x = 1\n".repeat(MAX_FILE_BYTES / 6);
        repo.commit("tool", &text);
        repo.commit("tool", &with_line(&text, 60, "y = 2"));
        let file = repo.diff(&["show", "HEAD"]);
        let parsing = repo.parsing(&file);
        assert_eq!(language(&parsing), Some("Python"));
        assert!(parsing.starts.is_empty(), "nothing to work them out from");
    }

    #[test]
    fn a_binary_or_overlong_or_garbled_first_line_names_nothing_it_should_not() {
        let long = format!("#!/bin/sh {}", "x".repeat(MAX_LINE_BYTES));
        let garbled = "#!/usr/bin/env python3 \u{fffd}";
        for (first, expected) in [
            ("#!/bin/sh\0", None),
            (long.as_str(), None),
            (garbled, Some("Python")),
            ("# -*- mode: ruby -*-", Some("Ruby")),
        ] {
            let repo = Repo::new();
            repo.commit("run", &script(first));
            repo.commit("run", &with_line(&script(first), 60, "y = 2"));
            let file = repo.diff(&["show", "--text", "HEAD"]);
            assert_eq!(language(&repo.parsing(&file)), expected, "{first:.20}");
        }
    }

    #[test]
    fn a_first_line_that_is_not_utf8_is_read_lossily() {
        let repo = Repo::new();
        let mut bytes = b"#!/usr/bin/env python3 \xff\n".to_vec();
        bytes.extend(
            script("#")
                .lines()
                .skip(1)
                .flat_map(|l| [l.as_bytes(), b"\n"].concat()),
        );
        fs::write(repo.0.join("run"), &bytes).unwrap();
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-qm", "x"]);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        repo.commit("run", &with_line(&text, 60, "y = 2"));
        let file = repo.diff(&["show", "HEAD"]);
        assert_eq!(language(&repo.parsing(&file)), Some("Python"));
    }

    #[test]
    fn a_merge_has_no_blob_to_name_it() {
        let repo = Repo::new();
        let text = script("#!/bin/sh");
        repo.commit("run", &text);
        repo.git(&["checkout", "-qb", "side"]);
        repo.commit("run", &with_line(&text, 60, "side=1"));
        repo.git(&["checkout", "-q", "-"]);
        repo.commit("run", &with_line(&text, 60, "main=1"));
        repo.run(&["merge", "side"]);
        repo.write("run", &with_line(&text, 60, "both=1"));
        repo.git(&["add", "."]);
        repo.git(&["commit", "-qm", "merge"]);

        let file = repo.diff(&["show", "HEAD"]);
        assert!(file.raw.starts_with("diff --cc run"), "{}", file.raw);
        assert!(!reads_language(&file, &diff::rows(&file)));
        assert_eq!(language(&repo.parsing(&file)), None);
    }

    /// Work out `file`'s starts through `reader`.
    fn answer(reader: &StartReader, file: &FileDiff) -> Starts {
        reader.read(file, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn an_answer_is_read_once_and_remembered() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let file = repo.diff(&["show", "HEAD"]);

        let reader = StartReader::new(repo.0.to_path_buf());
        assert_eq!(answer(&reader, &file).starts.len(), 1);
        // With the repository gone, only a remembered answer can come back.
        fs::remove_dir_all(repo.0.join(".git")).unwrap();
        assert_eq!(answer(&reader, &file).starts.len(), 1);
    }

    #[test]
    fn a_cancelled_read_has_no_answer_and_leaves_none_behind() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let file = repo.diff(&["show", "HEAD"]);

        let reader = StartReader::new(repo.0.to_path_buf());
        assert!(reader.read(&file, &AtomicBool::new(true)).is_none());
        assert_eq!(
            answer(&reader, &file).starts.len(),
            1,
            "it can be asked again"
        );
    }

    #[test]
    fn a_read_that_panics_leaves_the_file_starting_afresh() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let mut file = repo.diff(&["show", "HEAD"]);
        file.new_path = Some(PANIC_PATH.to_string());

        let reader = StartReader::new(repo.0.to_path_buf());
        assert!(answer(&reader, &file).starts.is_empty());
        // Remembered, so the panic isn't there to be hit again.
        fs::remove_dir_all(repo.0.join(".git")).unwrap();
        assert!(answer(&reader, &file).starts.is_empty());
    }
}

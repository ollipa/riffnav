//! Where each hunk's parser really starts.
//!
//! A hunk can begin inside a block comment or a multi-line string, and only the
//! lines above it say so. When riffnav ran git itself those lines can be had, and
//! one blob is enough: the `index` line names the old side, and the hunks turn
//! that into the new side as far down as the last of them. That holds whichever
//! view the diff is of, so the work tree is never read: its side rarely has a blob
//! behind it, but it can change at any moment, and filters leave it in a form the
//! diff doesn't show.
//!
//! Parsing is the expensive part, at roughly 20 lines of Rust a millisecond, so a
//! 3,500-line file costs 200ms and a file at the size limit most of a second. It
//! happens as the first step of rendering the file, off the UI thread, so the
//! render that comes out of it is the right one and there's no second to make.
//!
//! The answer is kept by the hash of the diff text, which names the blob and every
//! hunk, so resizing or changing theme costs nothing. A file the diff can't be
//! matched to, for any reason, keeps its hunks starting afresh.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use twox_hash::XxHash3_128;

use super::highlight::{self, HunkStart, MAX_FILE_BYTES};
use crate::autodiff::Tree;
use crate::diff::{self, FileDiff, Hunk, Row, RowKind};

/// How often a running git is checked on, to stop it if the job was cancelled.
const GIT_POLL: Duration = Duration::from_millis(2);
/// Files to remember the hunk starts of. Refreshing adds to these without
/// removing any, so they're dropped wholesale at this many.
const MAX_KNOWN: usize = 512;

/// One file's hunk starts. Empty when there's nothing better than starting afresh.
pub type Starts = Arc<Vec<HunkStart>>;

/// Works out hunk starts and remembers them. Shared by the render threads.
pub struct StartReader {
    repo: PathBuf,
    known: Mutex<HashMap<u128, Starts>>,
}

impl StartReader {
    /// A reader running git in `repo`.
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            known: Mutex::default(),
        }
    }

    fn known(&self) -> MutexGuard<'_, HashMap<u128, Starts>> {
        self.known.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The starts of `file`, or `None` if `cancelled` was set first. A file that
    /// panics gets none, as trying again would only repeat it, and renders without.
    pub fn read(&self, file: &FileDiff, cancelled: &AtomicBool) -> Option<Starts> {
        let source = XxHash3_128::oneshot(file.raw.as_bytes());
        if let Some(starts) = self.known().get(&source) {
            return Some(starts.clone());
        }
        let read = AssertUnwindSafe(|| read(file, &self.repo, cancelled));
        let starts = match panic::catch_unwind(read) {
            Ok(None) => return None,
            Ok(Some(starts)) => Arc::new(starts),
            Err(_) => Starts::default(),
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

/// Work out `file`'s hunk starts, running git in `repo`. `None` if `cancelled`.
fn read(file: &FileDiff, repo: &Path, cancelled: &AtomicBool) -> Option<Vec<HunkStart>> {
    #[cfg(test)]
    assert_ne!(file.path(), tests::PANIC_PATH, "injected");
    let rows = diff::rows(file);
    let hunks = diff::hunks(&file.raw);
    let stops: Vec<usize> = hunks
        .iter()
        .map(|hunk| lines_above(hunk.old_start, hunk.old_len))
        .collect();
    let headers = rows.iter().filter(|row| is_header(row)).count();
    let Some(syntax) = highlight::language(file, &rows).filter(|_| hunks.len() == headers) else {
        return Some(Vec::new());
    };
    // A hunk at the top of the file, as every hunk of an added or deleted file
    // is, has nothing above it to read.
    let needed = stops.iter().any(|&stop| stop > 0);
    let blob = old_blob(&file.raw).filter(|_| needed);
    let Some(text) = blob.and_then(|id| read_blob(repo, id, cancelled)) else {
        return (!cancelled.load(Ordering::Relaxed)).then(Vec::new);
    };
    let old: Vec<&str> = text.lines().collect();
    if !shows(&old, &stops, &rows) {
        return Some(Vec::new());
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
    Some(starts.collect())
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

/// A blob's text, if git has it and it isn't over the size limit. A cancelled job
/// doesn't wait for git, and neither does quitting, so nothing's left running.
fn read_blob(repo: &Path, id: &str, cancelled: &AtomicBool) -> Option<String> {
    let mut git = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(repo)
        // A partial clone would otherwise go to the network for a missing blob.
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let (stdin, stdout) = (git.stdin.take(), git.stdout.take());
    let text = thread::scope(|scope| {
        let reading = scope.spawn(|| blob_text(stdin?, stdout?, id));
        watch_git(&mut git, cancelled, || reading.is_finished());
        reading.join().ok().flatten()
    });
    // Stops git writing out a blob that turned out to be too big.
    let _ = git.kill();
    let _ = git.wait();
    text
}

/// Wait for `done`, stopping `git` if `cancelled` gets set first.
fn watch_git(git: &mut Child, cancelled: &AtomicBool, done: impl Fn() -> bool) {
    while !done() {
        if cancelled.load(Ordering::Relaxed) {
            let _ = git.kill();
            return;
        }
        thread::sleep(GIT_POLL);
    }
}

/// What each of `specs` names, in order, from one git. Each blob's size comes off
/// `budget`, and once that's used up the rest come back as `None`, as does anything
/// over the size limit, missing, or binary. `None` if `cancelled` was set first.
pub(super) fn read_blobs(
    repo: &Path,
    specs: &[String],
    budget: &mut usize,
    cancelled: &AtomicBool,
) -> Option<Vec<Option<String>>> {
    if specs.is_empty() {
        return Some(Vec::new());
    }
    let mut git = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(repo)
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let (stdin, stdout) = (git.stdin.take(), git.stdout.take());
    let texts = thread::scope(|scope| {
        scope.spawn(|| {
            let mut stdin = stdin?;
            specs
                .iter()
                .try_for_each(|spec| writeln!(stdin, "{spec}"))
                .ok()
        });
        let reading = scope.spawn(|| blob_texts(stdout?, specs.len(), budget));
        watch_git(&mut git, cancelled, || reading.is_finished());
        reading.join().ok().flatten()
    });
    let _ = git.kill();
    let _ = git.wait();
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let mut texts = texts.unwrap_or_default();
    texts.resize(specs.len(), None);
    Some(texts)
}

/// Read `count` answers from `git cat-file --batch`, stopping short at the budget.
fn blob_texts(
    stdout: ChildStdout,
    count: usize,
    budget: &mut usize,
) -> Option<Vec<Option<String>>> {
    let mut out = BufReader::new(stdout);
    let mut texts = Vec::with_capacity(count);
    for _ in 0..count {
        let mut header = String::new();
        if out.read_line(&mut header).ok()? == 0 {
            break;
        }
        // `<id> blob <size>`, or `<spec> missing`, where a spec can have spaces in.
        let mut fields = header.split_whitespace().rev();
        let (size, kind) = (fields.next()?, fields.next());
        let Ok(size) = size.parse::<usize>() else {
            texts.push(None);
            continue;
        };
        if size > *budget {
            break;
        }
        // The newline that ends every answer comes with the body.
        let mut bytes = Vec::with_capacity(size + 1);
        (&mut out)
            .take(size as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        bytes.truncate(size);
        let binary = bytes.iter().take(8000).any(|&byte| byte == 0);
        if kind == Some("blob") && size <= MAX_FILE_BYTES && !binary {
            *budget -= size;
            texts.push(Some(String::from_utf8_lossy(&bytes).into_owned()));
        } else {
            texts.push(None);
        }
    }
    Some(texts)
}

/// A line `git grep` found.
pub(super) struct GrepHit {
    pub path: String,
    pub text: String,
}

/// Search `tree` for any of `patterns`, as fixed strings, in the files `pathspecs`
/// allow. `limits` are how long it gets and how much output, and what it had found
/// by then is the answer. `None` if `cancelled` was set first.
pub(super) fn grep(
    repo: &Path,
    tree: &Tree,
    patterns: &[String],
    pathspecs: &[String],
    limits: (Duration, u64),
    cancelled: &AtomicBool,
) -> Option<Vec<GrepHit>> {
    let mut command = Command::new("git");
    command
        .args(["grep", "--no-color", "--no-column", "-F", "-I", "-n", "-z"])
        .args(["--full-name", "-f", "-"]);
    match tree {
        Tree::Index => command.arg("--cached"),
        Tree::Rev(rev) => command.arg(rev),
    };
    let mut git = command
        .arg("--")
        .args(pathspecs)
        .current_dir(repo)
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let (stdin, stdout) = (git.stdin.take(), git.stdout.take());
    let (limit, max_bytes) = limits;
    let deadline = Instant::now() + limit;
    let output = thread::scope(|scope| {
        scope.spawn(|| {
            let mut stdin = stdin?;
            patterns
                .iter()
                .try_for_each(|line| writeln!(stdin, "{line}"))
                .ok()
        });
        let reading = scope.spawn(|| {
            let mut bytes = Vec::new();
            stdout?.take(max_bytes).read_to_end(&mut bytes).ok()?;
            Some(bytes)
        });
        watch_git(&mut git, cancelled, || {
            reading.is_finished() || Instant::now() >= deadline
        });
        // Ends the read, which keeps what it has.
        let _ = git.kill();
        reading.join().ok().flatten()
    });
    let _ = git.kill();
    let _ = git.wait();
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let output = output.unwrap_or_default();
    let prefix = match tree {
        Tree::Index => String::new(),
        Tree::Rev(rev) => format!("{rev}:"),
    };
    let mut hits = Vec::new();
    for line in String::from_utf8_lossy(&output).lines() {
        let mut fields = line.splitn(3, '\0');
        if let (Some(path), Some(_), Some(text)) = (fields.next(), fields.next(), fields.next()) {
            let path = path.strip_prefix(&prefix).unwrap_or(path);
            hits.push(GrepHit {
                path: path.to_string(),
                text: text.to_string(),
            });
        }
    }
    Some(hits)
}

fn blob_text(mut stdin: ChildStdin, stdout: ChildStdout, id: &str) -> Option<String> {
    writeln!(stdin, "{id}").ok()?;
    drop(stdin);
    let mut out = BufReader::new(stdout);
    let mut header = String::new();
    out.read_line(&mut header).ok()?;
    // `<id> blob <size>`, or `<id> missing` or `<id> ambiguous`.
    let mut fields = header.split_whitespace().skip(1);
    let size: usize = (fields.next()? == "blob")
        .then(|| fields.next()?.parse().ok())
        .flatten()?;
    if size > MAX_FILE_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(size);
    out.take(size as u64).read_to_end(&mut bytes).ok()?;
    (bytes.len() == size).then(|| String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
pub(super) mod tests {
    use std::fs;
    use std::process::Output;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::diff::parse;
    use crate::render::highlight::{Run, highlight};
    use crate::theme::DiffTheme;

    /// A file whose read panics, to show what that does.
    pub const PANIC_PATH: &str = "panics-reading-starts.rs";

    /// A throwaway repository.
    pub struct Repo(pub PathBuf);

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    impl Repo {
        pub fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("riffnav-starts-{}-{id}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let repo = Self(dir);
            repo.git(&["init", "-q"]);
            repo
        }

        pub fn run(&self, args: &[&str]) -> Output {
            Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(&self.0)
                .output()
                .unwrap()
        }

        pub fn git(&self, args: &[&str]) {
            let out = self.run(args);
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }

        pub fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        pub fn commit(&self, path: &str, text: &str) {
            self.write(path, text);
            self.git(&["add", "-A"]);
            self.git(&["commit", "-qm", "x"]);
        }

        /// The first file of `git <args>`.
        fn diff(&self, args: &[&str]) -> FileDiff {
            let out = self.run(args);
            parse(&String::from_utf8_lossy(&out.stdout)).remove(0)
        }

        fn starts(&self, file: &FileDiff) -> Vec<HunkStart> {
            read(file, &self.0, &AtomicBool::new(false)).unwrap()
        }
    }

    fn read_all(repo: &Repo, specs: &[&str], budget: &mut usize) -> Vec<Option<String>> {
        let specs: Vec<String> = specs.iter().map(|spec| spec.to_string()).collect();
        read_blobs(&repo.0, &specs, budget, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn reading_blobs_gives_an_answer_for_every_spec() {
        let repo = Repo::new();
        repo.write("binary", "abc\0def");
        repo.commit("a.rs", "first\n");
        let got = read_all(
            &repo,
            &[
                "HEAD:a.rs",
                "HEAD:gone.rs",
                "HEAD:binary",
                "HEAD",
                "HEAD:a.rs",
            ],
            &mut 100_000,
        );
        let text = Some("first\n".to_string());
        assert_eq!(got, [text.clone(), None, None, None, text]);
    }

    #[test]
    fn reading_blobs_stops_at_the_budget() {
        let repo = Repo::new();
        repo.write("b.rs", "second\n");
        repo.commit("a.rs", "first\n");
        let mut budget = 10;
        let got = read_all(&repo, &["HEAD:a.rs", "HEAD:b.rs", "HEAD:a.rs"], &mut budget);
        assert_eq!(got, [Some("first\n".to_string()), None, None]);
        assert_eq!(budget, 4);
    }

    #[test]
    fn reading_blobs_skips_one_over_the_size_limit() {
        let repo = Repo::new();
        repo.write("big.rs", &"x".repeat(MAX_FILE_BYTES + 1));
        repo.commit("a.rs", "first\n");
        let got = read_all(&repo, &["HEAD:big.rs", "HEAD:a.rs"], &mut (64 << 20));
        assert_eq!(got, [None, Some("first\n".to_string())]);
    }

    #[test]
    fn a_grep_out_of_time_keeps_what_it_has_and_a_cancelled_one_nothing() {
        let repo = Repo::new();
        repo.commit("a.rs", "needle\n");
        let tree = Tree::Rev("HEAD".to_string());
        let patterns = ["needle".to_string()];
        let grep = |limits, cancelled: bool| {
            grep(
                &repo.0,
                &tree,
                &patterns,
                &[],
                limits,
                &AtomicBool::new(cancelled),
            )
        };
        let hits = grep((Duration::from_secs(30), 1 << 20), false).unwrap();
        assert_eq!(hits[0].path, "a.rs");
        assert_eq!(hits[0].text, "needle");
        assert!(grep((Duration::ZERO, 1 << 20), false).is_some());
        assert!(grep((Duration::from_secs(30), 1 << 20), true).is_none());
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
        let runs = highlight(
            file,
            &rows,
            DiffTheme::GitHubDark,
            starts,
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
        assert_eq!(read_blob(&repo.0, id.trim(), &AtomicBool::new(false)), None);
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

        let reader = StartReader::new(&repo.0);
        assert_eq!(answer(&reader, &file).len(), 1);
        // With the repository gone, only a remembered answer can come back.
        fs::remove_dir_all(repo.0.join(".git")).unwrap();
        assert_eq!(answer(&reader, &file).len(), 1);
    }

    #[test]
    fn a_cancelled_read_has_no_answer_and_leaves_none_behind() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let file = repo.diff(&["show", "HEAD"]);

        let reader = StartReader::new(&repo.0);
        assert!(reader.read(&file, &AtomicBool::new(true)).is_none());
        assert_eq!(answer(&reader, &file).len(), 1, "it can be asked again");
    }

    #[test]
    fn a_cancelled_job_stops_a_git_that_has_gone_quiet() {
        let mut git = Command::new("sleep").arg("30").spawn().unwrap();
        let cancelled = AtomicBool::new(true);
        watch_git(&mut git, &cancelled, || false);
        assert!(!git.wait().unwrap().success(), "killed, not left to finish");
    }

    #[test]
    fn a_read_that_panics_leaves_the_file_starting_afresh() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let mut file = repo.diff(&["show", "HEAD"]);
        file.new_path = Some(PANIC_PATH.to_string());

        let reader = StartReader::new(&repo.0);
        assert!(answer(&reader, &file).is_empty());
        // Remembered, so the panic isn't there to be hit again.
        fs::remove_dir_all(repo.0.join(".git")).unwrap();
        assert!(answer(&reader, &file).is_empty());
    }
}

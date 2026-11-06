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
//! 3,500-line file costs 200ms and a file at the size limit most of a second.
//! That's far too long for the UI thread, which only ever waits [`WAIT`]: the
//! work happens on a thread of its own, and a file that takes longer is drawn with
//! its hunks starting afresh until the answer comes in.
//!
//! The answer is kept by the hash of the diff text, which names the blob and every
//! hunk, so resizing or changing theme costs nothing. A file the diff can't be
//! matched to, for any reason, keeps its hunks starting afresh.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use super::highlight::{self, HunkStart, MAX_FILE_BYTES};
use crate::diff::{self, FileDiff, Hunk, Row, RowKind};

/// How long a first render waits for its hunk starts. Long enough that an ordinary
/// file is drawn once and correctly, and short enough not to be felt.
const WAIT: Duration = Duration::from_millis(25);
/// Files to remember the hunk starts of. Refreshing adds to these without
/// removing any, so they're dropped wholesale at this many.
const MAX_KNOWN: usize = 512;

/// One file's hunk starts. Empty when there's nothing better than starting afresh.
pub type Starts = Arc<Vec<HunkStart>>;

/// A file being worked on.
struct Job {
    source: u128,
    cancelled: Arc<AtomicBool>,
    /// Closes without an answer if the job was cancelled or panicked.
    result: Receiver<Starts>,
}

/// Works out hunk starts in the background, one file at a time, and remembers
/// them. Only the file on screen matters, so asking about another abandons the
/// one in progress.
pub struct StartReader {
    repo: PathBuf,
    known: HashMap<u128, Starts>,
    job: Option<Job>,
    /// What to work on when the cancelled job has stopped.
    next: Option<(u128, FileDiff)>,
}

impl StartReader {
    /// A reader running git in `repo`.
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Self {
            repo: repo.into(),
            known: HashMap::new(),
            job: None,
            next: None,
        }
    }

    /// Whether a file is being worked on.
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// The starts of `file`, known as `source`, or `None` while they're being
    /// worked out, which this sets going if nothing else has.
    pub fn ask(&mut self, source: u128, file: &FileDiff) -> Option<Starts> {
        if let Some(starts) = self.known.get(&source) {
            return Some(starts.clone());
        }
        match &self.job {
            None => self.start(source, file.clone()),
            Some(job) if job.source == source && !job.cancelled.load(Ordering::Relaxed) => {}
            Some(job) => {
                job.cancelled.store(true, Ordering::Relaxed);
                if self.next.as_ref().is_none_or(|(next, _)| *next != source) {
                    self.next = Some((source, file.clone()));
                }
            }
        }
        None
    }

    /// As [`Self::ask`], but give the answer up to [`WAIT`] to arrive.
    pub fn wait_for(&mut self, source: u128, file: &FileDiff) -> Option<Starts> {
        let deadline = Instant::now() + WAIT;
        loop {
            let starts = self.ask(source, file);
            let Some(job) = self.job.as_ref().filter(|_| starts.is_none()) else {
                return starts;
            };
            let left = deadline.saturating_duration_since(Instant::now());
            match job.result.recv_timeout(left) {
                Ok(starts) => self.finish(Some(starts)),
                Err(RecvTimeoutError::Disconnected) => self.finish(None),
                Err(RecvTimeoutError::Timeout) => return None,
            }
        }
    }

    /// Collect the running job's answer if it has one.
    pub fn drain(&mut self) {
        if let Some(job) = &self.job {
            match job.result.try_recv() {
                Ok(starts) => self.finish(Some(starts)),
                Err(TryRecvError::Disconnected) => self.finish(None),
                Err(TryRecvError::Empty) => {}
            }
        }
    }

    fn start(&mut self, source: u128, file: FileDiff) {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, result) = mpsc::channel();
        let (repo, flag) = (self.repo.clone(), cancelled.clone());
        thread::spawn(move || {
            if let Some(starts) = read(&file, &repo, &flag) {
                let _ = sender.send(Arc::new(starts));
            }
        });
        self.job = Some(Job {
            source,
            cancelled,
            result,
        });
    }

    /// Close the running job with its answer, and start what's waiting.
    fn finish(&mut self, starts: Option<Starts>) {
        let Some(job) = self.job.take() else {
            return;
        };
        // No answer and not cancelled means it panicked, which trying again
        // would only repeat.
        let dead = !job.cancelled.load(Ordering::Relaxed);
        if let Some(starts) = starts.or_else(|| dead.then(Starts::default)) {
            if self.known.len() >= MAX_KNOWN {
                self.known.clear();
            }
            self.known.insert(job.source, starts);
        }
        if let Some((source, file)) = self.next.take() {
            self.start(source, file);
        }
    }
}

/// Lines of a side above a hunk. A hunk with no lines on a side names the line
/// before it rather than its first, as `-U0` shows: `@@ -5,0 +6 @@` adds after 5.
fn lines_above(start: u32, len: u32) -> usize {
    let start = start as usize;
    if len == 0 {
        start
    } else {
        start.saturating_sub(1)
    }
}

/// Work out `file`'s hunk starts, running git in `repo`. `None` if `cancelled`.
fn read(file: &FileDiff, repo: &Path, cancelled: &AtomicBool) -> Option<Vec<HunkStart>> {
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
    let Some(text) = blob.and_then(|id| read_blob(repo, id)) else {
        return Some(Vec::new());
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
fn shows(old: &[&str], stops: &[usize], rows: &[Row]) -> bool {
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
/// names the all-zero id of a file that wasn't there.
fn old_blob(raw: &str) -> Option<&str> {
    let (id, _) = raw
        .lines()
        .take_while(|line| !line.starts_with("@@"))
        .find_map(|line| line.strip_prefix("index "))?
        .split_once("..")?;
    let hex = id.bytes().all(|b| b.is_ascii_hexdigit());
    (hex && id.bytes().any(|b| b != b'0')).then_some(id)
}

/// A blob's text, if git has it and it isn't over the size limit.
fn read_blob(repo: &Path, id: &str) -> Option<String> {
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
    let text = blob_text(&mut git, id);
    // Stops git writing out a blob that turned out to be too big.
    let _ = git.kill();
    let _ = git.wait();
    text
}

fn blob_text(git: &mut std::process::Child, id: &str) -> Option<String> {
    writeln!(git.stdin.take()?, "{id}").ok()?;
    let mut out = BufReader::new(git.stdout.take()?);
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
mod tests {
    use std::fs;
    use std::process::Output;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::diff::parse;
    use crate::render::highlight::{Run, highlight};
    use crate::theme::DiffTheme;

    /// A throwaway repository.
    struct Repo(PathBuf);

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    impl Repo {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("riffnav-starts-{}-{id}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let repo = Self(dir);
            repo.git(&["init", "-q"]);
            repo
        }

        fn run(&self, args: &[&str]) -> Output {
            Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(&self.0)
                .output()
                .unwrap()
        }

        fn git(&self, args: &[&str]) {
            let out = self.run(args);
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }

        fn write(&self, path: &str, text: &str) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn commit(&self, path: &str, text: &str) {
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
        let runs = highlight(file, &rows, DiffTheme::GitHubDark, starts).runs;
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
        assert_eq!(read_blob(&repo.0, id.trim()), None);
    }

    /// Ask until there's an answer.
    fn answer(reader: &mut StartReader, source: u128, file: &FileDiff) -> Starts {
        for _ in 0..1000 {
            if let Some(starts) = reader.wait_for(source, file) {
                return starts;
            }
        }
        panic!("no answer");
    }

    #[test]
    fn an_answer_is_read_once_and_remembered() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let file = repo.diff(&["show", "HEAD"]);

        let mut reader = StartReader::new(&repo.0);
        assert_eq!(answer(&mut reader, 1, &file).len(), 1);
        assert!(!reader.busy());
        // With the repository gone, only a remembered answer can come back.
        fs::remove_dir_all(repo.0.join(".git")).unwrap();
        assert_eq!(reader.ask(1, &file).unwrap().len(), 1);
        assert!(!reader.busy());
    }

    #[test]
    fn asking_about_another_file_abandons_the_first() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let file = repo.diff(&["show", "HEAD"]);

        let mut reader = StartReader::new(&repo.0);
        assert!(reader.ask(1, &file).is_none());
        assert!(reader.ask(2, &file).is_none());
        assert!(reader.ask(3, &file).is_none());
        assert_eq!(answer(&mut reader, 3, &file).len(), 1);
        assert!(!reader.busy(), "the job for 2 was replaced before it began");
    }

    #[test]
    fn returning_to_an_abandoned_file_asks_again() {
        let repo = Repo::new();
        repo.commit("a.rs", &commented());
        repo.commit("a.rs", &with_line(&commented(), 20, "let x = 1;"));
        let file = repo.diff(&["show", "HEAD"]);

        let mut reader = StartReader::new(&repo.0);
        assert!(reader.ask(1, &file).is_none());
        assert!(reader.ask(2, &file).is_none());
        assert_eq!(answer(&mut reader, 1, &file).len(), 1);
    }
}

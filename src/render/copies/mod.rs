//! Copied-code detection: which added blocks duplicate code that's still there.
//!
//! git shows a pasted function as plain new code, so a reviewer reads it without
//! knowing the same thing lives somewhere else, and that a fix to one now needs the
//! other. Unlike a move this is worth a harder look, so a copy keeps its added
//! colors and only gets a note. Being wrong costs more here than missing one: a note
//! on every `use` block would teach the reader to skip them all, so everything below
//! errs towards saying nothing.
//!
//! What counts as the source, nearest first:
//!
//! 1. Context rows of the diff, which is all a diff piped in has.
//! 2. The unchanged lines of the files in the diff, from their old blobs.
//! 3. Files the diff doesn't touch. A few of the rarest added lines go to one `git
//!    grep`, which is the only pass over the repository, and it's stopped after
//!    [`GREP_LIMIT`]. Only the files it names are read. It's skipped when the tree
//!    the diff was made from isn't known, as a line would be taken from the wrong
//!    version.
//!
//! Lines removed by the diff are never a source. Code that left and came back is a
//! move, and code that left and came back twice has no original left to point at.
//!
//! This runs on a thread of its own, as reading blobs takes as long as it takes. See
//! [`super::RenderCache::start_copies`].

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use twox_hash::XxHash3_128;

use super::blobs;
use super::moves::{MoveIndex, Target};
use super::starts::{self, lines_above};
use super::words::change_runs;
use crate::autodiff::Tree;
use crate::comment::store::{Anchor, Side};
use crate::diff::{self, FileDiff, Row, RowKind, path};

/// Lines of real code a block needs. See [`is_code`]. Much more than a move needs:
/// two lines coming back after being removed is a coincidence worth checking, but
/// two lines that happen to exist elsewhere are just how code looks.
const MIN_LINES: usize = 4;
/// Different shapes of line they need, as a table's rows all have one or two. See
/// [`shape_of`].
const MIN_SHAPES: usize = 3;
/// Alphanumeric characters those lines need between them.
const MIN_ALNUM: usize = 100;
/// Alphanumeric characters that make a line worth counting.
const MIN_LINE_ALNUM: usize = 8;
/// Places a line may turn up in and still count. Past this it proves nothing.
const MAX_PLACES: usize = 4;
/// Places a whole block may turn up in. Past this it's a pattern and not a copy.
const MAX_BLOCK_PLACES: usize = 2;
/// Changed lines in the whole diff past which detection is skipped.
const MAX_CHANGED_LINES: u64 = 100_000;
/// Blob bytes read for one diff, across every file.
const MAX_BLOB_BYTES: usize = 32 << 20;
/// A line this long is data, whatever it looks like.
pub(super) const MAX_LINE_BYTES: usize = 300;
/// Lines given to `git grep`, which reads each file once however many there are
/// but compares each of its lines against all of them.
const MAX_PATTERNS: usize = 256;
/// How long `git grep` gets. What it found by then still counts.
const GREP_LIMIT: Duration = Duration::from_secs(2);
/// Output read from `git grep`, as a line that was rarer than it looked can match
/// thousands of times.
const MAX_GREP_BYTES: u64 = 1 << 20;
/// Files outside the diff to read, the ones matching the most lines first.
const MAX_OUTSIDE_FILES: usize = 24;

/// Source files, by extension. Anything else is prose or data, where a repeated
/// block is how the format works.
const CODE: &[&str] = &[
    "bash", "c", "cc", "clj", "cpp", "cs", "cxx", "dart", "erl", "ex", "exs", "fish", "go", "h",
    "hpp", "hs", "java", "jl", "js", "jsx", "kt", "lua", "m", "mjs", "ml", "mm", "nim", "php",
    "pl", "pm", "ps1", "py", "r", "rb", "rs", "scala", "sh", "swift", "ts", "tsx", "vue", "zig",
    "zsh",
];
/// Path components of what's written by a tool, kept for a test to compare against,
/// or someone else's.
const NOT_OURS: &[&str] = &[
    "__fixtures__",
    "__generated__",
    "__mocks__",
    "__snapshots__",
    "__tests__",
    "build",
    "cassettes",
    "dist",
    "fixtures",
    "gen",
    "generated",
    "golden",
    "migrations",
    "node_modules",
    "snapshots",
    "spec",
    "specs",
    "target",
    "test",
    "testdata",
    "tests",
    "third_party",
    "vendor",
    "vendored",
];
/// File name endings of what's generated.
const GENERATED: &[&str] = &[
    ".bundle.js",
    ".d.ts",
    ".g.dart",
    ".gen.go",
    ".generated.ts",
    ".min.js",
    ".pb.go",
    "_generated.go",
    "_pb2.py",
    "_pb2_grpc.py",
];
/// File name endings of tests, where a repeated setup is good manners.
const TESTS: &[&str] = &[
    "Spec.java",
    "Test.java",
    "Test.kt",
    "Tests.cs",
    "Tests.swift",
    "_spec.rb",
    "_test.go",
    "_test.py",
    "_test.rb",
    "conftest.py",
];
/// What a file's first lines say if it was written by a tool.
const GENERATED_MARKS: &[&str] = &[
    "@generated",
    "auto-generated",
    "autogenerated",
    "do not edit",
    "generated by",
];
/// How far down a file a [`GENERATED_MARKS`] is looked for.
const MARK_LINES: usize = 5;

/// Starts the words of what isn't code, whatever it's written in.
const COMMENTS: &[&str] = &[
    "//", "/*", "*/", "* ", "- ", "#", "--", "<!--", "\"\"\"", "'''", ";",
];
/// First words of a line that only says what's being pulled in.
const IMPORTS: &[&str] = &[
    "extern",
    "from",
    "import",
    "include",
    "mod",
    "namespace",
    "package",
    "require",
    "use",
    "using",
];
/// First words of a line with no call in it that still does something.
const CONTROL: &[&str] = &[
    "catch", "elif", "else", "except", "for", "guard", "if", "loop", "match", "switch", "try",
    "unless", "until", "when", "while", "with",
];

/// The other end of a moved or copied line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Jump {
    Moved(Target),
    /// A line of the diff, on the far side of a copy.
    Copied(Target),
    /// A copy's source that the diff doesn't show, which is what's said instead.
    Elsewhere(String),
}

/// One file's copied rows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileCopies {
    /// The note on each block's first row.
    notes: BTreeMap<usize, String>,
    /// Where each copied line's source is, and where each source line in the diff
    /// was copied to.
    jumps: BTreeMap<Anchor, Jump>,
}

impl FileCopies {
    pub fn note(&self, row: usize) -> Option<&str> {
        self.notes.get(&row).map(String::as_str)
    }

    pub fn jump(&self, anchor: Anchor) -> Option<&Jump> {
        self.jumps.get(&anchor)
    }

    /// Whether this shows the same as `other`. The jumps aren't drawn, so a file
    /// that only gained some has no need of a new render.
    pub fn looks_like(&self, other: &Self) -> bool {
        self.notes == other.notes
    }
}

static NO_COPIES: FileCopies = FileCopies {
    notes: BTreeMap::new(),
    jumps: BTreeMap::new(),
};

/// Every copied block in a diff.
#[derive(Debug, Default)]
pub struct CopyIndex {
    files: Vec<FileCopies>,
}

impl CopyIndex {
    pub fn file(&self, file: usize) -> &FileCopies {
        self.files.get(file).unwrap_or(&NO_COPIES)
    }

    #[cfg(test)]
    pub fn set_jump_for_test(&mut self, file: usize, anchor: Anchor, jump: Jump) {
        self.files.resize_with(file + 1, FileCopies::default);
        self.files[file].jumps.insert(anchor, jump);
    }

    /// Every note, to look through.
    #[cfg(test)]
    pub fn notes(&self) -> impl Iterator<Item = (usize, usize, &str)> {
        self.files.iter().enumerate().flat_map(|(file, copies)| {
            copies
                .notes
                .iter()
                .map(move |(row, note)| (file, *row, note.as_str()))
        })
    }
}

/// Where to read what's behind a diff, when riffnav ran git for it.
#[derive(Debug, Clone)]
pub struct Git {
    pub repo: PathBuf,
    /// What the files the diff doesn't touch are read from.
    pub tree: Option<Tree>,
}

/// A line's text with every bit of whitespace taken out, as a number.
type Key = u64;

fn key_of(text: &str, buffer: &mut String) -> Key {
    buffer.clear();
    buffer.extend(text.chars().filter(|c| !c.is_whitespace()));
    XxHash3_128::oneshot(buffer.as_bytes()) as Key
}

/// A line with its names and numbers all made the same, so that the rows of a table
/// come out alike however different their words are.
fn shape_of(text: &str, buffer: &mut String) -> Key {
    buffer.clear();
    let mut in_word = false;
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        let word = c.is_alphanumeric() || c == '_';
        if !word {
            buffer.push(c);
        } else if !in_word {
            buffer.push('w');
        }
        in_word = word;
    }
    XxHash3_128::oneshot(buffer.as_bytes()) as Key
}

fn is_blank(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

/// Whether a line is the kind that says something about where it came from. A
/// license, an import, a field or a match arm comes out the same however it got
/// there, so those can sit inside a block but can't be what makes it one.
fn is_code(text: &str) -> bool {
    let line = text.trim();
    let alnum = line.chars().filter(|c| c.is_alphanumeric()).count();
    if alnum < MIN_LINE_ALNUM || line.len() > MAX_LINE_BYTES {
        return false;
    }
    if COMMENTS.iter().any(|start| line.starts_with(start)) || line.starts_with('@') {
        return false;
    }
    let word = line
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next();
    let word = word.unwrap_or_default();
    if IMPORTS.contains(&word) || is_reexport(line) {
        return false;
    }
    if word == "case" || word == "default" || is_arm(line) {
        return false;
    }
    // A row of a table, whether it's written as a tuple or a bare constructor.
    let row = line.ends_with(',') && !line.contains(" = ");
    (line.contains('(') && !row) || CONTROL.contains(&word)
}

fn is_reexport(line: &str) -> bool {
    let rest = line.strip_prefix("pub").map(str::trim_start);
    let rest = rest.map(|rest| match rest.strip_prefix('(') {
        Some(scoped) => scoped
            .split_once(')')
            .map_or(scoped, |(_, r)| r)
            .trim_start(),
        None => rest,
    });
    let word = |text: &str, word: &str| {
        text.strip_prefix(word)
            .is_some_and(|rest| rest.starts_with(' '))
    };
    rest.is_some_and(|rest| word(rest, "use") || word(rest, "mod"))
        || (line.starts_with("export") && line.contains(" from "))
        || line.contains("= require(")
}

/// A `pattern => value` line, which a closure's arrow isn't.
fn is_arm(line: &str) -> bool {
    line.split_once("=>").is_some_and(|(pattern, _)| {
        !line.ends_with(';') && !pattern.starts_with('.') && !pattern.contains('=')
    })
}

/// Whether a file is hand-written source, by its path. Lockfiles, fixtures and
/// anything generated repeat themselves, and no reviewer reads them for it.
fn is_source(path: &str) -> bool {
    let (dirs, name) = path.rsplit_once('/').unwrap_or(("", path));
    let code = name
        .rsplit_once('.')
        .is_some_and(|(_, ext)| CODE.contains(&ext.to_ascii_lowercase().as_str()));
    let test = name.starts_with("test_")
        || name.contains(".test.")
        || name.contains(".spec.")
        || TESTS.iter().any(|end| name.ends_with(end));
    code && !test
        && !dirs.split('/').any(|dir| NOT_OURS.contains(&dir))
        && !GENERATED.iter().any(|end| name.ends_with(end))
}

/// Whether `lines`, the top of a file, say a tool wrote it.
fn says_generated<'a>(lines: impl Iterator<Item = &'a str>) -> bool {
    lines.take(MARK_LINES).any(|line| {
        let line = line.to_ascii_lowercase();
        GENERATED_MARKS.iter().any(|mark| line.contains(mark))
    })
}

fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    name.rsplit_once('.').map(|(_, ext)| ext)
}

/// An added line, as the search sees it. Blank ones are left out, so a copy that
/// lost a blank line is still the same block.
struct Added {
    row: usize,
    number: u32,
    key: Key,
    shape: Key,
    alnum: usize,
    code: bool,
}

/// Added lines in a row, none of them moved.
struct Run {
    file: usize,
    lines: Vec<Added>,
}

impl Run {
    fn code_lines(&self) -> usize {
        self.lines.iter().filter(|line| line.code).count()
    }
}

/// Where a Rust file's tests start in its rows, which are test code however the file
/// is named. Either the diff shows the attribute, or git names the module in a hunk.
fn first_test_row(rows: &[Row]) -> usize {
    let tests = |row: &Row| match row.kind {
        RowKind::HunkHeader { .. } => row.text.trim_start().starts_with("mod tests"),
        _ => row.text.trim() == "#[cfg(test)]",
    };
    rows.iter().position(tests).unwrap_or(rows.len())
}

/// The runs of added lines long enough to hold a copy, in hand-written files.
fn added_runs(files: &[FileDiff], rows: &[Vec<Row>], moves: &MoveIndex) -> Vec<Run> {
    let mut buffer = String::new();
    let mut runs = Vec::new();
    for (file, rows) in rows.iter().enumerate() {
        let top = rows
            .iter()
            .filter(|row| row.new.is_some())
            .map(|row| row.text.as_str());
        if !is_source(files[file].path()) || says_generated(top) {
            continue;
        }
        let tests = first_test_row(rows);
        for (_, added) in change_runs(rows) {
            let unmoved = added.split(|&row| row >= tests || moves.file(file).at(row).is_some());
            for piece in unmoved {
                let line = |&row: &usize| {
                    let text = &rows[row].text;
                    (!is_blank(text)).then(|| Added {
                        row,
                        number: rows[row].new.unwrap_or_default(),
                        key: key_of(text, &mut buffer),
                        shape: shape_of(text, &mut buffer),
                        alnum: text.chars().filter(|c| c.is_alphanumeric()).count(),
                        code: is_code(text),
                    })
                };
                let run = Run {
                    file,
                    lines: piece.iter().filter_map(line).collect(),
                };
                if run.code_lines() >= MIN_LINES {
                    runs.push(run);
                }
            }
        }
    }
    runs
}

/// A line that's still there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Kept {
    key: Key,
    /// Its line number in the new version of the file.
    number: u32,
    /// Whether the diff shows it.
    shown: bool,
}

/// Where the code in a stretch lives.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    /// Its index, if it's a file of the diff.
    file: Option<usize>,
    path: String,
}

/// Lines with nothing changed between them. A block can't span two of these, as
/// nowhere in the new version of the file are the two halves together.
struct Stretch {
    origin: usize,
    lines: Vec<Kept>,
}

/// A line's place in a corpus.
#[derive(Clone, Copy)]
struct Place {
    stretch: usize,
    at: usize,
}

/// The code there is to have copied from.
struct Corpus {
    /// How many added lines each line of interest is.
    wanted: HashMap<Key, usize>,
    origins: Vec<Origin>,
    stretches: Vec<Stretch>,
    /// Where each line of interest is, up to [`MAX_PLACES`] and one over.
    places: HashMap<Key, Vec<Place>>,
    /// What's known of a line's count from elsewhere than here.
    seen_elsewhere: HashMap<Key, usize>,
}

impl Corpus {
    fn new(runs: &[Run]) -> Self {
        let mut wanted: HashMap<Key, usize> = HashMap::new();
        for line in runs.iter().flat_map(|run| &run.lines) {
            *wanted.entry(line.key).or_insert(0) += 1;
        }
        Self {
            wanted,
            origins: Vec::new(),
            stretches: Vec::new(),
            places: HashMap::new(),
            seen_elsewhere: HashMap::new(),
        }
    }

    fn add(&mut self, origin: Origin, stretches: Vec<Vec<Kept>>) {
        let at = self.origins.len();
        self.origins.push(origin);
        for lines in stretches {
            let stretch = self.stretches.len();
            for (at, line) in lines.iter().enumerate() {
                if self.wanted.contains_key(&line.key) {
                    let places = self.places.entry(line.key).or_default();
                    if places.len() <= MAX_PLACES {
                        places.push(Place { stretch, at });
                    }
                }
            }
            self.stretches.push(Stretch { origin: at, lines });
        }
    }

    /// Whether a line is rare enough to mean something, counting the added lines
    /// that have it: a line that's new in ten places was written by a macro or a
    /// habit.
    fn rare(&self, key: Key) -> bool {
        let here = self.places.get(&key).map_or(0, Vec::len);
        let elsewhere = self.seen_elsewhere.get(&key).copied().unwrap_or(0);
        let added = self.wanted.get(&key).copied().unwrap_or(0);
        here.max(elsewhere) + added.saturating_sub(1) <= MAX_PLACES
    }
}

/// The context of a file with no blob behind it, as stretches.
fn context_stretches(rows: &[Row]) -> Vec<Vec<Kept>> {
    let mut buffer = String::new();
    let mut stretches = vec![Vec::new()];
    for row in rows {
        match (row.kind, row.new) {
            (RowKind::Context, Some(number)) if !is_blank(&row.text) => {
                if let Some(stretch) = stretches.last_mut() {
                    stretch.push(Kept {
                        key: key_of(&row.text, &mut buffer),
                        number,
                        shown: true,
                    });
                }
            }
            (RowKind::Context | RowKind::NoNewline, _) => {}
            _ => stretches.push(Vec::new()),
        }
    }
    stretches.retain(|stretch| !stretch.is_empty());
    stretches
}

/// The unchanged lines of a file, given its old version. `None` if that isn't the
/// version the diff was made from.
fn kept_stretches(old: &str, raw: &str, rows: &[Row]) -> Option<Vec<Vec<Kept>>> {
    let old: Vec<&str> = old.lines().collect();
    if says_generated(old.iter().copied()) {
        return Some(Vec::new());
    }
    let hunks = diff::hunks(raw);
    let headers = rows
        .iter()
        .filter(|row| matches!(row.kind, RowKind::HunkHeader { .. }));
    let stops: Vec<usize> = hunks
        .iter()
        .map(|hunk| lines_above(hunk.old_start, hunk.old_len))
        .collect();
    if headers.count() != hunks.len() || !starts::shows(&old, &stops, rows) {
        return None;
    }

    let mut buffer = String::new();
    let mut stretches = vec![Vec::new()];
    // The next old line, and what to add to it for the same line on the new side.
    let (mut next, mut delta) = (1_i64, 0_i64);
    let mut keep = |stretches: &mut Vec<Vec<Kept>>, line: i64, new: i64, shown: bool| {
        let text = old.get(line as usize - 1).copied().unwrap_or_default();
        if !is_blank(text)
            && let Some(stretch) = stretches.last_mut()
        {
            stretch.push(Kept {
                key: key_of(text, &mut buffer),
                number: new as u32,
                shown,
            });
        }
    };
    let mut hunks = hunks.iter();
    for row in rows {
        match row.kind {
            RowKind::HunkHeader { .. } => {
                let hunk = hunks.next()?;
                let above = lines_above(hunk.old_start, hunk.old_len) as i64;
                while next <= above {
                    keep(&mut stretches, next, next + delta, false);
                    next += 1;
                }
                let new_above = lines_above(hunk.new_start, hunk.new_len) as i64;
                delta = new_above - above;
            }
            RowKind::Context => {
                let (old, new) = (i64::from(row.old?), i64::from(row.new?));
                delta = new - old;
                keep(&mut stretches, old, new, true);
                next = old + 1;
            }
            RowKind::Removed => {
                stretches.push(Vec::new());
                next += 1;
                delta -= 1;
            }
            RowKind::Added => {
                stretches.push(Vec::new());
                delta += 1;
            }
            RowKind::NoNewline => {}
        }
    }
    while next <= old.len() as i64 {
        keep(&mut stretches, next, next + delta, false);
        next += 1;
    }
    stretches.retain(|stretch| !stretch.is_empty());
    Some(stretches)
}

/// `len` lines of a run from `from` matching a stretch from `at`.
#[derive(Debug, Clone, Copy)]
struct Block {
    run: usize,
    from: usize,
    len: usize,
    stretch: usize,
    at: usize,
}

/// How many places each `(run, from, len)` of added lines turned up in.
type BlockPlaces = HashMap<(usize, usize, usize), usize>;

/// Every maximal match between `runs` and `corpus` that starts on a line worth
/// starting on.
fn matches(runs: &[Run], free: &[Vec<bool>], corpus: &Corpus) -> (Vec<Block>, BlockPlaces) {
    let mut blocks = Vec::new();
    let mut counts = HashMap::new();
    let mut seen = HashSet::new();
    for (run_at, run) in runs.iter().enumerate() {
        for (from, line) in run.lines.iter().enumerate() {
            if !line.code || !free[run_at][from] || !corpus.rare(line.key) {
                continue;
            }
            let places = corpus.places.get(&line.key).map_or(&[][..], Vec::as_slice);
            for place in places {
                let stretch = &corpus.stretches[place.stretch].lines;
                // Lines are walked top down, so a pair already seen sits inside a
                // block found from further up.
                if !seen.insert((run_at, from, place.stretch, place.at)) {
                    continue;
                }
                let len = run.lines[from..]
                    .iter()
                    .zip(&stretch[place.at..])
                    .take_while(|(added, kept)| added.key == kept.key)
                    .count();
                seen.extend(
                    (1..len).map(|step| (run_at, from + step, place.stretch, place.at + step)),
                );
                *counts.entry((run_at, from, len)).or_insert(0) += 1;
                blocks.push(Block {
                    run: run_at,
                    from,
                    len,
                    stretch: place.stretch,
                    at: place.at,
                });
            }
        }
    }
    (blocks, counts)
}

/// How far `block`'s source is from the code that copied it, nearest first: what
/// the reader can jump to, then the same file, then the same folder.
fn distance(block: &Block, runs: &[Run], corpus: &Corpus, files: &[FileDiff]) -> (bool, usize) {
    let stretch = &corpus.stretches[block.stretch];
    let origin = &corpus.origins[stretch.origin];
    let file = runs[block.run].file;
    let folder = |path: &str| path.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
    let near = if origin.file == Some(file) {
        0
    } else if folder(&origin.path) == folder(files[file].path()) {
        1
    } else {
        2
    };
    (!stretch.lines[block.at].shown, near)
}

/// The blocks that are worth a note, each added line going to one of them.
fn claim(
    mut candidates: Vec<Block>,
    counts: &BlockPlaces,
    runs: &[Run],
    free: &mut [Vec<bool>],
    corpus: &Corpus,
    files: &[FileDiff],
) -> Vec<Block> {
    candidates.sort_by_cached_key(|block| {
        let origin = &corpus.origins[corpus.stretches[block.stretch].origin];
        let line = corpus.stretches[block.stretch].lines[block.at].number;
        let (hidden, near) = distance(block, runs, corpus, files);
        (
            Reverse(block.len),
            hidden,
            near,
            origin.path.clone(),
            line,
            block.run,
            block.from,
        )
    });
    let mut blocks = Vec::new();
    for candidate in candidates {
        let places = counts
            .get(&(candidate.run, candidate.from, candidate.len))
            .copied()
            .unwrap_or(1);
        if places > MAX_BLOCK_PLACES {
            continue;
        }
        let mut at = 0;
        while at < candidate.len {
            let room = (at..candidate.len)
                .take_while(|&i| free[candidate.run][candidate.from + i])
                .count();
            let piece = Block {
                from: candidate.from + at,
                at: candidate.at + at,
                len: room,
                ..candidate
            };
            at += room.max(1);
            if let Some(block) = qualified(piece, runs, corpus) {
                free[block.run][block.from..block.from + block.len].fill(false);
                blocks.push(block);
            }
        }
    }
    blocks
}

/// `block` from its first line of code to its last, if that's enough to call a copy.
fn qualified(block: Block, runs: &[Run], corpus: &Corpus) -> Option<Block> {
    let lines = &runs[block.run].lines[block.from..block.from + block.len];
    let counts = |line: &&Added| line.code && corpus.rare(line.key);
    let lead = lines.iter().position(|line| counts(&line))?;
    let lines = &lines[lead..];
    let code: Vec<&Added> = lines.iter().filter(counts).collect();
    let distinct: HashSet<Key> = code.iter().map(|line| line.key).collect();
    let shapes: HashSet<Key> = code.iter().map(|line| line.shape).collect();
    let alnum: usize = code.iter().map(|line| line.alnum).sum();
    let enough = distinct.len() >= MIN_LINES && shapes.len() >= MIN_SHAPES;
    (enough && alnum >= MIN_ALNUM).then_some(Block {
        from: block.from + lead,
        at: block.at + lead,
        len: lines.len(),
        ..block
    })
}

/// One pass over what's in `corpus`, claiming what it can explain.
fn search(runs: &[Run], free: &mut [Vec<bool>], corpus: &Corpus, files: &[FileDiff]) -> Vec<Block> {
    let (candidates, counts) = matches(runs, free, corpus);
    claim(candidates, &counts, runs, free, corpus, files)
}

/// What reading from git has to go on.
struct Reading<'a> {
    git: &'a Git,
    /// Blob bytes still to spend.
    budget: usize,
    cancelled: &'a AtomicBool,
}

impl Reading<'_> {
    fn blobs(&mut self, specs: &[String]) -> Option<Vec<Option<String>>> {
        blobs::read_blobs(&self.git.repo, specs, &mut self.budget, self.cancelled)
    }
}

/// Add the files of the diff as they are outside the changes: read in full when
/// their blobs can be had, as the context in the diff otherwise.
fn add_diff_files(
    corpus: &mut Corpus,
    files: &[FileDiff],
    rows: &[Vec<Row>],
    extensions: &HashSet<String>,
    reading: Option<&mut Reading<'_>>,
    cancelled: &AtomicBool,
) -> Option<()> {
    let mut candidates = Vec::new();
    for (at, file) in files.iter().enumerate() {
        let path = file.path();
        let same_language = extension(path).is_some_and(|ext| extensions.contains(ext));
        if file.new_path.is_some() && is_source(path) && same_language {
            candidates.push(at);
        }
    }
    let ids: Vec<(usize, String)> = candidates
        .iter()
        .filter_map(|&at| Some((at, starts::old_blob(&files[at].raw)?.to_string())))
        .collect();
    let mut blobs: HashMap<usize, String> = HashMap::new();
    if let Some(reading) = reading {
        let specs: Vec<String> = ids.iter().map(|(_, id)| id.clone()).collect();
        for ((at, _), text) in ids.iter().zip(reading.blobs(&specs)?) {
            if let Some(text) = text {
                blobs.insert(*at, text);
            }
        }
    }

    for at in candidates {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let file = &files[at];
        let kept = blobs
            .get(&at)
            .and_then(|old| kept_stretches(old, &file.raw, &rows[at]))
            .unwrap_or_else(|| context_stretches(&rows[at]));
        let origin = Origin {
            file: Some(at),
            path: file.path().to_string(),
        };
        corpus.add(origin, kept);
    }
    Some(())
}

/// What's left of the runs after `free`, as lines to look for elsewhere: the longest
/// of each half of [`MIN_LINES`] lines of code, as a block that long, wherever it
/// starts, holds at least one such half whole.
fn rare_lines(runs: &[Run], rows: &[Vec<Row>], free: &[Vec<bool>], corpus: &Corpus) -> Vec<String> {
    let mut patterns = HashSet::new();
    for (at, run) in runs.iter().enumerate() {
        let open = run
            .lines
            .iter()
            .zip(&free[at])
            .filter(|(line, free)| **free && line.code && corpus.rare(line.key))
            .map(|(line, _)| line);
        let open: Vec<&Added> = open.collect();
        for window in open.chunks(MIN_LINES / 2) {
            if let Some(line) = window.iter().max_by_key(|line| line.alnum) {
                patterns.insert(rows[run.file][line.row].text.trim().to_string());
            }
        }
    }
    let mut patterns: Vec<String> = patterns.into_iter().collect();
    patterns.sort_by_key(|line| (Reverse(line.len()), line.clone()));
    patterns.truncate(MAX_PATTERNS);
    patterns
}

/// Add the files outside the diff that share lines with what's left of the runs.
fn add_outside_files(
    corpus: &mut Corpus,
    reading: &mut Reading<'_>,
    tree: &Tree,
    patterns: &[String],
    files: &[FileDiff],
    extensions: &HashSet<String>,
) -> Option<()> {
    let globs: Vec<String> = extensions
        .iter()
        .map(|ext| format!(":(top,glob)**/*.{ext}"))
        .collect();
    let limits = (GREP_LIMIT, MAX_GREP_BYTES);
    let repo = &reading.git.repo;
    let hits = blobs::grep(repo, tree, patterns, &globs, limits, reading.cancelled)?;

    let mut buffer = String::new();
    let mut counts: HashMap<Key, usize> = HashMap::new();
    for hit in &hits {
        *counts.entry(key_of(&hit.text, &mut buffer)).or_insert(0) += 1;
    }
    let sides = files
        .iter()
        .flat_map(|file| [&file.old_path, &file.new_path]);
    let touched: HashSet<&str> = sides.flatten().map(String::as_str).collect();
    let mut matched: HashMap<&str, usize> = HashMap::new();
    for hit in &hits {
        let key = key_of(&hit.text, &mut buffer);
        let rare = counts.get(&key).is_some_and(|&count| count <= MAX_PLACES);
        let path = hit.path.as_str();
        // `git cat-file` takes a name a line, so one with a newline can't be read.
        if rare && is_source(path) && !touched.contains(path) && !path.contains('\n') {
            *matched.entry(path).or_insert(0) += 1;
        }
    }
    corpus.seen_elsewhere.extend(counts);

    let mut paths: Vec<(&str, usize)> = matched.into_iter().collect();
    paths.sort_by_key(|&(path, count)| (Reverse(count), path));
    paths.truncate(MAX_OUTSIDE_FILES);
    let specs: Vec<String> = paths.iter().map(|(path, _)| tree.spec(path)).collect();
    for ((path, _), text) in paths.iter().zip(reading.blobs(&specs)?) {
        let Some(text) = text.filter(|text| !says_generated(text.lines())) else {
            continue;
        };
        let lines = text.lines().enumerate().filter(|(_, line)| !is_blank(line));
        let lines = lines.map(|(at, line)| Kept {
            key: key_of(line, &mut buffer),
            number: at as u32 + 1,
            shown: false,
        });
        let origin = Origin {
            file: None,
            path: (*path).to_string(),
        };
        corpus.add(origin, vec![lines.collect()]);
    }
    Some(())
}

/// Whether a diff is small enough to look through, which is the check to make before
/// going to the trouble of handing it to another thread.
pub fn worth_trying(files: &[FileDiff]) -> bool {
    let changed: u64 = files
        .iter()
        .map(|f| u64::from(f.additions) + u64::from(f.deletions))
        .sum();
    (1..=MAX_CHANGED_LINES).contains(&changed)
}

/// Find what the diff copied. `None` if `cancelled` was set first.
pub fn build(
    files: &[FileDiff],
    moves: &MoveIndex,
    git: Option<&Git>,
    cancelled: &AtomicBool,
) -> Option<CopyIndex> {
    let rows: Vec<Vec<Row>> = files.iter().map(diff::rows).collect();
    let runs = added_runs(files, &rows, moves);
    if runs.is_empty() {
        return Some(CopyIndex::default());
    }
    let extensions: HashSet<String> = runs
        .iter()
        .filter_map(|run| extension(files[run.file].path()))
        .map(str::to_string)
        .collect();
    let mut free: Vec<Vec<bool>> = runs.iter().map(|run| vec![true; run.lines.len()]).collect();

    let mut reading = git.map(|git| Reading {
        git,
        budget: MAX_BLOB_BYTES,
        cancelled,
    });
    let mut corpus = Corpus::new(&runs);
    let diff_reading = reading.as_mut();
    add_diff_files(
        &mut corpus,
        files,
        &rows,
        &extensions,
        diff_reading,
        cancelled,
    )?;
    let mut blocks = search(&runs, &mut free, &corpus, files);

    if let Some(reading) = &mut reading
        && let Some(tree) = &reading.git.tree
    {
        let patterns = rare_lines(&runs, &rows, &free, &corpus);
        if !patterns.is_empty() {
            let tree = tree.clone();
            add_outside_files(&mut corpus, reading, &tree, &patterns, files, &extensions)?;
            blocks.extend(search(&runs, &mut free, &corpus, files));
        }
    }
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    Some(index_of(&blocks, &runs, &corpus, files))
}

/// Put the blocks into a shape the renderer and the `m` key can use.
fn index_of(blocks: &[Block], runs: &[Run], corpus: &Corpus, files: &[FileDiff]) -> CopyIndex {
    let mut index: Vec<FileCopies> = files.iter().map(|_| FileCopies::default()).collect();
    let new = |line: u32| Anchor {
        side: Side::New,
        line,
    };
    for block in blocks {
        let run = &runs[block.run];
        let stretch = &corpus.stretches[block.stretch];
        let origin = &corpus.origins[stretch.origin];
        let added = &run.lines[block.from..block.from + block.len];
        let kept = &stretch.lines[block.at..block.at + block.len];
        let place = |line: u32| match origin.file == Some(run.file) {
            true => format!("line {line}"),
            false => format!("{}:{line}", path::shown(&origin.path)),
        };
        index[run.file].notes.insert(
            added[0].row,
            format!("copied from {}", place(kept[0].number)),
        );

        // A line can be off screen while the block it's in is partly shown.
        let shown = kept.iter().find(|line| line.shown).map(|line| line.number);
        for (added, kept) in added.iter().zip(kept) {
            let number = if kept.shown { Some(kept.number) } else { shown };
            let jump = match origin.file.zip(number) {
                Some((file, number)) => Jump::Copied(Target {
                    file,
                    anchor: new(number),
                }),
                None => Jump::Elsewhere(format!(
                    "Copied from {}, not in this diff",
                    place(kept.number)
                )),
            };
            index[run.file].jumps.insert(new(added.number), jump);
            if let (Some(file), true) = (origin.file, kept.shown) {
                let back = Jump::Copied(Target {
                    file: run.file,
                    anchor: new(added.number),
                });
                index[file].jumps.entry(new(kept.number)).or_insert(back);
            }
        }
    }
    CopyIndex { files: index }
}

#[cfg(test)]
mod tests;

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

mod corpus;
mod filter;
mod matching;
mod sources;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use self::corpus::{Corpus, Run, added_runs};
use self::filter::extension;
use self::matching::{Block, search};
use self::sources::{Reading, add_diff_files, add_outside_files, rare_lines};
use super::moves::{self, MAX_CHANGED_LINES, MoveIndex, Target, lines_changed};
use crate::autodiff::Tree;
use crate::comment::store::{Anchor, Side};
use crate::diff::{self, FileDiff, Row};

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

/// Whether a diff is small enough to look through, which is the check to make before
/// going to the trouble of handing it to another thread.
pub fn worth_trying(files: &[FileDiff]) -> bool {
    (1..=MAX_CHANGED_LINES).contains(&lines_changed(files))
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
        let place = |line| moves::place(origin.file == Some(run.file), &origin.path, line);
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

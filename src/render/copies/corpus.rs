//! The added lines to find a source for, and the code that can be one.

use std::collections::HashMap;

use twox_hash::XxHash3_128;

use super::filter::{is_blank, is_code, is_source, says_generated, shape_of};
use super::{MAX_PLACES, MIN_LINES};
use crate::diff::change_runs;
use crate::diff::{self, FileDiff, Row, RowKind};
use crate::render::moves::MoveIndex;
use crate::render::starts::{self, lines_above};

/// A line's text with every bit of whitespace taken out, as a number.
pub(super) type Key = u64;

pub(super) fn key_of(text: &str, buffer: &mut String) -> Key {
    buffer.clear();
    buffer.extend(text.chars().filter(|c| !c.is_whitespace()));
    XxHash3_128::oneshot(buffer.as_bytes()) as Key
}

/// An added line, as the search sees it. Blank ones are left out, so a copy that
/// lost a blank line is still the same block.
pub(super) struct Added {
    pub(super) row: usize,
    pub(super) number: u32,
    pub(super) key: Key,
    pub(super) shape: Key,
    pub(super) alnum: usize,
    pub(super) code: bool,
}

/// Added lines in a row, none of them moved.
pub(super) struct Run {
    pub(super) file: usize,
    pub(super) lines: Vec<Added>,
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
pub(super) fn added_runs(files: &[FileDiff], rows: &[Vec<Row>], moves: &MoveIndex) -> Vec<Run> {
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
pub(super) struct Kept {
    pub(super) key: Key,
    /// Its line number in the new version of the file.
    pub(super) number: u32,
    /// Whether the diff shows it.
    pub(super) shown: bool,
}

/// Where the code in a stretch lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Origin {
    /// Its index, if it's a file of the diff.
    pub(super) file: Option<usize>,
    pub(super) path: String,
}

/// Lines with nothing changed between them. A block can't span two of these, as
/// nowhere in the new version of the file are the two halves together.
pub(super) struct Stretch {
    pub(super) origin: usize,
    pub(super) lines: Vec<Kept>,
}

/// A line's place in a corpus.
#[derive(Clone, Copy)]
pub(super) struct Place {
    pub(super) stretch: usize,
    pub(super) at: usize,
}

/// The code there is to have copied from.
pub(super) struct Corpus {
    /// How many added lines each line of interest is.
    wanted: HashMap<Key, usize>,
    pub(super) origins: Vec<Origin>,
    pub(super) stretches: Vec<Stretch>,
    /// Where each line of interest is, up to [`MAX_PLACES`] and one over.
    pub(super) places: HashMap<Key, Vec<Place>>,
    /// What's known of a line's count from elsewhere than here.
    pub(super) seen_elsewhere: HashMap<Key, usize>,
}

impl Corpus {
    pub(super) fn new(runs: &[Run]) -> Self {
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

    pub(super) fn add(&mut self, origin: Origin, stretches: Vec<Vec<Kept>>) {
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
    pub(super) fn rare(&self, key: Key) -> bool {
        let here = self.places.get(&key).map_or(0, Vec::len);
        let elsewhere = self.seen_elsewhere.get(&key).copied().unwrap_or(0);
        let added = self.wanted.get(&key).copied().unwrap_or(0);
        here.max(elsewhere) + added.saturating_sub(1) <= MAX_PLACES
    }
}

/// The context of a file with no blob behind it, as stretches.
pub(super) fn context_stretches(rows: &[Row]) -> Vec<Vec<Kept>> {
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
pub(super) fn kept_stretches(old: &str, raw: &str, rows: &[Row]) -> Option<Vec<Vec<Kept>>> {
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

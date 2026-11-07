//! Moved-line detection: which removed lines came back somewhere else.
//!
//! git reports a move as an unrelated removal and addition, so a reviewer reads the
//! same code twice. Matching the two up, across every file in the diff, lets the
//! renderer mute those rows and say where each block went.
//!
//! A block is a run of consecutive removed lines that reappears as a run of
//! consecutive added lines, in order and ignoring whitespace. The thresholds follow
//! git's `--color-moved=blocks`, which wants 20 alphanumeric characters per block,
//! and add a two-line minimum: a lone line coming back is usually a coincidence.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};

use super::words::change_runs;
use crate::comment::store::{Anchor, Side};
use crate::diff::{self, FileDiff, Row, path};

/// Substantial lines a block needs. See [`Line::is_anchor`].
const MIN_LINES: usize = 2;
/// Alphanumeric characters a block needs, so `}` and `)` can't add up to a move.
const MIN_ALNUM: usize = 20;
/// Alphanumeric characters that make a line substantial. Anything shorter (a
/// brace, a blank, `else`) can continue a block but can't start one, because it
/// matches everywhere.
const MIN_ANCHOR_ALNUM: usize = 5;
/// How many added copies of a line it may have and still start a block. This bounds
/// the pairs compared, and so the memory used, at this many per removed line.
const MAX_COPIES: usize = 32;
/// Changed lines in the whole diff past which move and copy detection are skipped.
pub(super) const MAX_CHANGED_LINES: u64 = 100_000;
/// The hint on a block that never left its place: re-indented, realigned, or
/// given new line endings.
const WHITESPACE_ONLY: &str = "whitespace only";

/// What a moved row needs the renderer to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    /// On a block's first row, where the block went or came from. A block that only
    /// carries on the whitespace change above it has none. See [`repeats`].
    pub hint: Option<String>,
    /// On a removed row, the added row it matches if the two share a change, which
    /// is what side-by-side sets opposite it.
    pub twin: Option<usize>,
}

/// The line at the other end of a move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub file: usize,
    pub anchor: Anchor,
}

/// One file's moved rows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileMoves {
    /// Index-aligned with the file's rows, or empty when nothing in it moved.
    marks: Vec<Option<Mark>>,
    /// Where each moved line's other end is.
    targets: BTreeMap<Anchor, Target>,
}

impl FileMoves {
    pub fn at(&self, row: usize) -> Option<&Mark> {
        self.marks.get(row)?.as_ref()
    }

    pub fn target(&self, anchor: Anchor) -> Option<Target> {
        self.targets.get(&anchor).copied()
    }
}

static NO_MOVES: FileMoves = FileMoves {
    marks: Vec::new(),
    targets: BTreeMap::new(),
};

/// Every moved row in a diff. A block can span two files, so this is built from
/// the whole file set rather than file by file.
#[derive(Debug, Default)]
pub struct MoveIndex {
    files: Vec<FileMoves>,
}

/// The lines `files` add and remove between them.
pub(super) fn lines_changed(files: &[FileDiff]) -> u64 {
    files
        .iter()
        .map(|f| u64::from(f.additions) + u64::from(f.deletions))
        .sum()
}

impl MoveIndex {
    pub fn build(files: &[FileDiff]) -> Self {
        if lines_changed(files) > MAX_CHANGED_LINES {
            return Self::default();
        }

        let rows: Vec<Vec<Row>> = files.iter().map(diff::rows).collect();
        let (removed, added) = changed_lines(&rows);
        let blocks = claim(matches(&removed, &added), &removed, &added);

        let reordered = reordered_runs(&blocks, &removed, &added);
        let stayed = |block: &Block| {
            let (old, new) = (&removed[block.old], &added[block.new]);
            old.run == new.run && !reordered.contains(&old.run)
        };
        let old_repeats = repeats(&blocks, &removed, |b| b.old, stayed);
        let new_repeats = repeats(&blocks, &added, |b| b.new, stayed);
        let mut marks: Vec<Vec<Option<Mark>>> = rows.iter().map(|r| vec![None; r.len()]).collect();
        let mut targets = vec![BTreeMap::new(); rows.len()];
        for block in &blocks {
            let (old, new) = (&removed[block.old], &added[block.new]);
            let (to, from) = if stayed(block) {
                (WHITESPACE_ONLY.to_string(), WHITESPACE_ONLY.to_string())
            } else {
                hints(old, new, files)
            };
            for at in 0..block.len {
                let (old, new) = (&removed[block.old + at], &added[block.new + at]);
                marks[old.file][old.row] = Some(Mark {
                    hint: (at == 0 && !old_repeats.contains(&block.old)).then(|| to.clone()),
                    twin: (old.run == new.run).then_some(new.row),
                });
                marks[new.file][new.row] = Some(Mark {
                    hint: (at == 0 && !new_repeats.contains(&block.new)).then(|| from.clone()),
                    twin: None,
                });
                let (old_end, new_end) = (old.target(Side::Old), new.target(Side::New));
                targets[old.file].insert(old_end.anchor, new_end);
                targets[new.file].insert(new_end.anchor, old_end);
            }
        }
        let files = marks
            .into_iter()
            .zip(targets)
            .map(|(marks, targets)| FileMoves {
                marks: if targets.is_empty() {
                    Vec::new()
                } else {
                    marks
                },
                targets,
            })
            .collect();
        Self { files }
    }

    pub fn file(&self, file: usize) -> &FileMoves {
        self.files.get(file).unwrap_or(&NO_MOVES)
    }
}

/// A removed or added line, as the search sees it.
struct Line {
    file: usize,
    row: usize,
    /// Its line number on the side it's on.
    number: u32,
    /// Which change it's in, unique across files. Lines of one side sharing a run
    /// are consecutive.
    run: usize,
    /// Its text without whitespace, interned so comparing lines is comparing ints.
    key: u32,
    alnum: usize,
}

impl Line {
    fn is_anchor(&self) -> bool {
        self.alnum >= MIN_ANCHOR_ALNUM
    }

    fn target(&self, side: Side) -> Target {
        Target {
            file: self.file,
            anchor: Anchor {
                side,
                line: self.number,
            },
        }
    }
}

/// Every removed and added line in the diff, in order.
fn changed_lines(files: &[Vec<Row>]) -> (Vec<Line>, Vec<Line>) {
    let mut keys: HashMap<String, u32> = HashMap::new();
    let mut line = |file: usize, row: usize, run: usize| {
        let Row { text, old, new, .. } = &files[file][row];
        let bare: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let alnum = bare.chars().filter(|c| c.is_alphanumeric()).count();
        let next = keys.len() as u32;
        Line {
            file,
            row,
            number: old.or(*new).unwrap_or_default(),
            run,
            key: *keys.entry(bare).or_insert(next),
            alnum,
        }
    };

    let (mut removed, mut added) = (Vec::new(), Vec::new());
    let mut run = 0;
    for (file, rows) in files.iter().enumerate() {
        for (old, new) in change_runs(rows) {
            // A merge's line that only another parent lost was never on the old
            // side, and there'd be no number to say it moved from.
            let old = old.into_iter().filter(|&row| rows[row].old.is_some());
            removed.extend(old.map(|row| line(file, row, run)));
            added.extend(new.into_iter().map(|row| line(file, row, run)));
            run += 1;
        }
    }
    (removed, added)
}

/// `len` lines of `removed` from `old` matching as many of `added` from `new`.
#[derive(Debug, Clone, Copy)]
struct Block {
    old: usize,
    new: usize,
    len: usize,
}

/// Every maximal run of matching lines that starts on an anchor, best first.
fn matches(removed: &[Line], added: &[Line]) -> Vec<Block> {
    let mut copies: HashMap<u32, Vec<usize>> = HashMap::new();
    for (at, line) in added.iter().enumerate() {
        if line.is_anchor() {
            copies.entry(line.key).or_default().push(at);
        }
    }

    let continues = |lines: &[Line], from: usize, at: usize| {
        lines
            .get(from + at)
            .is_some_and(|l| l.run == lines[from].run)
    };
    let mut seen = HashSet::new();
    let mut blocks = Vec::new();
    for (old, line) in removed.iter().enumerate() {
        let Some(copies) = copies.get(&line.key) else {
            continue;
        };
        if !line.is_anchor() || copies.len() > MAX_COPIES {
            continue;
        }
        for &new in copies {
            // Lines are walked top down, so a pair already seen sits inside a
            // block found from further up.
            if !seen.insert((old, new)) {
                continue;
            }
            let len = 1
                + (1..)
                    .take_while(|&at| {
                        continues(removed, old, at)
                            && continues(added, new, at)
                            && removed[old + at].key == added[new + at].key
                    })
                    .count();
            seen.extend((1..len).map(|at| (old + at, new + at)));
            if len >= MIN_LINES {
                blocks.push(Block { old, new, len });
            }
        }
    }

    // Longest first, then closest, so a line with several homes goes to the one
    // that explains the most. The rest of the key keeps the order stable.
    blocks.sort_by_key(|b| {
        let (old, new) = (&removed[b.old], &added[b.new]);
        let distance = match (old.run == new.run, old.file == new.file) {
            (true, _) => 0,
            (_, true) => 1,
            _ => 2,
        };
        (Reverse(b.len), distance, b.old, b.new)
    });
    blocks
}

/// Give each line to the first block that wants it, so every line has one home.
/// What's left of a block that lost lines is judged again as blocks of its own.
fn claim(candidates: Vec<Block>, removed: &[Line], added: &[Line]) -> Vec<Block> {
    let mut old_free = vec![true; removed.len()];
    let mut new_free = vec![true; added.len()];
    let mut blocks = Vec::new();
    for candidate in candidates {
        let mut at = 0;
        while at < candidate.len {
            let free = (at..candidate.len)
                .take_while(|&i| old_free[candidate.old + i] && new_free[candidate.new + i])
                .count();
            let piece = Block {
                old: candidate.old + at,
                new: candidate.new + at,
                len: free,
            };
            at += free.max(1);
            if let Some(block) = qualified(piece, removed) {
                old_free[block.old..block.old + block.len].fill(false);
                new_free[block.new..block.new + block.len].fill(false);
                blocks.push(block);
            }
        }
    }
    blocks
}

/// `block` from its first anchor on, if that's enough to call a move.
fn qualified(block: Block, removed: &[Line]) -> Option<Block> {
    let lines = &removed[block.old..block.old + block.len];
    let lead = lines.iter().position(Line::is_anchor)?;
    let lines = &lines[lead..];
    let anchors = lines.iter().filter(|l| l.is_anchor()).count();
    let alnum: usize = lines.iter().map(|l| l.alnum).sum();
    (anchors >= MIN_LINES && alnum >= MIN_ALNUM).then_some(Block {
        old: block.old + lead,
        new: block.new + lead,
        len: lines.len(),
    })
}

/// The changes whose blocks came back in a different order. A block that stayed
/// in its own change only had its whitespace touched, unless it swapped places
/// with another there, which is a real move.
fn reordered_runs(blocks: &[Block], removed: &[Line], added: &[Line]) -> HashSet<usize> {
    let mut local: Vec<&Block> = blocks
        .iter()
        .filter(|b| removed[b.old].run == added[b.new].run)
        .collect();
    local.sort_by_key(|b| b.old);
    local
        .windows(2)
        .filter(|pair| removed[pair[0].old].run == removed[pair[1].old].run)
        .filter(|pair| pair[0].new > pair[1].new)
        .map(|pair| removed[pair[0].old].run)
        .collect()
}

/// Where the blocks that needn't say anything start on one side, `start` being where
/// a block starts there. An edit splits a whitespace-only block into pieces, and
/// the note on the first speaks for them all. A piece under another block's note
/// keeps its own, or it would read as part of that block.
fn repeats(
    blocks: &[Block],
    lines: &[Line],
    start: impl Fn(&Block) -> usize,
    stayed: impl Fn(&Block) -> bool,
) -> HashSet<usize> {
    let mut order: Vec<&Block> = blocks.iter().collect();
    order.sort_by_key(|b| start(b));
    order
        .windows(2)
        .filter(|pair| lines[start(pair[0])].run == lines[start(pair[1])].run)
        .filter(|pair| stayed(pair[0]) && stayed(pair[1]))
        .map(|pair| start(pair[1]))
        .collect()
}

/// What to say above a block's removed and added halves, given their first lines.
fn hints(old: &Line, new: &Line, files: &[FileDiff]) -> (String, String) {
    let there = |verb: &str, line: &Line| {
        if old.file == new.file {
            format!("moved {verb} line {}", line.number)
        } else {
            let path = path::shown(files[line.file].path());
            format!("moved {verb} {path}:{}", line.number)
        }
    };
    (there("to", new), there("from", old))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::testing::header;
    use crate::diff::{RowKind, parse};

    const FN_A: [&str; 3] = ["fn alpha_value() {", "    first_call(1);", "}"];
    const FN_B: [&str; 3] = ["fn beta_value() {", "    second_call(2);", "}"];

    /// A file whose only hunk removes `removed` from line 1 and adds `added`,
    /// separated from each other by a kept line.
    fn file(name: &str, removed: &[&str], added: &[&str]) -> String {
        let mut raw = format!(
            "{}@@ -1,{} +1,{} @@\n",
            header(name),
            removed.len() + 1,
            added.len() + 1,
        );
        for line in removed {
            raw.push_str(&format!("-{line}\n"));
        }
        raw.push_str(" kept\n");
        for line in added {
            raw.push_str(&format!("+{line}\n"));
        }
        raw
    }

    /// The moved rows of each file, as `(sign, text, hint)`.
    fn moved(raw: &str) -> Vec<Vec<(char, String, Option<String>)>> {
        let files = parse(raw);
        let index = MoveIndex::build(&files);
        files
            .iter()
            .enumerate()
            .map(|(at, file)| {
                diff::rows(file)
                    .iter()
                    .enumerate()
                    .filter_map(|(i, row)| {
                        let mark = index.file(at).at(i)?;
                        let sign = if row.kind == RowKind::Added { '+' } else { '-' };
                        Some((sign, row.text.clone(), mark.hint.clone()))
                    })
                    .collect()
            })
            .collect()
    }

    fn texts(rows: &[(char, String, Option<String>)]) -> Vec<&str> {
        rows.iter().map(|r| r.1.as_str()).collect()
    }

    fn hint_of(rows: &[(char, String, Option<String>)], sign: char) -> Option<&str> {
        rows.iter().find(|r| r.0 == sign)?.2.as_deref()
    }

    #[test]
    fn a_merge_moves_what_the_first_parent_had() {
        let merge = |marker: &str| {
            let mut raw = "diff --cc f\n--- a/f\n+++ b/f\n@@@ -1,9 -1,9 +1,9 @@@\n".to_string();
            for line in FN_A {
                raw.push_str(&format!("{marker}{line}\n"));
            }
            raw.push_str("  kept\n");
            for line in FN_A {
                raw.push_str(&format!("++{line}\n"));
            }
            raw
        };
        let got = moved(&merge("- "));
        assert_eq!(hint_of(&got[0], '+'), Some("moved from line 1"));
        // Only the second parent lost these, so nothing moved as far as the old
        // side goes, and they have no line to say it from.
        assert_eq!(moved(&merge(" -")), [[]]);
    }

    #[test]
    fn a_block_moved_within_a_file_says_where_it_went() {
        let got = moved(&file("f", &FN_A, &FN_A));
        assert_eq!(got[0].len(), 6);
        assert_eq!(hint_of(&got[0], '-'), Some("moved to line 2"));
        assert_eq!(hint_of(&got[0], '+'), Some("moved from line 1"));
    }

    #[test]
    fn only_a_blocks_first_row_carries_the_hint() {
        let got = moved(&file("f", &FN_A, &FN_A));
        assert_eq!(got[0].iter().filter(|r| r.2.is_some()).count(), 2);
    }

    #[test]
    fn a_block_moved_between_files_names_the_other_file() {
        let raw = file("a.rs", &FN_A, &[]) + &file("b.rs", &[], &FN_A);
        let got = moved(&raw);
        assert_eq!(hint_of(&got[0], '-'), Some("moved to b.rs:2"));
        assert_eq!(hint_of(&got[1], '+'), Some("moved from a.rs:1"));
    }

    #[test]
    fn indentation_doesnt_stop_a_match() {
        let indented = FN_A.map(|l| format!("    {l}"));
        let indented: Vec<&str> = indented.iter().map(String::as_str).collect();
        assert_eq!(moved(&file("f", &FN_A, &indented))[0].len(), 6);
    }

    #[test]
    fn code_reindented_in_place_is_a_whitespace_change() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n\
                   -first_call(1);\n-second_call(2);\n\
                   +    first_call(1);\n+    second_call(2);\n";
        let got = moved(raw);
        assert_eq!(hint_of(&got[0], '-'), Some(WHITESPACE_ONLY));
        assert_eq!(hint_of(&got[0], '+'), Some(WHITESPACE_ONLY));
    }

    #[test]
    fn realigned_comments_are_a_whitespace_change() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n\
                   -first_call(1);  // one\n-second_call(2); // two\n\
                   +first_call(1);      // one\n+second_call(2);     // two\n";
        assert_eq!(hint_of(&moved(raw)[0], '-'), Some(WHITESPACE_ONLY));
    }

    #[test]
    fn new_line_endings_are_a_whitespace_change_not_a_move() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n\
                   -first_call(1);\n-second_call(2);\n\
                   +first_call(1);\r\n+second_call(2);\r\n";
        assert_eq!(hint_of(&moved(raw)[0], '-'), Some(WHITESPACE_ONLY));
    }

    #[test]
    fn blocks_swapped_within_a_change_are_moves_even_when_reindented() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,4 +1,4 @@\n\
                   -first_call(1);\n-second_call(2);\n-third_call(3);\n-fourth_call(4);\n\
                   +    third_call(3);\n+    fourth_call(4);\n\
                   +    first_call(1);\n+    second_call(2);\n";
        let got = moved(raw);
        let hints: Vec<&str> = got[0].iter().filter_map(|r| r.2.as_deref()).collect();
        assert_eq!(
            hints,
            [
                "moved to line 3",
                "moved to line 1",
                "moved from line 3",
                "moved from line 1"
            ]
        );
    }

    /// Six re-indented calls, as `(removed, added)` lines without their signs.
    fn reindented() -> (Vec<String>, Vec<String>) {
        let calls = (1..=6).map(|i| format!("a_numbered_call_{i}(argument);"));
        let added = calls.clone().map(|call| format!("    {call}"));
        (calls.collect(), added.collect())
    }

    /// One change that removes `removed` and adds `added`.
    fn change(removed: &[String], added: &[String]) -> String {
        let mut raw = format!(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,{} +1,{} @@\n",
            removed.len(),
            added.len()
        );
        raw.extend(removed.iter().map(|line| format!("-{line}\n")));
        raw.extend(added.iter().map(|line| format!("+{line}\n")));
        raw
    }

    /// The text of each row with a hint, in order, sign first.
    fn hinted(raw: &str) -> Vec<String> {
        let got = moved(raw).remove(0);
        let rows = got.iter().filter(|r| r.2.is_some());
        rows.map(|r| format!("{}{}", r.0, r.1.trim())).collect()
    }

    #[test]
    fn an_edit_in_a_whitespace_change_leaves_one_note_on_each_side() {
        let (mut removed, mut added) = reindented();
        removed[2] = "the_old_line(1);".to_string();
        added[2] = "    the_new_line(2);".to_string();
        let raw = change(&removed, &added);
        assert_eq!(
            hinted(&raw),
            [
                "-a_numbered_call_1(argument);",
                "+a_numbered_call_1(argument);"
            ]
        );
        // The rows after the edit are still marked, and still lead somewhere.
        let files = parse(&raw);
        let index = MoveIndex::build(&files);
        let old = Anchor {
            side: Side::Old,
            line: 4,
        };
        let new = Anchor {
            side: Side::New,
            line: 4,
        };
        assert_eq!(index.file(0).target(old).unwrap().anchor, new);
        assert_eq!(index.file(0).target(new).unwrap().anchor, old);
        assert_eq!(moved(&raw)[0].len(), 10);
    }

    #[test]
    fn a_line_added_to_a_whitespace_change_leaves_one_note_on_each_side() {
        let (removed, mut added) = reindented();
        added.insert(3, "    a_brand_new_line(1);".to_string());
        assert_eq!(hinted(&change(&removed, &added)).len(), 2);
    }

    #[test]
    fn a_line_dropped_from_a_whitespace_change_leaves_one_note_on_each_side() {
        let (mut removed, added) = reindented();
        removed.insert(3, "a_line_going_away(1);".to_string());
        assert_eq!(hinted(&change(&removed, &added)).len(), 2);
    }

    #[test]
    fn three_pieces_of_a_whitespace_change_share_one_note() {
        let (mut removed, mut added) = reindented();
        removed.extend(["a_numbered_call_7(argument);".to_string()]);
        added.extend(["    a_numbered_call_7(argument);".to_string()]);
        for at in [2, 5] {
            removed[at] = format!("the_old_line({at});");
            added[at] = format!("    the_new_line({at});");
        }
        assert_eq!(hinted(&change(&removed, &added)).len(), 2);
    }

    #[test]
    fn a_piece_below_a_real_move_keeps_its_note() {
        let (mut removed, added) = reindented();
        removed.splice(3..3, FN_A.map(str::to_string));
        let mut raw = change(&removed, &added);
        raw.push_str(&file("g", &[], &FN_A));
        assert_eq!(
            hinted(&raw),
            [
                "-a_numbered_call_1(argument);",
                "-fn alpha_value() {",
                "-a_numbered_call_4(argument);",
                "+a_numbered_call_1(argument);"
            ]
        );
    }

    #[test]
    fn pieces_that_really_moved_keep_a_note_each() {
        let removed = [FN_A[0], FN_A[1], "let old = 1;", FN_B[0], FN_B[1]];
        let added = [FN_A[0], FN_A[1], "let new = 1;", FN_B[0], FN_B[1]];
        let got = moved(&file("f", &removed, &added));
        let hints: Vec<&str> = got[0].iter().filter_map(|r| r.2.as_deref()).collect();
        assert_eq!(
            hints,
            [
                "moved to line 2",
                "moved to line 5",
                "moved from line 1",
                "moved from line 4"
            ]
        );
    }

    #[test]
    fn a_whitespace_change_either_side_of_a_kept_line_keeps_both_notes() {
        let (removed, added) = reindented();
        let mut raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,7 +1,7 @@\n".to_string();
        for (lines, sign) in [(&removed[..3], '-'), (&added[..3], '+')] {
            raw.extend(lines.iter().map(|line| format!("{sign}{line}\n")));
        }
        raw.push_str(" kept\n");
        for (lines, sign) in [(&removed[3..], '-'), (&added[3..], '+')] {
            raw.extend(lines.iter().map(|line| format!("{sign}{line}\n")));
        }
        assert_eq!(hinted(&raw).len(), 4);
    }

    #[test]
    fn a_removed_row_knows_its_twin_in_the_same_change() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n\
                   -first_call(1);\n-second_call(2);\n\
                   +    first_call(1);\n+    second_call(2);\n";
        let files = parse(raw);
        let index = MoveIndex::build(&files);
        assert_eq!(index.file(0).at(1).unwrap().twin, Some(3));
        assert_eq!(index.file(0).at(3).unwrap().twin, None);
    }

    #[test]
    fn each_moved_line_points_at_the_matching_line_at_the_other_end() {
        let raw = file("a.rs", &FN_A, &[]) + &file("b.rs", &[], &FN_A);
        let files = parse(&raw);
        let index = MoveIndex::build(&files);
        let at = |side, line| Anchor { side, line };
        let target = index.file(0).target(at(Side::Old, 2)).unwrap();
        assert_eq!((target.file, target.anchor), (1, at(Side::New, 3)));
        let back = index.file(1).target(target.anchor).unwrap();
        assert_eq!((back.file, back.anchor), (0, at(Side::Old, 2)));
    }

    #[test]
    fn an_unmoved_line_has_no_target() {
        let files = parse(&file("f", &FN_A, &FN_A));
        let kept = Anchor {
            side: Side::New,
            line: 1,
        };
        assert_eq!(MoveIndex::build(&files).file(0).target(kept), None);
    }

    #[test]
    fn a_row_moved_to_another_change_has_no_twin() {
        let files = parse(&file("f", &FN_A, &FN_A));
        let index = MoveIndex::build(&files);
        assert_eq!(index.file(0).at(1).unwrap().twin, None);
    }

    #[test]
    fn one_line_is_not_a_block() {
        let line = ["a_call_long_enough_to_pass_the_character_count();"];
        assert!(moved(&file("f", &line, &line))[0].is_empty());
    }

    #[test]
    fn a_closing_brace_doesnt_count_as_a_second_line() {
        let lines = ["a_call_long_enough_to_pass_the_character_count();", "}"];
        assert!(moved(&file("f", &lines, &lines))[0].is_empty());
    }

    #[test]
    fn a_block_needs_enough_characters() {
        let lines = ["alpha;", "gamma;"];
        assert!(moved(&file("f", &lines, &lines))[0].is_empty());
    }

    #[test]
    fn a_block_doesnt_start_on_a_brace() {
        let removed = ["}", "fn alpha_value() {", "    first_call(1);"];
        let added = [
            "fn other() {}",
            "}",
            "fn alpha_value() {",
            "    first_call(1);",
        ];
        let got = moved(&file("f", &removed, &added));
        assert_eq!(
            texts(&got[0]),
            [
                "fn alpha_value() {",
                "    first_call(1);",
                "fn alpha_value() {",
                "    first_call(1);"
            ]
        );
    }

    #[test]
    fn a_brace_inside_a_block_stays_in_it() {
        let lines = ["fn alpha_value() {", "}", "fn gamma_value() {"];
        assert_eq!(moved(&file("f", &lines, &lines))[0].len(), 6);
    }

    #[test]
    fn an_edited_line_splits_a_block_and_stays_unmarked() {
        let removed = [FN_A[0], FN_A[1], "let old = 1;", FN_B[0], FN_B[1]];
        let added = [FN_A[0], FN_A[1], "let new = 1;", FN_B[0], FN_B[1]];
        let got = moved(&file("f", &removed, &added));
        assert_eq!(got[0].len(), 8);
        assert!(!texts(&got[0]).iter().any(|t| t.starts_with("let")));
        assert_eq!(got[0].iter().filter(|r| r.2.is_some()).count(), 4);
    }

    #[test]
    fn each_removed_line_goes_to_one_copy_and_the_longest_wins() {
        let removed = [FN_A[0], FN_A[1]];
        let added = [FN_A[0], FN_A[1], "other();", FN_A[0], FN_A[1], "}"];
        let raw = file("f", &removed, &added);
        let got = moved(&raw);
        assert_eq!(got[0].iter().filter(|r| r.0 == '+').count(), 2);
        assert_eq!(got[0].iter().filter(|r| r.0 == '-').count(), 2);
    }

    #[test]
    fn a_nearby_match_beats_an_equally_long_one_in_another_file() {
        let raw = file("a.rs", &[], &FN_A) + &file("b.rs", &FN_A, &FN_A);
        let got = moved(&raw);
        assert!(got[0].is_empty());
        assert_eq!(got[1].len(), 6);
    }

    #[test]
    fn a_line_with_too_many_copies_cant_start_a_block() {
        let line = "the_same_statement_repeated_over_and_over();";
        let added = vec![line; MAX_COPIES + 1];
        assert!(moved(&file("f", &[line, line], &added))[0].is_empty());
    }

    #[test]
    fn a_huge_diff_skips_detection() {
        let mut files = parse(&file("f", &FN_A, &FN_A));
        files[0].additions = MAX_CHANGED_LINES as u32 + 1;
        assert_eq!(*MoveIndex::build(&files).file(0), FileMoves::default());
    }

    #[test]
    fn a_file_past_the_end_of_the_index_has_no_moves() {
        assert_eq!(MoveIndex::default().file(9).at(0), None);
    }
}

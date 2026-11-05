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
use std::collections::{HashMap, HashSet};

use super::words::change_runs;
use crate::diff::{self, FileDiff, Row};

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
/// Changed lines in the whole diff past which detection is skipped.
const MAX_CHANGED_LINES: u64 = 100_000;
/// The hint on a block that never left its place: re-indented, realigned, or
/// given new line endings.
const WHITESPACE_ONLY: &str = "whitespace only";

/// What a moved row needs the renderer to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    /// On a block's first row, where the block went or came from.
    pub hint: Option<String>,
    /// On a removed row, the added row it matches if the two share a change, which
    /// is what side-by-side sets opposite it.
    pub twin: Option<usize>,
}

/// One file's moved rows.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileMoves {
    /// Index-aligned with the file's rows, or empty when nothing in it moved.
    marks: Vec<Option<Mark>>,
}

impl FileMoves {
    pub fn at(&self, row: usize) -> Option<&Mark> {
        self.marks.get(row)?.as_ref()
    }
}

static NO_MOVES: FileMoves = FileMoves { marks: Vec::new() };

/// Every moved row in a diff. A block can span two files, so this is built from
/// the whole file set rather than file by file.
#[derive(Debug, Default)]
pub struct MoveIndex {
    files: Vec<FileMoves>,
}

impl MoveIndex {
    pub fn build(files: &[FileDiff]) -> Self {
        let changed: u64 = files
            .iter()
            .map(|f| u64::from(f.additions) + u64::from(f.deletions))
            .sum();
        if changed > MAX_CHANGED_LINES {
            return Self::default();
        }

        let rows: Vec<Vec<Row>> = files.iter().map(diff::rows).collect();
        let (removed, added) = changed_lines(&rows);
        let blocks = claim(matches(&removed, &added), &removed, &added);

        let reordered = reordered_runs(&blocks, &removed, &added);
        let mut marks: Vec<Vec<Option<Mark>>> = rows.iter().map(|r| vec![None; r.len()]).collect();
        for block in blocks {
            let (old, new) = (&removed[block.old], &added[block.new]);
            let stayed = old.run == new.run && !reordered.contains(&old.run);
            let (to, from) = if stayed {
                (WHITESPACE_ONLY.to_string(), WHITESPACE_ONLY.to_string())
            } else {
                hints(old, new, &rows, files)
            };
            for at in 0..block.len {
                let (old, new) = (&removed[block.old + at], &added[block.new + at]);
                marks[old.file][old.row] = Some(Mark {
                    hint: (at == 0).then(|| to.clone()),
                    twin: (old.run == new.run).then_some(new.row),
                });
                marks[new.file][new.row] = Some(Mark {
                    hint: (at == 0).then(|| from.clone()),
                    twin: None,
                });
            }
        }
        let files = marks
            .into_iter()
            .map(|marks| {
                let any = marks.iter().any(Option::is_some);
                FileMoves {
                    marks: if any { marks } else { Vec::new() },
                }
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
}

/// Every removed and added line in the diff, in order.
fn changed_lines(files: &[Vec<Row>]) -> (Vec<Line>, Vec<Line>) {
    let mut keys: HashMap<String, u32> = HashMap::new();
    let mut line = |file: usize, row: usize, run: usize| {
        let text = &files[file][row].text;
        let bare: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let alnum = bare.chars().filter(|c| c.is_alphanumeric()).count();
        let next = keys.len() as u32;
        Line {
            file,
            row,
            run,
            key: *keys.entry(bare).or_insert(next),
            alnum,
        }
    };

    let (mut removed, mut added) = (Vec::new(), Vec::new());
    let mut run = 0;
    for (file, rows) in files.iter().enumerate() {
        for (old, new) in change_runs(rows) {
            removed.extend(old.into_iter().map(|row| line(file, row, run)));
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

/// What to say above a block's removed and added halves, given their first lines.
fn hints(old: &Line, new: &Line, rows: &[Vec<Row>], files: &[FileDiff]) -> (String, String) {
    let there = |verb: &str, line: &Line, number: Option<u32>| {
        let number = number.unwrap_or_default();
        if old.file == new.file {
            format!("moved {verb} line {number}")
        } else {
            format!("moved {verb} {}:{number}", files[line.file].path())
        }
    };
    (
        there("to", new, rows[new.file][new.row].new),
        there("from", old, rows[old.file][old.row].old),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{RowKind, parse};

    const FN_A: [&str; 3] = ["fn alpha_value() {", "    first_call(1);", "}"];
    const FN_B: [&str; 3] = ["fn beta_value() {", "    second_call(2);", "}"];

    /// A file whose only hunk removes `removed` from line 1 and adds `added`,
    /// separated from each other by a kept line.
    fn file(name: &str, removed: &[&str], added: &[&str]) -> String {
        let mut raw = format!(
            "diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n@@ -1,{} +1,{} @@\n",
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

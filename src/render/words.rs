//! Word-level emphasis: which part of a changed line actually changed.
//!
//! git only says a line was removed and another added. Lines that are recognisably
//! the same line edited get paired, and a token diff of each pair marks the bytes
//! that differ. A line with no partner gets no emphasis, because all of it changed.

use std::ops::Range;

use similar::{Algorithm, DiffOp, capture_diff_slices};

use super::highlight::MAX_LINE_BYTES;
use super::moves::FileMoves;
use crate::diff::{Row, RowKind};

/// How much of two lines' non-blank text must match to call them one edited line.
const MIN_SIMILARITY: f32 = 0.4;
/// How many added lines ahead to look for a removed line's partner, which bounds
/// the work on a large rewrite to a few token diffs per line.
const PARTNER_WINDOW: usize = 8;

/// What the word pass worked out about a file's changed rows.
#[derive(Debug, Default)]
pub struct Changes {
    /// Byte ranges to emphasize in each row, index-aligned with the rows.
    pub emphasis: Vec<Vec<Range<usize>>>,
    /// For a removed row, the added row to set opposite it, index-aligned: the
    /// row it's an edit of, or the one it moved to within the same change.
    pub partner: Vec<Option<usize>>,
}

/// Pair up the edited lines in `rows` and mark what changed in each. A moved row
/// isn't an edit of anything, so it stays out of the pairing.
pub fn changes(rows: &[Row], moves: &FileMoves) -> Changes {
    let mut out = Changes {
        emphasis: vec![Vec::new(); rows.len()],
        partner: (0..rows.len())
            .map(|row| moves.at(row).and_then(|mark| mark.twin))
            .collect(),
    };
    for (mut removed, mut added) in change_runs(rows) {
        removed.retain(|&row| moves.at(row).is_none());
        added.retain(|&row| moves.at(row).is_none());
        let mut next = 0;
        for &old_row in &removed {
            let window = added.iter().enumerate().skip(next).take(PARTNER_WINDOW);
            let best = window
                .filter_map(|(at, &new_row)| {
                    let edit = compare(&rows[old_row].text, &rows[new_row].text)?;
                    (edit.similarity >= MIN_SIMILARITY).then_some((at, new_row, edit))
                })
                .max_by(|a, b| a.2.similarity.total_cmp(&b.2.similarity));
            if let Some((at, new_row, edit)) = best {
                out.emphasis[old_row] = edit.old;
                out.emphasis[new_row] = edit.new;
                out.partner[old_row] = Some(new_row);
                next = at + 1;
            }
        }
    }
    out
}

/// Each block of removed rows and the added rows straight after it, as row
/// indices. A `\ No newline` marker can sit between the two, or inside either.
/// Either side can be empty, for a pure addition or deletion.
pub fn change_runs(rows: &[Row]) -> Vec<(Vec<usize>, Vec<usize>)> {
    let mut runs = Vec::new();
    let (mut removed, mut added) = (Vec::new(), Vec::new());
    for (i, row) in rows.iter().enumerate() {
        match row.kind {
            RowKind::Removed if added.is_empty() => removed.push(i),
            RowKind::Added => added.push(i),
            RowKind::NoNewline => {}
            _ => {
                if !removed.is_empty() || !added.is_empty() {
                    runs.push((std::mem::take(&mut removed), std::mem::take(&mut added)));
                }
                if row.kind == RowKind::Removed {
                    removed.push(i);
                }
            }
        }
    }
    if !removed.is_empty() || !added.is_empty() {
        runs.push((removed, added));
    }
    runs
}

struct Edit {
    similarity: f32,
    old: Vec<Range<usize>>,
    new: Vec<Range<usize>>,
}

/// Token-diff two lines. `None` when either is too long to be worth it.
fn compare(old: &str, new: &str) -> Option<Edit> {
    if old.len() > MAX_LINE_BYTES || new.len() > MAX_LINE_BYTES {
        return None;
    }
    let (old_tokens, new_tokens) = (tokens(old), tokens(new));
    let old_text: Vec<&str> = old_tokens.iter().map(|t| &old[t.clone()]).collect();
    let new_text: Vec<&str> = new_tokens.iter().map(|t| &new[t.clone()]).collect();

    let mut matched = 0;
    let (mut old_ranges, mut new_ranges) = (Vec::new(), Vec::new());
    for op in capture_diff_slices(Algorithm::Myers, &old_text, &new_text) {
        let (old_span, new_span) = (op.old_range(), op.new_range());
        if matches!(op, DiffOp::Equal { .. }) {
            matched += old_text[old_span].iter().map(|t| ink(t)).sum::<usize>();
            continue;
        }
        push_span(&mut old_ranges, &old_tokens, old_span);
        push_span(&mut new_ranges, &new_tokens, new_span);
    }

    let total = ink(old) + ink(new);
    let similarity = if total == 0 {
        1.0
    } else {
        2.0 * matched as f32 / total as f32
    };
    Some(Edit {
        similarity,
        old: bridge_blanks(old_ranges, old),
        new: bridge_blanks(new_ranges, new),
    })
}

fn push_span(ranges: &mut Vec<Range<usize>>, tokens: &[Range<usize>], span: Range<usize>) {
    if let (Some(first), Some(last)) = (tokens.get(span.start), tokens[..span.end].last())
        && !span.is_empty()
    {
        ranges.push(first.start..last.end);
    }
}

/// Bytes that aren't whitespace. Similarity is measured on these, or two
/// unrelated lines would look alike for sharing an indent and some spaces.
fn ink(text: &str) -> usize {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .map(char::len_utf8)
        .sum()
}

/// Join ranges separated only by whitespace, so `foo bar` replaced by `baz qux`
/// reads as one edit rather than two with a gap.
fn bridge_blanks(ranges: Vec<Range<usize>>, text: &str) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match out.last_mut() {
            Some(last) if text[last.end..range.start].trim().is_empty() => last.end = range.end,
            _ => out.push(range),
        }
    }
    out
}

/// Split a line into words, whitespace runs, and single punctuation characters,
/// as byte ranges. `foo_bar` is one token, `foo.bar` three.
fn tokens(text: &str) -> Vec<Range<usize>> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Blank,
        Mark,
    }
    let class = |c: char| match c {
        c if c.is_alphanumeric() || c == '_' => Class::Word,
        c if c.is_whitespace() => Class::Blank,
        _ => Class::Mark,
    };

    let mut out: Vec<Range<usize>> = Vec::new();
    let mut last = None;
    for (at, c) in text.char_indices() {
        let current = class(c);
        match out.last_mut() {
            Some(token) if last.as_ref() == Some(&current) && current != Class::Mark => {
                token.end = at + c.len_utf8();
            }
            _ => out.push(at..at + c.len_utf8()),
        }
        last = Some(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{parse, rows};

    fn emphasized(hunk: &str) -> Vec<Vec<String>> {
        let raw = format!("diff --git a/f b/f\n--- a/f\n+++ b/f\n{hunk}");
        let rows = rows(&parse(&raw).remove(0));
        changes(&rows, &FileMoves::default())
            .emphasis
            .iter()
            .zip(&rows)
            .map(|(ranges, row)| {
                ranges
                    .iter()
                    .map(|r| row.text[r.clone()].to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn marks_only_the_token_that_changed() {
        let got = emphasized("@@ -1 +1 @@\n-let total = sum(items);\n+let total = sum(values);\n");
        assert_eq!(got[1], ["items"]);
        assert_eq!(got[2], ["values"]);
    }

    #[test]
    fn unrelated_lines_stay_unmarked() {
        let got = emphasized("@@ -1 +1 @@\n-let total = sum(items);\n+println!(\"done\");\n");
        assert!(got.iter().all(Vec::is_empty));
    }

    #[test]
    fn shared_indentation_alone_does_not_pair_lines() {
        let got = emphasized("@@ -1 +1 @@\n-        alpha\n+        omega\n");
        assert!(got.iter().all(Vec::is_empty));
    }

    #[test]
    fn pairs_each_line_with_its_own_partner_not_the_next_in_line() {
        let got = emphasized(
            "@@ -1,2 +1,3 @@\n-let first = 1;\n-let second = 2;\n\
             +println!(\"new\");\n+let first = 10;\n+let second = 20;\n",
        );
        assert_eq!(got[1], ["1"]);
        assert_eq!(got[2], ["2"]);
        assert!(got[3].is_empty());
        assert_eq!(got[4], ["10"]);
        assert_eq!(got[5], ["20"]);
    }

    #[test]
    fn neighbouring_edits_split_by_a_blank_read_as_one() {
        let got = emphasized("@@ -1 +1 @@\n-call(alpha beta, keep);\n+call(gamma delta, keep);\n");
        assert_eq!(got[1], ["alpha beta"]);
        assert_eq!(got[2], ["gamma delta"]);
    }

    #[test]
    fn pairs_across_a_missing_newline_marker() {
        let got = emphasized(
            "@@ -1 +1 @@\n-let a = 1;\n\\ No newline at end of file\n\
             +let a = 2;\n\\ No newline at end of file\n",
        );
        assert_eq!(got[1], ["1"]);
        assert_eq!(got[3], ["2"]);
    }

    #[test]
    fn multibyte_text_splits_on_character_boundaries() {
        let got = emphasized("@@ -1 +1 @@\n-let näme = \"日本\";\n+let näme = \"日本語\";\n");
        assert_eq!(got[1], ["日本"]);
        assert_eq!(got[2], ["日本語"]);
    }

    #[test]
    fn a_partner_links_the_removed_row_to_the_added_one() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
                   @@ -1,2 +1,2 @@\n-let a = 1;\n-gone\n+let a = 2;\n+other\n";
        let rows = rows(&parse(raw).remove(0));
        assert_eq!(
            changes(&rows, &FileMoves::default()).partner,
            [None, Some(3), None, None, None]
        );
    }

    #[test]
    fn a_pure_addition_is_a_run_of_its_own() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1,2 @@\n keep\n+new\n";
        let rows = rows(&parse(raw).remove(0));
        assert_eq!(change_runs(&rows), [(vec![], vec![2])]);
    }

    #[test]
    fn a_later_block_does_not_pair_with_an_earlier_one() {
        let got = emphasized("@@ -1,3 +1,3 @@\n-let a = 1;\n keep\n+let a = 2;\n");
        assert!(got.iter().all(Vec::is_empty));
    }
}

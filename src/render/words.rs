//! Word-level emphasis: which part of a changed line actually changed.
//!
//! git only says a line was removed and another added. Lines that are recognisably
//! the same line edited get paired, and a token diff of each pair marks the bytes
//! that differ. A line with no partner gets no emphasis, because all of it changed.
//!
//! A token diff is exact but noisy, so it's tidied for reading. A mark inside a
//! token the parser knows widens to all of it, edits split only by a bracket or a
//! dot become one, and formatting changed beside a real edit goes unmarked.
//!
//! The diff itself never sees the parser's tokens, because two sides can parse the
//! same text differently, which would make unchanged code look changed.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use similar::{Algorithm, DiffOp, capture_diff_slices};

use super::highlight::MAX_LINE_BYTES;
use super::moves::FileMoves;
use super::tokens::{self, Tokens};
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
/// isn't an edit of anything, so it stays out of the pairing. Stops short once
/// `cancelled` is set.
///
/// `tokens` only shapes the marks: rows pair up the same way with none, which is
/// what lets a file be laid out in its final shape before it has been parsed.
pub fn changes(
    rows: &[Row],
    tokens: &[Tokens],
    moves: &FileMoves,
    cancelled: &AtomicBool,
) -> Changes {
    let mut out = Changes {
        emphasis: vec![Vec::new(); rows.len()],
        partner: (0..rows.len())
            .map(|row| moves.at(row).and_then(|mark| mark.twin))
            .collect(),
    };
    let line = |row: usize| Line::new(&rows[row].text, &tokens[row]);
    for (mut removed, mut added) in change_runs(rows) {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        removed.retain(|&row| moves.at(row).is_none());
        added.retain(|&row| moves.at(row).is_none());
        let lines: Vec<Line> = added.iter().map(|&row| line(row)).collect();
        let mut next = 0;
        for &old_row in &removed {
            let old = line(old_row);
            let window = lines.iter().enumerate().skip(next).take(PARTNER_WINDOW);
            let best = window
                .filter_map(|(at, new)| {
                    let edit = compare(&old, new)?;
                    (edit.similarity >= MIN_SIMILARITY).then_some((at, edit))
                })
                .max_by(|a, b| a.1.similarity.total_cmp(&b.1.similarity));
            if let Some((at, edit)) = best {
                let (old_marks, new_marks) = marks(&old, &lines[at], edit.spans);
                out.emphasis[old_row] = old_marks;
                out.emphasis[added[at]] = new_marks;
                out.partner[old_row] = Some(added[at]);
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

/// A row's text cut into tokens, with what its syntax says about them.
struct Line<'a> {
    text: &'a str,
    syntax: &'a Tokens,
    /// Token byte ranges, empty for a line too long to compare.
    ranges: Vec<Range<usize>>,
    words: Vec<&'a str>,
}

impl<'a> Line<'a> {
    fn new(text: &'a str, syntax: &'a Tokens) -> Self {
        let ranges = if text.len() > MAX_LINE_BYTES {
            Vec::new()
        } else {
            tokens::split(text)
        };
        let words = ranges.iter().map(|r| &text[r.clone()]).collect();
        Self {
            text,
            syntax,
            ranges,
            words,
        }
    }

    /// Whether the cut before token `at` falls inside a whole token.
    fn cuts_token(&self, at: usize) -> bool {
        self.ranges.get(at).is_some_and(|token| {
            let cut = token.start;
            self.syntax
                .whole
                .iter()
                .any(|t| t.start < cut && cut < t.end)
        })
    }

    /// Whether every token in `span` is spacing that's only formatting: code's,
    /// or the indent before a comment or string.
    fn is_formatting(&self, span: &Range<usize>) -> bool {
        self.ranges[span.clone()].iter().all(|token| {
            let literal = |r: &Range<usize>| r.contains(&token.start);
            self.text[token.clone()].trim().is_empty()
                && (token.start == 0 || !self.syntax.literal.iter().any(literal))
        })
    }
}

/// Where two lines differ, as token indices into each.
#[derive(Debug, Clone)]
struct Span {
    old: Range<usize>,
    new: Range<usize>,
}

struct Edit {
    similarity: f32,
    spans: Vec<Span>,
}

/// Token-diff two lines. `None` when either is too long to be worth it.
fn compare(old: &Line, new: &Line) -> Option<Edit> {
    if old.text.len() > MAX_LINE_BYTES || new.text.len() > MAX_LINE_BYTES {
        return None;
    }
    let mut matched = 0;
    let mut spans = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, &old.words, &new.words) {
        let (old_span, new_span) = (op.old_range(), op.new_range());
        if matches!(op, DiffOp::Equal { .. }) {
            matched += old.words[old_span].iter().map(|t| ink(t)).sum::<usize>();
        } else {
            spans.push(Span {
                old: old_span,
                new: new_span,
            });
        }
    }

    let total = ink(old.text) + ink(new.text);
    let similarity = if total == 0 {
        1.0
    } else {
        2.0 * matched as f32 / total as f32
    };
    Some(Edit { similarity, spans })
}

/// The byte ranges to emphasize in two paired lines, from the spans that differ.
fn marks(old: &Line, new: &Line, spans: Vec<Span>) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let spans = without_formatting(spans, old, new);
    let spans = join_islands(whole_tokens(spans, old, new), old, new);
    let ranges = |line: &Line, side: fn(&Span) -> &Range<usize>| {
        let ranges = spans.iter().filter_map(|span| {
            let tokens = &line.ranges[side(span).clone()];
            Some(tokens.first()?.start..tokens.last()?.end)
        });
        bridge_blanks(ranges.collect(), line.text)
    };
    (ranges(old, |s| &s.old), ranges(new, |s| &s.new))
}

/// Drop the spacing a span only changed, such as a new indent, when something
/// else on the line changed too. If nothing else did, the spacing is the edit and
/// stays marked.
fn without_formatting(spans: Vec<Span>, old: &Line, new: &Line) -> Vec<Span> {
    let real: Vec<Span> = spans
        .iter()
        .filter(|s| !(old.is_formatting(&s.old) && new.is_formatting(&s.new)))
        .cloned()
        .collect();
    if real.is_empty() {
        return spans;
    }
    let both_ways = |span: &Span| !span.old.is_empty() && !span.new.is_empty();
    let ends_in_spacing = |span: &Span| {
        both_ways(span)
            && old.is_formatting(&(span.old.end - 1..span.old.end))
            && new.is_formatting(&(span.new.end - 1..span.new.end))
    };
    let starts_with_spacing = |span: &Span| {
        both_ways(span)
            && old.is_formatting(&(span.old.start..span.old.start + 1))
            && new.is_formatting(&(span.new.start..span.new.start + 1))
    };
    let trimmed = |mut span: Span| {
        while ends_in_spacing(&span) {
            span.old.end -= 1;
            span.new.end -= 1;
        }
        while starts_with_spacing(&span) {
            span.old.start += 1;
            span.new.start += 1;
        }
        span
    };
    real.into_iter().map(trimmed).collect()
}

/// Widen each span until neither end falls inside a whole token, so `==` becoming
/// `!=` is marked whole and not just its first character. The tokens it grows over
/// matched, so each step moves both sides together.
fn whole_tokens(mut spans: Vec<Span>, old: &Line, new: &Line) -> Vec<Span> {
    let mut floor = (0, 0);
    for at in 0..spans.len() {
        let ceiling = spans
            .get(at + 1)
            .map_or((old.words.len(), new.words.len()), |next| {
                (next.old.start, next.new.start)
            });
        let span = &mut spans[at];
        while span.old.start > floor.0
            && span.new.start > floor.1
            && (old.cuts_token(span.old.start) || new.cuts_token(span.new.start))
        {
            span.old.start -= 1;
            span.new.start -= 1;
        }
        while span.old.end < ceiling.0
            && span.new.end < ceiling.1
            && (old.cuts_token(span.old.end) || new.cuts_token(span.new.end))
        {
            span.old.end += 1;
            span.new.end += 1;
        }
        floor = (span.old.end, span.new.end);
    }
    spans
}

/// Merge spans separated only by punctuation, such as the `(` of `sum(items)`
/// becoming `total(values)`. A match that small is a coincidence of the two
/// lines' shapes, and marking around it scatters one edit into several.
fn join_islands(spans: Vec<Span>, old: &Line, new: &Line) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    let mut spans = spans.into_iter().peekable();
    while let Some(span) = spans.next() {
        let joined = out.last().and_then(|last| {
            let limit = spans.peek().map(|next| (next.old.start, next.new.start));
            join(last, &span, limit, old, new)
        });
        match joined {
            Some(whole) => *out.last_mut().expect("a join has a span before it") = whole,
            None => out.push(span),
        }
    }
    out
}

/// `first` and `second` as one span, if only an island lies between them. Any
/// bracket that leaves open has to close straight after, and the closer joins in,
/// or the mark would end inside the brackets. `limit` is where the next span starts.
fn join(
    first: &Span,
    second: &Span,
    limit: Option<(usize, usize)>,
    old: &Line,
    new: &Line,
) -> Option<Span> {
    if !is_island(&old.words[first.old.end..second.old.start])
        || !is_island(&new.words[first.new.end..second.new.start])
    {
        return None;
    }
    let owed = unclosed(&old.words[first.old.start..second.old.end]);
    if owed != unclosed(&new.words[first.new.start..second.new.end]) {
        return None;
    }
    let (old_end, new_end) = (second.old.end + owed.len(), second.new.end + owed.len());
    let (old_limit, new_limit) = limit.unwrap_or((old.words.len(), new.words.len()));
    if old_end > old_limit
        || new_end > new_limit
        || !owed.iter().rev().eq(&old.words[second.old.end..old_end])
        || !owed.iter().rev().eq(&new.words[second.new.end..new_end])
    {
        return None;
    }
    Some(Span {
        old: first.old.start..old_end,
        new: first.new.start..new_end,
    })
}

/// Whether `words` are small enough to mean nothing as a match: punctuation with
/// no spaces, which aren't closing anything or delimiting a string or generics.
fn is_island(words: &[&str]) -> bool {
    !words.is_empty()
        && words.iter().all(|word| {
            !word.contains(|c: char| {
                c.is_alphanumeric() || c.is_whitespace() || ")]}<>\"'`".contains(c)
            })
        })
}

/// The closers `words` still owes, innermost last.
fn unclosed<'a>(words: &[&str]) -> Vec<&'a str> {
    let mut open = Vec::new();
    for &word in words {
        match word {
            "(" => open.push(")"),
            "[" => open.push("]"),
            "{" => open.push("}"),
            ")" | "]" | "}" if open.last() == Some(&word) => {
                open.pop();
            }
            _ => {}
        }
    }
    open
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{parse, rows};
    use crate::render::highlight::highlight;
    use crate::theme::DiffTheme;

    /// The word pass over `hunk` in a file named `name`.
    fn run(name: &str, hunk: &str) -> (Vec<Row>, Changes) {
        let raw = format!("diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n{hunk}");
        let file = parse(&raw).remove(0);
        let rows = rows(&file);
        let cancelled = AtomicBool::new(false);
        let syntax = highlight(&file, &rows, DiffTheme::GitHubDark, &[], &cancelled);
        let changes = changes(&rows, &syntax.tokens, &FileMoves::default(), &cancelled);
        (rows, changes)
    }

    fn emphasized(hunk: &str) -> Vec<Vec<String>> {
        emphasized_in("a.rs", hunk)
    }

    fn emphasized_in(name: &str, hunk: &str) -> Vec<Vec<String>> {
        let (rows, changes) = run(name, hunk);
        changes
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
        let (_, changes) = run(
            "f",
            "@@ -1,2 +1,2 @@\n-let a = 1;\n-gone\n+let a = 2;\n+other\n",
        );
        assert_eq!(changes.partner, [None, Some(3), None, None, None]);
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

    /// The marks on the removed and added lines of a one-line change to `name`.
    fn edit(name: &str, old: &str, new: &str) -> (Vec<String>, Vec<String>) {
        let mut got = emphasized_in(name, &format!("@@ -1 +1 @@\n-{old}\n+{new}\n"));
        (got.remove(1), got.remove(1))
    }

    #[test]
    fn an_operator_changes_as_a_whole() {
        let (old, new) = edit("a.rs", "if a == b {", "if a != b {");
        assert_eq!((old, new), (vec!["==".into()], vec!["!=".into()]));
    }

    #[test]
    fn an_operator_that_grew_is_marked_on_both_sides() {
        let (old, new) = edit("a.rs", "if a > b {", "if a >= b {");
        assert_eq!((old, new), (vec![">".into()], vec![">=".into()]));
    }

    #[test]
    fn a_number_changes_as_a_whole() {
        let (old, new) = edit("a.rs", "let a = 1.5;", "let a = 1.6;");
        assert_eq!((old, new), (vec!["1.5".into()], vec!["1.6".into()]));
    }

    #[test]
    fn without_a_language_marks_stay_on_character_classes() {
        let (old, new) = edit("notes.xyz", "a == b", "a != b");
        assert_eq!((old, new), (vec!["=".into()], vec!["!".into()]));
    }

    #[test]
    fn text_that_parses_differently_on_each_side_is_not_a_change() {
        let (old, new) = edit(
            "Makefile",
            "\tPATH=x $(PYTHON) tests/run.py",
            "\tPATH=x poetry run python tests/run.py",
        );
        assert_eq!(
            (old, new),
            (vec!["$(PYTHON)".into()], vec!["poetry run python".into()])
        );
    }

    #[test]
    fn a_call_that_changed_entirely_is_one_mark() {
        let (old, new) = edit("a.rs", "let a = sum(items);", "let a = total(values);");
        assert_eq!(
            (old, new),
            (vec!["sum(items)".into()], vec!["total(values)".into()])
        );
    }

    #[test]
    fn a_path_that_changed_entirely_is_one_mark() {
        let (old, new) = edit(
            "a.rs",
            "let a = keep + old.path;",
            "let a = keep + new.road;",
        );
        assert_eq!(
            (old, new),
            (vec!["old.path".into()], vec!["new.road".into()])
        );
    }

    #[test]
    fn a_mark_never_stops_inside_brackets() {
        let (old, _) = edit("a.rs", "foo(a, keep, b);", "bar(c, keep, d);");
        assert_eq!(old, ["foo", "a", "b"]);
    }

    #[test]
    fn empty_brackets_that_matched_stay_unmarked() {
        let (old, new) = edit("a.rs", "keep.collect::<V>();", "rows.collect(),");
        assert_eq!(old, ["keep", "::<V>", ";"]);
        assert_eq!(new, ["rows", ","]);
    }

    #[test]
    fn neighbouring_strings_keep_their_quotes_unmarked() {
        let (old, new) = edit("a.rs", "let v = [\"a\", \"b\"];", "let v = [\"c\", \"d\"];");
        assert_eq!(
            (old, new),
            (vec!["a".into(), "b".into()], vec!["c".into(), "d".into()])
        );
    }

    #[test]
    fn a_commented_out_line_marks_only_the_comment_marker() {
        let (old, new) = edit("a.rs", "foo(x);", "// foo(x);");
        assert!(old.is_empty());
        assert_eq!(new, ["// "]);
    }

    #[test]
    fn a_new_indent_goes_unmarked_beside_a_real_edit() {
        let (old, new) = edit("a.rs", "let a = 1;", "    let a = 2;");
        assert_eq!((old, new), (vec!["1".into()], vec!["2".into()]));
    }

    #[test]
    fn a_new_indent_is_marked_when_nothing_else_changed() {
        let (old, new) = edit("a.rs", "let a = 1;", "    let a = 1;");
        assert!(old.is_empty());
        assert_eq!(new, ["    "]);
    }

    #[test]
    fn realigned_code_goes_unmarked_beside_a_real_edit() {
        let (old, new) = edit("a.toml", "a = 1 # note", "a = 2   # note");
        assert_eq!((old, new), (vec!["1".into()], vec!["2".into()]));
    }

    #[test]
    fn spacing_inside_a_string_stays_marked_beside_a_real_edit() {
        let (old, new) = edit("a.rs", "let a = \"x y\";", "let b = \"x  y\";");
        assert_eq!(
            (old, new),
            (vec!["a".into(), " ".into()], vec!["b".into(), "  ".into()])
        );
    }
}

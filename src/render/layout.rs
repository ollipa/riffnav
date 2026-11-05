//! Turning a file's rows into styled, width-filled lines.

use std::ops::Range;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use unicode_width::UnicodeWidthChar;

use super::highlight::Run;
use super::moves::FileMoves;
use super::words::{Changes, change_runs};
use crate::comment::LineMap;
use crate::comment::anchor::LineNumbers;
use crate::diff::{self, FileDiff, FileStatus, Row, RowKind};
use crate::theme::{DiffPalette, DiffTheme};

/// Width of a line-number field.
const NUMBER: usize = 5;
/// Columns the unified gutter takes: two number fields and their separators.
const GUTTER: usize = NUMBER * 2 + 2;
/// Columns each side-by-side panel's gutter takes.
const PANEL_GUTTER: usize = NUMBER + 1;
/// Closes the left and right panels' number fields.
const LEFT_DELIM: char = '⋮';
const RIGHT_DELIM: char = '│';
/// Ends a side-by-side row that continues on the next.
pub const WRAP_MARK: char = '↵';
/// Starts the row saying where a moved block went.
pub const MOVE_MARK: char = '⇄';

/// What the render passes worked out about a file, index-aligned with `rows`.
pub struct Content<'a> {
    pub rows: &'a [Row],
    pub syntax: &'a [Vec<Run>],
    pub changes: &'a Changes,
    pub moves: &'a FileMoves,
}

pub fn unified(
    file: &FileDiff,
    content: &Content<'_>,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
) -> (Text<'static>, LineMap) {
    let palette = theme.palette();
    let width = usize::from(width);
    let tab = usize::from(tab_width.max(1));

    let mut lines = file_header(file, width, &palette);
    let mut numbers = vec![LineNumbers::none(); lines.len()];
    for (i, row) in content.rows.iter().enumerate() {
        let mark = content.moves.at(i);
        let line = match row.kind {
            RowKind::HunkHeader { new_start } => {
                let header = hunk_header(new_start, &row.text, width, tab, &palette);
                numbers.resize(numbers.len() + header.len(), LineNumbers::none());
                lines.extend(header);
                continue;
            }
            RowKind::NoNewline => no_newline(&row.text, &palette),
            RowKind::Context | RowKind::Added | RowKind::Removed => {
                if let Some(hint) = mark.and_then(|m| m.hint.as_deref()) {
                    lines.push(move_hint(hint, width, &palette));
                    numbers.push(LineNumbers::none());
                }
                let (syntax, emphasis) = (&content.syntax[i], &content.changes.emphasis[i]);
                code_line(row, syntax, emphasis, mark.is_some(), width, tab, &palette)
            }
        };
        lines.push(line);
        numbers.push(LineNumbers {
            old: row.old,
            new: row.new,
        });
    }
    (Text::from(lines), LineMap::from_numbers(numbers))
}

/// One side of a side-by-side row.
struct Cell {
    number: Option<u32>,
    kind: RowKind,
    moved: bool,
    /// The text, already wrapped to the panel.
    rows: Vec<Vec<Span<'static>>>,
}

/// Geometry and styling shared by every side-by-side row.
struct Panels<'a> {
    left: usize,
    right: usize,
    tab: usize,
    palette: &'a DiffPalette,
    content: &'a Content<'a>,
}

pub fn side_by_side(
    file: &FileDiff,
    content: &Content<'_>,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
) -> (Text<'static>, LineMap) {
    let palette = theme.palette();
    let width = usize::from(width);
    let left = width / 2;
    let panels = Panels {
        left,
        right: width - left,
        tab: usize::from(tab_width.max(1)),
        palette: &palette,
        content,
    };
    let rows = content.rows;

    let mut lines = file_header(file, width, &palette);
    let mut numbers = vec![LineNumbers::none(); lines.len()];
    let mut runs = change_runs(rows).into_iter().peekable();
    let mut i = 0;
    while i < rows.len() {
        let row = &rows[i];
        if let Some((removed, added)) =
            runs.next_if(|(removed, added)| removed.first().or(added.first()) == Some(&i))
        {
            let last = removed.iter().chain(&added).max().copied().unwrap_or(i);
            for (old, new) in aligned(&removed, &added, &content.changes.partner) {
                panels.emit(old, new, &mut lines, &mut numbers);
            }
            i = last + 1;
            // The marker after the run's last row was emitted with that row.
            i += usize::from(rows.get(i).is_some_and(|r| r.kind == RowKind::NoNewline));
            continue;
        }
        match row.kind {
            RowKind::HunkHeader { new_start } => {
                let header = hunk_header(new_start, &row.text, width, panels.tab, &palette);
                numbers.resize(numbers.len() + header.len(), LineNumbers::none());
                lines.extend(header);
            }
            RowKind::Context => panels.emit(Some(i), Some(i), &mut lines, &mut numbers),
            // Only reached for a marker with no code row in front of it to carry
            // it, which git doesn't write.
            _ => {}
        }
        i += 1;
        if row.kind == RowKind::Context && rows.get(i).is_some_and(|r| r.kind == RowKind::NoNewline)
        {
            i += 1;
        }
    }
    (Text::from(lines), LineMap::from_numbers(numbers))
}

/// The rows of one change, laid out opposite each other. A removed row and its
/// partner share a row; what's left between partners is zipped together in order,
/// so a rewrite takes as few rows as it can. Rows only read downwards, so a partner
/// above one already placed, as lines swapped by a move can be, is zipped instead.
fn aligned(
    removed: &[usize],
    added: &[usize],
    partner: &[Option<usize>],
) -> Vec<(Option<usize>, Option<usize>)> {
    let mut out = Vec::new();
    let (mut old_from, mut new_from) = (0, 0);
    for (at, &old) in removed.iter().enumerate() {
        let Some(new) = partner[old] else { continue };
        let Some(skipped) = added[new_from..].iter().position(|&a| a == new) else {
            continue;
        };
        let new_at = new_from + skipped;
        zip_into(&mut out, &removed[old_from..at], &added[new_from..new_at]);
        out.push((Some(old), Some(new)));
        (old_from, new_from) = (at + 1, new_at + 1);
    }
    zip_into(&mut out, &removed[old_from..], &added[new_from..]);
    out
}

fn zip_into(out: &mut Vec<(Option<usize>, Option<usize>)>, old: &[usize], new: &[usize]) {
    for i in 0..old.len().max(new.len()) {
        out.push((old.get(i).copied(), new.get(i).copied()));
    }
}

impl Panels<'_> {
    /// Draw the rows at `old` and `new` opposite each other, plus any `\ No
    /// newline` markers that follow them.
    fn emit(
        &self,
        old: Option<usize>,
        new: Option<usize>,
        lines: &mut Vec<Line<'static>>,
        numbers: &mut Vec<LineNumbers>,
    ) {
        let rows = self.content.rows;
        let hint = |at: Option<usize>| {
            let hint = self.content.moves.at(at?)?.hint.as_deref()?;
            Some(format!("{MOVE_MARK} {hint}"))
        };
        self.notes(hint(old), hint(new), lines, numbers);

        let left = old.map(|i| self.cell(i, rows[i].old, self.left));
        let right = new.map(|i| self.cell(i, rows[i].new, self.right));
        let height = [&left, &right]
            .iter()
            .filter_map(|cell| cell.as_ref().map(|c| c.rows.len()))
            .max()
            .unwrap_or(1);
        for at in 0..height {
            let mut spans = self.side(left.as_ref(), at, self.left, LEFT_DELIM);
            spans.extend(self.side(right.as_ref(), at, self.right, RIGHT_DELIM));
            lines.push(Line::from(spans));
            numbers.push(if at == 0 {
                LineNumbers {
                    old: left.as_ref().and_then(|c| c.number),
                    new: right.as_ref().and_then(|c| c.number),
                }
            } else {
                LineNumbers::none()
            });
        }

        let marker = |at: Option<usize>| {
            let next = rows.get(at? + 1)?;
            (next.kind == RowKind::NoNewline).then(|| format!("⌀ {}", next.text))
        };
        self.notes(marker(old), marker(new), lines, numbers);
    }

    /// A row of unnumbered remarks, if either side has one.
    fn notes(
        &self,
        old: Option<String>,
        new: Option<String>,
        lines: &mut Vec<Line<'static>>,
        numbers: &mut Vec<LineNumbers>,
    ) {
        if old.is_some() || new.is_some() {
            let mut spans = self.note_side(old, self.left, LEFT_DELIM);
            spans.extend(self.note_side(new, self.right, RIGHT_DELIM));
            lines.push(Line::from(spans));
            numbers.push(LineNumbers::none());
        }
    }

    fn cell(&self, row: usize, number: Option<u32>, panel: usize) -> Cell {
        let moved = self.content.moves.at(row).is_some();
        let (spans, _) = code_spans(
            &self.content.rows[row],
            &self.content.syntax[row],
            &self.content.changes.emphasis[row],
            moved,
            self.tab,
            self.palette,
        );
        Cell {
            number,
            kind: self.content.rows[row].kind,
            moved,
            rows: wrap(spans, panel.saturating_sub(PANEL_GUTTER), self.palette),
        }
    }

    /// One panel's share of a screen row: the number on the cell's first row,
    /// then the text, then padding out to the panel's edge.
    fn side(
        &self,
        cell: Option<&Cell>,
        at: usize,
        panel: usize,
        delim: char,
    ) -> Vec<Span<'static>> {
        let kind = cell.map_or(RowKind::Context, |c| c.kind);
        let number = cell.and_then(|c| c.number).filter(|_| at == 0);
        let mut spans = vec![panel_gutter(number, kind, delim, self.palette)];
        let text = cell.and_then(|c| c.rows.get(at));
        let used: usize = text.map_or(0, |t| t.iter().map(|s| display_width(&s.content)).sum());
        spans.extend(text.into_iter().flatten().cloned());
        let fill = panel.saturating_sub(PANEL_GUTTER + used);
        // An empty panel keeps the terminal's background: tinting it would read
        // as a change on a side where nothing happened.
        let style = cell.map_or(self.palette.context(), |c| {
            line_style(c.kind, c.moved, self.palette)
        });
        spans.push(Span::styled(" ".repeat(fill), style));
        spans
    }

    fn note_side(&self, text: Option<String>, panel: usize, delim: char) -> Vec<Span<'static>> {
        let mut spans = vec![panel_gutter(None, RowKind::Context, delim, self.palette)];
        let room = panel.saturating_sub(PANEL_GUTTER);
        let shown = truncate(&text.unwrap_or_default(), room);
        let fill = room.saturating_sub(display_width(&shown));
        spans.push(Span::styled(shown, self.palette.marker()));
        spans.push(Span::styled(" ".repeat(fill), self.palette.context()));
        spans
    }
}

fn panel_gutter(
    number: Option<u32>,
    kind: RowKind,
    delim: char,
    palette: &DiffPalette,
) -> Span<'static> {
    let field = number.map_or_else(String::new, |n| n.to_string());
    Span::styled(
        format!("{field:>NUMBER$}{delim}"),
        Style::new().fg(number_color(kind, palette)),
    )
}

fn number_color(kind: RowKind, palette: &DiffPalette) -> Color {
    match kind {
        RowKind::Added => palette.plus_number,
        RowKind::Removed => palette.minus_number,
        _ => palette.number,
    }
}

/// Break styled text into rows of at most `cols` columns, ending every row but
/// the last with a [`WRAP_MARK`]. Breaks fall between characters: code has no
/// word boundaries worth keeping, and a wide character that won't fit moves down
/// whole rather than being split.
fn wrap(spans: Vec<Span<'static>>, cols: usize, palette: &DiffPalette) -> Vec<Vec<Span<'static>>> {
    let total: usize = spans.iter().map(|s| display_width(&s.content)).sum();
    if total <= cols {
        return vec![spans];
    }
    // One column stays free for the mark; at least one holds text, so a pane too
    // narrow to be useful still makes progress instead of looping.
    let room = cols.saturating_sub(1).max(1);
    let mark = Span::styled(WRAP_MARK.to_string(), Style::new().fg(palette.number));

    let mut rows = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut piece = String::new();
        for c in span.content.chars() {
            let width = c.width().unwrap_or(0);
            if used + width > room && used > 0 {
                if !piece.is_empty() {
                    row.push(Span::styled(std::mem::take(&mut piece), span.style));
                }
                // Pad a row a wide character left a column short, so the mark
                // stays at the edge.
                if used < room {
                    row.push(Span::styled(" ".repeat(room - used), span.style));
                }
                row.push(mark.clone());
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            piece.push(c);
            used += width;
        }
        if !piece.is_empty() {
            row.push(Span::styled(piece, span.style));
        }
    }
    rows.push(row);
    rows
}

/// The file's name, how it changed, and anything else git said about it, over a
/// rule. A file with no hunks shows only this, so it has to say enough on its own.
fn file_header(file: &FileDiff, width: usize, palette: &DiffPalette) -> Vec<Line<'static>> {
    let path = file.path();
    let mut title = match (file.status, &file.old_path) {
        (FileStatus::Added, _) => format!("added: {path}"),
        (FileStatus::Deleted, _) => format!("removed: {path}"),
        (FileStatus::Renamed, Some(old)) => format!("renamed: {old} ⟶ {path}"),
        (FileStatus::Copied, Some(old)) => format!("copied: {old} ⟶ {path}"),
        _ => path.to_string(),
    };
    let notes = diff::notes(file);
    if !notes.is_empty() {
        title.push_str(&format!(" ({})", notes.join(", ")));
    }
    vec![
        Line::from(Span::styled(title, palette.heading())),
        Line::from(Span::styled(
            "─".repeat(width),
            Style::new().fg(palette.rule),
        )),
    ]
}

/// A hunk's enclosing function in a box open on the left, set apart from the
/// code by a blank row so the eye finds where each hunk starts:
///
/// ```text
/// ────────────────┐
///  fn enclosing() │
/// ────────────────┘
/// ```
///
/// git only names a function when it found one, so the hunk's first line number
/// stands in otherwise, and the box still marks the break.
fn hunk_header(
    start: u32,
    context: &str,
    width: usize,
    tab: usize,
    palette: &DiffPalette,
) -> Vec<Line<'static>> {
    let rule = Style::new().fg(palette.rule);
    let (label, style) = if context.is_empty() {
        (format!("{start}:"), Style::new().fg(palette.number))
    } else {
        let label = expand(context, &mut 0, tab);
        (label, palette.heading())
    };
    // The pad on each side and the right edge take three columns.
    let label = truncate(&label, width.saturating_sub(3));
    let edge = display_width(&label) + 2;
    vec![
        Line::default(),
        Line::from(Span::styled(format!("{}┐", "─".repeat(edge)), rule)),
        Line::from(vec![
            Span::styled(format!(" {label} "), style),
            Span::styled("│", rule),
        ]),
        Line::from(Span::styled(format!("{}┘", "─".repeat(edge)), rule)),
    ]
}

/// Cut `text` to `cols` columns, ending in `…` if anything was dropped.
fn truncate(text: &str, cols: usize) -> String {
    if display_width(text) <= cols {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let width = c.width().unwrap_or(0);
        if used + width + 1 > cols {
            break;
        }
        out.push(c);
        used += width;
    }
    out.push('…');
    out
}

fn no_newline(text: &str, palette: &DiffPalette) -> Line<'static> {
    Line::from(vec![
        gutter(None, None, RowKind::Context, palette),
        Span::styled(format!("⌀ {text}"), palette.marker()),
    ])
}

/// The unnumbered row above a moved block, saying where the block went.
fn move_hint(hint: &str, width: usize, palette: &DiffPalette) -> Line<'static> {
    let room = width.saturating_sub(GUTTER);
    let shown = truncate(&format!("{MOVE_MARK} {hint}"), room);
    let fill = room.saturating_sub(display_width(&shown));
    Line::from(vec![
        gutter(None, None, RowKind::Context, palette),
        Span::styled(shown, palette.marker()),
        Span::styled(" ".repeat(fill), palette.context()),
    ])
}

fn gutter(
    old: Option<u32>,
    new: Option<u32>,
    kind: RowKind,
    palette: &DiffPalette,
) -> Span<'static> {
    let field = |n: Option<u32>| n.map_or_else(String::new, |n| n.to_string());
    Span::styled(
        format!("{:>NUMBER$}⋮{:>NUMBER$}│", field(old), field(new)),
        Style::new().fg(number_color(kind, palette)),
    )
}

fn code_line(
    row: &Row,
    runs: &[Run],
    emphasis: &[Range<usize>],
    moved: bool,
    width: usize,
    tab: usize,
    palette: &DiffPalette,
) -> Line<'static> {
    let (code, column) = code_spans(row, runs, emphasis, moved, tab, palette);
    let mut spans = vec![gutter(row.old, row.new, row.kind, palette)];
    spans.extend(code);
    let fill = width.saturating_sub(GUTTER + column);
    if fill > 0 {
        spans.push(Span::styled(
            " ".repeat(fill),
            line_style(row.kind, moved, palette),
        ));
    }
    Line::from(spans)
}

/// The background a row's unemphasized text and padding sit on.
fn line_style(kind: RowKind, moved: bool, palette: &DiffPalette) -> Style {
    match kind {
        RowKind::Added | RowKind::Removed if moved => palette.moved(),
        RowKind::Added => palette.plus(),
        RowKind::Removed => palette.minus(),
        _ => palette.context(),
    }
}

/// A row's text as styled spans with tabs expanded, and the columns it takes.
fn code_spans(
    row: &Row,
    runs: &[Run],
    emphasis: &[Range<usize>],
    moved: bool,
    tab: usize,
    palette: &DiffPalette,
) -> (Vec<Span<'static>>, usize) {
    let line = line_style(row.kind, moved, palette);
    let emph = match row.kind {
        RowKind::Added => palette.plus_emph(),
        RowKind::Removed => palette.minus_emph(),
        _ => palette.context(),
    };
    // Only the added side can introduce a whitespace error: flagging a removed
    // line's would be pointing at something the change just fixed.
    let trailing = (row.kind == RowKind::Added)
        .then(|| row.text.trim_end().len())
        .filter(|&end| end < row.text.len());

    let mut spans = Vec::new();
    let mut column = 0;
    for piece in pieces(&row.text, runs, emphasis, trailing) {
        let style = if piece.trailing {
            palette.whitespace_error()
        } else {
            let background = if piece.emphasized { emph } else { line };
            background.patch(piece.syntax.unwrap_or_default())
        };
        let shown = expand(&row.text[piece.range], &mut column, tab);
        spans.push(Span::styled(shown, style));
    }
    (spans, column)
}

struct Piece {
    range: Range<usize>,
    syntax: Option<Style>,
    emphasized: bool,
    trailing: bool,
}

/// Cut `text` wherever its syntax color, emphasis or trailing-whitespace state
/// changes, so each piece has one style. `Style::patch` can then layer them.
fn pieces(
    text: &str,
    runs: &[Run],
    emphasis: &[Range<usize>],
    trailing: Option<usize>,
) -> Vec<Piece> {
    let mut cuts: Vec<usize> = runs.iter().map(|r| r.end).collect();
    cuts.extend(emphasis.iter().flat_map(|r| [r.start, r.end]));
    cuts.extend(trailing);
    cuts.extend([0, text.len()]);
    cuts.sort_unstable();
    cuts.dedup();

    cuts.windows(2)
        .map(|cut| {
            let range = cut[0]..cut[1];
            Piece {
                syntax: runs.iter().find(|r| r.end > range.start).map(|r| r.style),
                emphasized: emphasis.iter().any(|e| e.contains(&range.start)),
                trailing: trailing.is_some_and(|at| range.start >= at),
                range,
            }
        })
        .collect()
}

/// Expand tabs to real tab stops, counting from the start of the code so the
/// gutter's width can't shift them, and advance `column` by what was shown.
fn expand(text: &str, column: &mut usize, tab: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\t' {
            let to_next = tab - *column % tab;
            out.extend(std::iter::repeat_n(' ', to_next));
            *column += to_next;
        } else {
            out.push(c);
            *column += c.width().unwrap_or(0);
        }
    }
    out
}

fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::store::{Anchor, Side};
    use crate::diff::parse;
    use crate::render;
    use crate::render::moves::MoveIndex;

    const WIDTH: u16 = 40;

    fn render(raw: &str) -> (Text<'static>, LineMap) {
        let file = parse(raw).remove(0);
        render::unified(
            &file,
            &FileMoves::default(),
            WIDTH,
            DiffTheme::GitHubDark,
            4,
        )
    }

    fn file_with(hunk: &str) -> String {
        format!("diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n{hunk}")
    }

    fn plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn line_width(line: &Line<'_>) -> usize {
        display_width(&plain(line))
    }

    #[test]
    fn the_line_map_points_at_the_rows_the_numbers_are_drawn_on() {
        let (text, map) = render(&file_with("@@ -5,2 +5,2 @@\n keep\n-old\n+new\n"));
        assert_eq!(text.lines.len(), map.len());
        let row = map
            .row_for(Anchor {
                side: Side::New,
                line: 6,
            })
            .unwrap();
        assert_eq!(plain(&text.lines[row]).trim_end(), "     ⋮    6│new");
        let row = map
            .row_for(Anchor {
                side: Side::Old,
                line: 6,
            })
            .unwrap();
        assert_eq!(plain(&text.lines[row]).trim_end(), "    6⋮     │old");
    }

    #[test]
    fn the_cursor_starts_on_code_not_on_a_header() {
        let (text, map) = render(&file_with("@@ -1 +1 @@\n-a\n+b\n"));
        let first = map.first_code_row().unwrap();
        assert_eq!(plain(&text.lines[first]).trim_end(), "    1⋮     │a");
    }

    #[test]
    fn changed_lines_fill_the_pane_with_their_background() {
        let (text, map) = render(&file_with("@@ -1 +1 @@\n-a\n+日本語\n"));
        let row = map
            .row_for(Anchor {
                side: Side::New,
                line: 1,
            })
            .unwrap();
        assert_eq!(line_width(&text.lines[row]), usize::from(WIDTH));
    }

    #[test]
    fn tabs_stop_on_multiples_of_the_tab_width() {
        let (text, map) = render(&file_with("@@ -0,0 +1 @@\n+\tab\tc\n"));
        let row = map
            .row_for(Anchor {
                side: Side::New,
                line: 1,
            })
            .unwrap();
        assert_eq!(plain(&text.lines[row]).trim_end(), "     ⋮    1│    ab  c");
    }

    #[test]
    fn a_file_with_no_hunks_still_says_what_happened() {
        for (raw, title) in [
            (
                "diff --git a/i.png b/i.png\nBinary files a/i.png and b/i.png differ\n",
                "i.png (binary file)",
            ),
            (
                "diff --git a/s b/s\nold mode 100644\nnew mode 100755\n",
                "s (mode +x)",
            ),
            (
                "diff --git a/o b/n\nsimilarity index 100%\nrename from o\nrename to n\n",
                "renamed: o ⟶ n",
            ),
        ] {
            let (text, map) = render(raw);
            assert_eq!(plain(&text.lines[0]), title);
            assert_eq!(map.first_code_row(), None);
        }
    }

    #[test]
    fn trailing_whitespace_is_flagged_on_added_lines_only() {
        let (text, map) = render(&file_with("@@ -1 +1 @@\n-a  \n+b  \n"));
        let flagged = DiffTheme::GitHubDark.palette().whitespace_error();
        let flags = |side, line| {
            let row = map.row_for(Anchor { side, line }).unwrap();
            text.lines[row]
                .spans
                .iter()
                .filter(|s| s.style == flagged)
                .count()
        };
        assert_eq!(flags(Side::New, 1), 1);
        assert_eq!(flags(Side::Old, 1), 0);
    }

    #[test]
    fn the_missing_newline_marker_takes_a_row_but_no_number() {
        let (text, map) = render(&file_with(
            "@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n",
        ));
        assert_eq!(text.lines.len(), map.len());
        let new = map
            .row_for(Anchor {
                side: Side::New,
                line: 1,
            })
            .unwrap();
        let old = map
            .row_for(Anchor {
                side: Side::Old,
                line: 1,
            })
            .unwrap();
        assert_eq!(new, old + 2);
        assert_eq!(map.get(old + 1), LineNumbers::none());
    }

    #[test]
    fn a_hunk_header_boxes_the_enclosing_function() {
        let (text, map) = render(&file_with("@@ -1 +40 @@ fn outer()\n-a\n+b\n"));
        let header: Vec<String> = text.lines[2..6].iter().map(plain).collect();
        assert_eq!(
            header,
            ["", "────────────┐", " fn outer() │", "────────────┘"]
        );
        assert_eq!(map.first_code_row(), Some(6));
    }

    #[test]
    fn a_hunk_with_no_function_shows_its_line_number_instead() {
        let (text, _) = render(&file_with("@@ -1 +40 @@\n-a\n+b\n"));
        assert_eq!(plain(&text.lines[4]), " 40: │");
    }

    #[test]
    fn a_function_wider_than_the_pane_is_cut_short() {
        let long = "fn ".to_string() + &"x".repeat(60);
        let (text, _) = render(&file_with(&format!("@@ -1 +1 @@ {long}\n-a\n+b\n")));
        for line in &text.lines[3..6] {
            assert_eq!(line_width(line), usize::from(WIDTH), "{}", plain(line));
        }
        assert!(plain(&text.lines[4]).ends_with("… │"));
    }

    #[test]
    fn a_tab_in_the_function_line_keeps_the_box_square() {
        let (text, _) = render(&file_with("@@ -1 +1 @@ fn\ta()\n-a\n+b\n"));
        assert_eq!(line_width(&text.lines[3]), line_width(&text.lines[4]));
    }

    #[test]
    fn each_hunk_is_set_apart_from_the_one_before() {
        let (text, map) = render(&file_with(
            "@@ -1 +1 @@ fn a()\n-a\n+b\n@@ -9 +9 @@ fn b()\n-c\n+d\n",
        ));
        assert_eq!(text.lines.len(), map.len());
        let row = map
            .row_for(Anchor {
                side: Side::New,
                line: 1,
            })
            .unwrap();
        assert_eq!(plain(&text.lines[row + 1]), "");
        assert_eq!(plain(&text.lines[row + 3]), " fn b() │");
    }

    fn sbs(raw: &str) -> (Text<'static>, LineMap) {
        let file = parse(raw).remove(0);
        render::side_by_side(
            &file,
            &FileMoves::default(),
            WIDTH,
            DiffTheme::GitHubDark,
            4,
        )
    }

    #[test]
    fn side_by_side_sets_an_edit_opposite_its_original() {
        let (text, map) = sbs(&file_with(
            "@@ -1,2 +1,2 @@\n keep\n-let a = 1;\n+let a = 2;\n",
        ));
        let row = map
            .row_for(Anchor {
                side: Side::New,
                line: 2,
            })
            .unwrap();
        assert_eq!(
            map.row_for(Anchor {
                side: Side::Old,
                line: 2
            }),
            Some(row)
        );
        assert_eq!(
            plain(&text.lines[row]).trim_end(),
            "    2⋮let a = 1;        2│let a = 2;"
        );
    }

    #[test]
    fn side_by_side_keeps_partners_level_past_unrelated_rows() {
        let (_, map) = sbs(&file_with(
            "@@ -1,2 +1,3 @@\n-let a = 1;\n-let b = 1;\n\
             +println!(\"new\");\n+let a = 2;\n+let b = 2;\n",
        ));
        for (old, new) in [(1, 2), (2, 3)] {
            assert_eq!(
                map.row_for(Anchor {
                    side: Side::Old,
                    line: old
                }),
                map.row_for(Anchor {
                    side: Side::New,
                    line: new
                }),
            );
        }
    }

    #[test]
    fn side_by_side_zips_unrelated_lines_rather_than_stacking_them() {
        let (text, map) = sbs(&file_with("@@ -1 +1 @@\n-alpha\n+omega\n"));
        let row = map
            .row_for(Anchor {
                side: Side::Old,
                line: 1,
            })
            .unwrap();
        assert_eq!(
            map.row_for(Anchor {
                side: Side::New,
                line: 1
            }),
            Some(row)
        );
        assert_eq!(text.lines.len(), 7);
    }

    #[test]
    fn side_by_side_fills_every_row_to_the_pane_edge() {
        let long = "word ".repeat(30);
        let (text, _) = sbs(&file_with(&format!(
            "@@ -1,3 +1,3 @@\n keep\n-{long}\n+日本語{long}\n+extra\n"
        )));
        // The title and the hunk's box stop at their own text.
        for line in text.lines.iter().skip(6) {
            assert_eq!(line_width(line), usize::from(WIDTH), "{}", plain(line));
        }
    }

    #[test]
    fn side_by_side_wraps_each_panel_and_numbers_only_the_first_row() {
        let long = "x".repeat(30);
        let (text, map) = sbs(&file_with(&format!("@@ -0,0 +1 @@\n+{long}\n")));
        let row = map
            .row_for(Anchor {
                side: Side::New,
                line: 1,
            })
            .unwrap();
        let first = plain(&text.lines[row]);
        assert!(first.ends_with(WRAP_MARK), "{first}");
        assert_eq!(map.get(row + 1), LineNumbers::none());
        let shown: usize = text
            .lines
            .iter()
            .skip(6)
            .map(|l| plain(l).matches('x').count())
            .sum();
        assert_eq!(shown, long.len(), "wrapping must keep every character");
    }

    #[test]
    fn a_wide_character_at_the_edge_moves_down_whole() {
        let long = "日".repeat(20);
        let (text, _) = sbs(&file_with(&format!("@@ -0,0 +1 @@\n+{long}\n")));
        let shown: usize = text
            .lines
            .iter()
            .map(|l| plain(l).matches('日').count())
            .sum();
        assert_eq!(shown, 20);
        for line in text.lines.iter().skip(6) {
            assert_eq!(line_width(line), usize::from(WIDTH), "{}", plain(line));
        }
    }

    #[test]
    fn side_by_side_puts_each_missing_newline_marker_under_its_own_side() {
        let (text, map) = sbs(&file_with(
            "@@ -1 +1 @@\n-let a = 1;\n+let a = 2;\n\\ No newline at end of file\n",
        ));
        let row = map
            .row_for(Anchor {
                side: Side::New,
                line: 1,
            })
            .unwrap();
        let marker = plain(&text.lines[row + 1]);
        assert_eq!(map.get(row + 1), LineNumbers::none());
        let split = marker.chars().position(|c| c == RIGHT_DELIM).unwrap();
        let (left, right): (String, String) = (
            marker.chars().take(split).collect(),
            marker.chars().skip(split).collect(),
        );
        assert!(!left.contains('⌀'), "{marker}");
        assert!(right.contains('⌀'), "{marker}");
    }

    #[test]
    fn side_by_side_shows_context_on_both_sides() {
        let (text, map) = sbs(&file_with("@@ -5,1 +9,1 @@\n same\n"));
        let row = map
            .row_for(Anchor {
                side: Side::New,
                line: 9,
            })
            .unwrap();
        assert_eq!(
            map.row_for(Anchor {
                side: Side::Old,
                line: 5
            }),
            Some(row)
        );
        assert_eq!(plain(&text.lines[row]).matches("same").count(), 2);
    }

    #[test]
    fn a_pane_too_narrow_for_the_gutter_still_terminates() {
        let file = parse(&file_with("@@ -1 +1 @@\n-abcdef\n+ghijkl\n")).remove(0);
        let (text, map) =
            render::side_by_side(&file, &FileMoves::default(), 8, DiffTheme::GitHubDark, 4);
        assert_eq!(text.lines.len(), map.len());
    }

    #[test]
    fn emphasis_sits_on_a_stronger_background_than_the_line() {
        let (text, map) = render(&file_with("@@ -1 +1 @@\n-let a = 1;\n+let a = 2;\n"));
        let palette = DiffTheme::GitHubDark.palette();
        let row = map
            .row_for(Anchor {
                side: Side::New,
                line: 1,
            })
            .unwrap();
        let strong: Vec<_> = text.lines[row]
            .spans
            .iter()
            .filter(|s| s.style.bg == palette.plus_emph().bg)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(strong, ["2"]);
    }

    /// A function moved from line 1 to line 4, around a line that stayed.
    const MOVED: &str = "@@ -1,3 +1,3 @@\n-fn alpha_value() {\n-    first_call(1);\n keep\n\
                         +fn alpha_value() {\n+    first_call(1);\n";

    fn moved(side_by_side: bool) -> (Text<'static>, LineMap) {
        let files = parse(&file_with(MOVED));
        let index = MoveIndex::build(&files);
        let draw = if side_by_side {
            render::side_by_side
        } else {
            render::unified
        };
        draw(&files[0], index.file(0), WIDTH, DiffTheme::GitHubDark, 4)
    }

    fn background(text: &Text<'_>, row: usize, column_text: &str) -> Option<Color> {
        let spans = &text.lines[row].spans;
        spans
            .iter()
            .find(|s| s.content.contains(column_text))
            .and_then(|s| s.style.bg)
    }

    #[test]
    fn a_moved_row_is_tinted_rather_than_red_or_green() {
        let (text, map) = moved(false);
        let tint = DiffTheme::GitHubDark.palette().moved().bg;
        for (side, line) in [(Side::Old, 1), (Side::New, 2)] {
            let row = map.row_for(Anchor { side, line }).unwrap();
            assert_eq!(background(&text, row, "fn"), tint);
        }
    }

    #[test]
    fn a_hint_row_sits_above_a_moved_block_with_no_line_number() {
        let (text, map) = moved(false);
        let row = map
            .row_for(Anchor {
                side: Side::Old,
                line: 1,
            })
            .unwrap();
        assert_eq!(map.get(row - 1), LineNumbers::none());
        assert_eq!(
            plain(&text.lines[row - 1]).trim_end(),
            "     ⋮     │⇄ moved to line 2"
        );
        assert_eq!(text.lines.len(), map.len());
    }

    #[test]
    fn side_by_side_puts_a_hint_on_the_side_it_describes() {
        let (text, map) = moved(true);
        assert_eq!(text.lines.len(), map.len());
        let row = map
            .row_for(Anchor {
                side: Side::Old,
                line: 1,
            })
            .unwrap();
        assert_eq!(map.get(row - 1), LineNumbers::none());
        let hint = plain(&text.lines[row - 1]);
        assert_eq!(line_width(&text.lines[row - 1]), usize::from(WIDTH));
        let split = hint.chars().position(|c| c == RIGHT_DELIM).unwrap();
        let left: String = hint.chars().take(split).collect();
        assert!(left.contains("moved to"), "{hint}");
    }

    #[test]
    fn a_hint_wider_than_the_pane_is_cut_short() {
        let long = format!("{}.rs", "x".repeat(60));
        let moved = |file: &str, raw: &str| {
            format!("diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n{raw}")
        };
        let raw = moved(
            "a.rs",
            "@@ -1,2 +0,0 @@\n-fn alpha_value() {\n-    first_call(1);\n",
        ) + &moved(
            &long,
            "@@ -0,0 +1,2 @@\n+fn alpha_value() {\n+    first_call(1);\n",
        );
        let files = parse(&raw);
        let index = MoveIndex::build(&files);
        let (text, _) = render::unified(&files[0], index.file(0), WIDTH, DiffTheme::GitHubDark, 4);
        assert_eq!(line_width(&text.lines[6]), usize::from(WIDTH));
        assert!(plain(&text.lines[6]).ends_with('…'));
    }

    #[test]
    fn code_reindented_in_place_sits_opposite_itself_with_no_emphasis() {
        let files = parse(&file_with(
            "@@ -1,2 +1,2 @@\n-first_call(1);\n-second_call(2);\n\
             +    first_call(1);\n+    second_call(2);\n",
        ));
        let index = MoveIndex::build(&files);
        let (text, map) =
            render::side_by_side(&files[0], index.file(0), WIDTH, DiffTheme::GitHubDark, 4);
        let row = map
            .row_for(Anchor {
                side: Side::Old,
                line: 2,
            })
            .unwrap();
        assert_eq!(map.get(row).new, Some(2));
        let palette = DiffTheme::GitHubDark.palette();
        let emphasized = text.lines[row]
            .spans
            .iter()
            .filter(|s| [palette.plus_emph().bg, palette.minus_emph().bg].contains(&s.style.bg));
        assert_eq!(emphasized.count(), 0);
    }

    #[test]
    fn lines_swapped_in_one_change_still_draw_side_by_side() {
        let first = "    first_statement(1);\n    second_statement(2);\n";
        let second = "    third_statement(3);\n    fourth_statement(4);\n";
        let minus = |t: &str| t.lines().map(|l| format!("-{l}\n")).collect::<String>();
        let plus = |t: &str| t.lines().map(|l| format!("+{l}\n")).collect::<String>();
        let raw = file_with(&format!(
            "@@ -1,4 +1,4 @@\n{}{}{}{}",
            minus(first),
            minus(second),
            plus(second),
            plus(first),
        ));
        let files = parse(&raw);
        let index = MoveIndex::build(&files);
        let (_, map) =
            render::side_by_side(&files[0], index.file(0), WIDTH, DiffTheme::GitHubDark, 4);
        for line in 1..=4 {
            for side in [Side::Old, Side::New] {
                assert!(map.row_for(Anchor { side, line }).is_some());
            }
        }
    }

    #[test]
    fn a_trailing_blank_on_a_moved_line_is_still_flagged() {
        let files = parse(&file_with(
            "@@ -1,3 +1,3 @@\n-fn alpha_value() {\n-    first_call(1);  \n keep\n\
             +fn alpha_value() {\n+    first_call(1);  \n",
        ));
        let index = MoveIndex::build(&files);
        let (text, _) = render::unified(&files[0], index.file(0), WIDTH, DiffTheme::GitHubDark, 4);
        let flagged = DiffTheme::GitHubDark.palette().whitespace_error();
        let any = text
            .lines
            .iter()
            .flat_map(|l| &l.spans)
            .any(|s| s.style == flagged);
        assert!(any);
    }
}

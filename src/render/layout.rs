//! Turning a file's rows into styled, width-filled lines.

use std::ops::Range;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use unicode_width::UnicodeWidthChar;

use super::highlight::Run;
use crate::comment::LineMap;
use crate::comment::anchor::LineNumbers;
use crate::diff::{self, FileDiff, FileStatus, Row, RowKind};
use crate::theme::{DiffPalette, DiffTheme};

/// Width of a line-number field.
const NUMBER: usize = 5;
/// Columns the unified gutter takes: two number fields and their separators.
const GUTTER: usize = NUMBER * 2 + 2;

pub fn unified(
    file: &FileDiff,
    rows: &[Row],
    syntax: &[Vec<Run>],
    emphasis: &[Vec<Range<usize>>],
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
) -> (Text<'static>, LineMap) {
    let palette = theme.palette();
    let width = usize::from(width);
    let tab = usize::from(tab_width.max(1));

    let mut lines = file_header(file, width, &palette);
    let mut numbers = vec![LineNumbers::none(); lines.len()];
    for (i, row) in rows.iter().enumerate() {
        let line = match row.kind {
            RowKind::HunkHeader { new_start } => hunk_header(new_start, &row.text, width, &palette),
            RowKind::NoNewline => no_newline(&row.text, &palette),
            RowKind::Context | RowKind::Added | RowKind::Removed => {
                code_line(row, &syntax[i], &emphasis[i], width, tab, &palette)
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

/// `── 42: fn enclosing() ─────`, where 42 is the hunk's first new line.
fn hunk_header(start: u32, context: &str, width: usize, palette: &DiffPalette) -> Line<'static> {
    let rule = Style::new().fg(palette.rule);
    let mut spans = vec![
        Span::styled("── ", rule),
        Span::styled(format!("{start}:"), Style::new().fg(palette.number)),
    ];
    let mut used = 4 + start.to_string().len();
    if !context.is_empty() {
        let text = format!(" {context}");
        used += display_width(&text);
        spans.push(Span::styled(text, palette.heading()));
    }
    spans.push(Span::styled(
        format!(" {}", "─".repeat(width.saturating_sub(used + 1))),
        rule,
    ));
    Line::from(spans)
}

fn no_newline(text: &str, palette: &DiffPalette) -> Line<'static> {
    Line::from(vec![
        gutter(None, None, RowKind::Context, palette),
        Span::styled(
            format!("⌀ {text}"),
            Style::new()
                .fg(palette.number)
                .add_modifier(Modifier::ITALIC),
        ),
    ])
}

fn gutter(
    old: Option<u32>,
    new: Option<u32>,
    kind: RowKind,
    palette: &DiffPalette,
) -> Span<'static> {
    let field = |n: Option<u32>| n.map_or_else(String::new, |n| n.to_string());
    let color = match kind {
        RowKind::Added => palette.plus_number,
        RowKind::Removed => palette.minus_number,
        _ => palette.number,
    };
    Span::styled(
        format!("{:>NUMBER$}⋮{:>NUMBER$}│", field(old), field(new)),
        Style::new().fg(color),
    )
}

fn code_line(
    row: &Row,
    runs: &[Run],
    emphasis: &[Range<usize>],
    width: usize,
    tab: usize,
    palette: &DiffPalette,
) -> Line<'static> {
    let (line_style, emph_style) = match row.kind {
        RowKind::Added => (palette.plus(), palette.plus_emph()),
        RowKind::Removed => (palette.minus(), palette.minus_emph()),
        _ => (palette.context(), palette.context()),
    };
    // Only the added side can introduce a whitespace error: flagging a removed
    // line's would be pointing at something the change just fixed.
    let trailing = (row.kind == RowKind::Added)
        .then(|| row.text.trim_end().len())
        .filter(|&end| end < row.text.len());

    let mut spans = vec![gutter(row.old, row.new, row.kind, palette)];
    let mut column = 0;
    for piece in pieces(&row.text, runs, emphasis, trailing) {
        let style = if piece.trailing {
            palette.whitespace_error()
        } else {
            let background = if piece.emphasized {
                emph_style
            } else {
                line_style
            };
            background.patch(piece.syntax.unwrap_or_default())
        };
        let shown = expand(&row.text[piece.range], &mut column, tab);
        spans.push(Span::styled(shown, style));
    }
    let fill = width.saturating_sub(GUTTER + column);
    if fill > 0 {
        spans.push(Span::styled(" ".repeat(fill), line_style));
    }
    Line::from(spans)
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

    const WIDTH: u16 = 40;

    fn render(raw: &str) -> (Text<'static>, LineMap) {
        let file = parse(raw).remove(0);
        render::unified(&file, WIDTH, DiffTheme::GitHubDark, 4)
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
    fn a_hunk_header_shows_its_line_and_enclosing_function() {
        let (text, _) = render(&file_with("@@ -1 +40 @@ fn outer()\n-a\n+b\n"));
        let header = plain(&text.lines[2]);
        assert!(header.starts_with("── 40: fn outer() ─"), "{header}");
        assert_eq!(display_width(&header), usize::from(WIDTH));
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
}

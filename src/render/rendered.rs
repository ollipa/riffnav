//! A rendered file, and the way between its lines, rows and code.

use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use super::layout::{COPY_MARK, MOVE_MARK, WRAP_MARK};
use super::marks::Marks;
use super::spec::CommentLayer;
use super::wrap::{WRAP, rows_of, skip_rows};
use crate::comment::LineMap;
use crate::comment::anchor::Source;
use crate::comment::render::CommentBlock;
use crate::comment::render::{self as comment_render, Card};
use crate::search::Hit;
use crate::theme::DiffTheme;

/// A rendered file, ready to drop into the diff viewport.
pub struct Rendered {
    pub text: Text<'static>,
    /// Scrollable height in rows at the keyed `width`: the exact line count for
    /// side-by-side (each panel is wrapped when it's drawn), or the wrapped row
    /// count for unified (long lines are left whole for the viewer to wrap).
    pub height: u32,
    /// Unified only: cumulative wrapped-row offset before each source line, with
    /// a trailing total — `row_offsets[i]` is the first screen row of line `i`.
    /// `None` for side-by-side, where lines map one-to-one to rows.
    ///
    /// This is what keeps scrolling cheap. ratatui's `Paragraph` re-wraps every
    /// row from the top up to the scroll offset on each frame (`O(scroll)`), so
    /// deep scrolling in a long diff slows to a crawl. With these offsets the
    /// viewer seeks straight to the line under the scroll position and wraps only
    /// the visible window — see [`Rendered::visible`].
    pub(super) row_offsets: Option<Vec<u32>>,
    /// The diff line numbers behind each line of `text`, kept aligned through
    /// comment splicing. Drives the line cursor and comment anchoring.
    pub line_map: LineMap,
    /// Where each spliced comment thread landed, in row order — what `]` and `[`
    /// step through, and how the cursor names the comment it's sitting on.
    pub comment_rows: Vec<CommentBlock>,
    /// Per row of [`diff::rows`], the line it starts on and the side it's on, or
    /// `None` for a row with no code. The way back from a search match to the line
    /// it's on, which is the other way round from `line_map`.
    pub(super) row_starts: Vec<Option<RowStart>>,
    /// The first line of each change, in order: what `J` and `K` stop on. Worked
    /// out from the final lines, so comments, move notes and wrapping can't throw
    /// it off, and every new render has its own.
    pub changes: Vec<usize>,
}

/// Where a row's code starts in a render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RowStart {
    pub line: usize,
    /// 0 for the left panel or a unified line, 1 for the right panel.
    pub side: usize,
}

/// Index each row's first line. Runs after splicing, so the lines are final.
fn row_starts(map: &LineMap) -> Vec<Option<RowStart>> {
    let mut starts = Vec::new();
    for (line, sources) in map.all_sources().iter().enumerate() {
        for (side, source) in sources.iter().enumerate() {
            let Some(source) = source else { continue };
            let row = source.row as usize;
            if row >= starts.len() {
                starts.resize(row + 1, None);
            }
            starts[row].get_or_insert(RowStart { line, side });
        }
    }
    starts
}

/// The first line of each change. Side by side, a change's first line can show
/// any of its rows, so it's found by walking the lines rather than from its rows.
fn change_starts(map: &LineMap) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut last = None;
    for line in 0..map.all_sources().len() {
        let change = map.change_on(line);
        if change.is_some() && change != last {
            starts.push(line);
            last = change;
        }
    }
    starts
}

impl Rendered {
    /// The number of lines in the render (not screen rows; see [`Self::row_of`]).
    pub fn lines(&self) -> usize {
        self.text.lines.len()
    }

    /// The numbered line behind `line`, which may be `line` itself: the note above
    /// a moved or copied block stands for its first line, and a wrapped side-by-side
    /// row for the row it continues. Anything else unnumbered is left as it is.
    pub fn code_line(&self, line: usize) -> usize {
        let numbered = |at: usize| self.line_map.get(at).anchor().is_some();
        let has = |at: usize, wanted: &dyn Fn(&str) -> bool| {
            let line = self.text.lines.get(at);
            line.is_some_and(|l| l.spans.iter().any(|s| wanted(&s.content)))
        };
        if numbered(line) {
            return line;
        }
        if has(line, &|text| text.starts_with([MOVE_MARK, COPY_MARK])) {
            return line + 1;
        }
        let mut at = line;
        while at > 0 && !numbered(at) && has(at - 1, &|text| text.chars().eq([WRAP_MARK])) {
            at -= 1;
        }
        at
    }

    /// Where to scroll this render to show what `old` shows at `scroll`, going by
    /// the first numbered line in view. The top of the file stays the top.
    pub(super) fn scroll_like(&self, old: &Rendered, scroll: u32) -> Option<u32> {
        if scroll == 0 {
            return None;
        }
        let top = old.line_at(scroll);
        let (line, anchor) =
            (top..old.lines()).find_map(|line| Some((line, old.line_map.get(line).anchor()?)))?;
        let here = self.row_of(self.line_map.row_for(anchor)?);
        let above = i64::from(old.row_of(line)) - i64::from(scroll);
        Some((i64::from(here) - above).clamp(0, i64::from(u32::MAX)) as u32)
    }

    /// The first screen row of line `line`, for scrolling a cursor into view.
    pub fn row_of(&self, line: usize) -> u32 {
        match &self.row_offsets {
            None => line.min(u32::MAX as usize) as u32,
            Some(offsets) => offsets.get(line).or(offsets.last()).copied().unwrap_or(0),
        }
    }

    /// The line occupying screen row `row` — the inverse of [`Self::row_of`],
    /// used to park the cursor after a page jump.
    pub fn line_at(&self, row: u32) -> usize {
        match &self.row_offsets {
            None => (row as usize).min(self.text.lines.len().saturating_sub(1)),
            Some(offsets) => match offsets.binary_search(&row) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            }
            .min(self.text.lines.len().saturating_sub(1)),
        }
    }

    /// Where `row` of [`diff::rows`] starts in this render.
    pub fn row_start(&self, row: u32) -> Option<RowStart> {
        self.row_starts.get(row as usize).copied().flatten()
    }

    /// The screen row `hit` starts on: lower than [`Self::row_of`] its line when
    /// the line wraps, as a long one will. `width` is the width of the render.
    pub fn row_of_hit(&self, hit: Hit, width: u16) -> Option<u32> {
        let first = self.row_start(hit.row)?.line;
        match &self.row_offsets {
            // Side-by-side wrapped the row itself, onto lines of its own. Comments
            // can land between them, so a line that isn't this row's is stepped
            // over rather than taken for the end.
            None => (first..self.lines())
                .find(|&line| {
                    let shows = |s: &Source| s.row == hit.row && hit.start < s.end_char;
                    self.line_map.sources(line).iter().flatten().any(shows)
                })
                .map(|line| self.row_of(line)),
            // Unified leaves wrapping to ratatui, so ask how many rows the line
            // takes up to the match. That can be a row short, if the rest of the
            // word would have pushed it down, which the padding around a landing
            // absorbs. Laying out the whole line to find the cell would be exact,
            // but a minified line would need a screen thousands of rows tall.
            Some(_) => {
                let source = self.line_map.sources(first)[0]?;
                let line = &self.text.lines[first];
                let mut spans = line.spans[..source.first_span].to_vec();
                let mut left = hit.start as usize + 1;
                for span in &line.spans[source.first_span..] {
                    let chars = span.content.chars().count();
                    if chars >= left {
                        let end = span.content.char_indices().nth(left).map(|(i, _)| i);
                        let text = &span.content[..end.unwrap_or(span.content.len())];
                        spans.push(Span::styled(text.to_string(), span.style));
                        break;
                    }
                    spans.push(span.clone());
                    left -= chars;
                }
                let rows = Paragraph::new(Line::from(spans))
                    .wrap(WRAP)
                    .line_count(width)
                    .max(1);
                let below = (rows - 1).min(u32::MAX as usize) as u32;
                Some(self.row_of(first).saturating_add(below))
            }
        }
    }

    /// A `Paragraph` showing just the rows visible at `scroll` for a viewport
    /// `width` by `height`, so per-frame work is `O(height)` rather than
    /// `O(scroll)`. The returned widget owns its (cloned) slice of lines, so it
    /// outlives the borrow of `self`, and `marks` are painted on those copies.
    ///
    /// `scroll` must already be clamped to `self.height`.
    pub fn visible(
        &self,
        scroll: u32,
        width: u16,
        height: u16,
        marks: Option<&Marks>,
    ) -> Paragraph<'static> {
        let lines = &self.text.lines;
        let shown = |from: usize, to: usize, reach: u32| -> Vec<Line<'static>> {
            let mut shown = lines[from..to].to_vec();
            if let Some(marks) = marks {
                for (at, line) in shown.iter_mut().enumerate() {
                    marks.paint(line, &self.line_map.sources(from + at), reach);
                }
            }
            shown
        };
        match &self.row_offsets {
            // Side-by-side: one source line per row, slice it directly.
            None => {
                let start = (scroll as usize).min(lines.len());
                let end = start.saturating_add(height as usize).min(lines.len());
                Paragraph::new(Text::from(shown(start, end, u32::MAX)))
            }
            // Unified: find the source line holding wrapped row `scroll`, take it
            // plus enough following lines to fill the viewport (each contributes
            // at least one row), and let ratatui wrap just that window. The
            // sub-offset skips the part of the first line already scrolled past.
            Some(offsets) => {
                let start = match offsets.binary_search(&scroll) {
                    Ok(i) => i,
                    Err(i) => i.saturating_sub(1),
                };
                // `offsets[start] <= scroll` by construction, so this can't wrap.
                let sub = scroll - offsets[start];
                let end = start.saturating_add(height as usize + 1).min(lines.len());
                let rows = sub.saturating_add(u32::from(height)).saturating_add(1);
                let reach = marks.map_or(0, |marks| marks.reach(rows));
                let mut window = shown(start, end, reach);
                // ratatui's scroll is a `u16`, and one line can wrap further than
                // that. Such a line loses the rows above the view here instead.
                let sub = match (u16::try_from(sub), window.first_mut()) {
                    (Ok(sub), _) => sub,
                    (Err(_), Some(first)) => {
                        skip_rows(first, width, sub);
                        0
                    }
                    (Err(_), None) => 0,
                };
                Paragraph::new(Text::from(window))
                    .wrap(WRAP)
                    .scroll((sub, 0))
            }
        }
    }
}

/// Measure a render at `width`: its scrollable height and, for unified output,
/// the per-line wrapped-row offsets (see [`Rendered::row_offsets`]). Side-by-side
/// is already wrapped, so each line is exactly one row. Stops short once
/// `cancelled` is set.
pub(super) fn measure(
    text: &Text<'static>,
    width: u16,
    side_by_side: bool,
    cancelled: &AtomicBool,
) -> (u32, Option<Vec<u32>>) {
    if side_by_side {
        let height = text.lines.len().min(u32::MAX as usize) as u32;
        return (height, None);
    }
    // Wrap each line on its own — `Wrap` resets at every newline, so a line's
    // wrapped height is independent of its neighbours and the offsets sum exactly
    // to what the full paragraph would lay out.
    let mut offsets = Vec::with_capacity(text.lines.len() + 1);
    let mut acc: u32 = 0;
    for line in &text.lines {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        offsets.push(acc);
        acc = acc.saturating_add(rows_of(line, width));
    }
    offsets.push(acc);
    (acc, Some(offsets))
}

/// Splice `comments` into a fresh render and measure the result.
/// Measurement has to come after splicing: comment rows change the wrapped
/// height and shift every row offset below them.
pub(super) fn splice_and_measure(
    mut text: Text<'static>,
    mut map: LineMap,
    comments: &CommentLayer<'_>,
    width: u16,
    side_by_side: bool,
    theme: DiffTheme,
    cancelled: &AtomicBool,
) -> Rendered {
    let card = Card {
        file: comments.file,
        me: comments.me,
        width,
        diff_hash: comments.diff_hash,
        theme,
    };
    let comment_rows = comment_render::splice(&mut text, &mut map, &comments.threads, &card);
    let (height, row_offsets) = measure(&text, width, side_by_side, cancelled);
    let row_starts = row_starts(&map);
    let changes = change_starts(&map);
    Rendered {
        text,
        height,
        row_offsets,
        line_map: map,
        comment_rows,
        row_starts,
        changes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::text::{Line, Text};
    use ratatui::widgets::Paragraph;

    use crate::comment::LineMap;
    use crate::render::wrap::WRAP;

    /// The windowed render (`Rendered::visible`) must be pixel-for-pixel identical
    /// to the naive "wrap the whole text, then scroll" it replaces — at every
    /// scroll offset. Equivalence here is what lets us swap the `O(scroll)` render
    /// for an `O(viewport)` one without changing what the user sees.
    #[test]
    fn visible_window_matches_full_wrap_at_every_scroll() {
        let (width, view_h) = (24u16, 6u16);
        // A mix of blank, short, and several-widths-long lines so wrapping varies
        // line to line and the per-line offsets get a real workout.
        let lines: Vec<Line> = (0..40)
            .map(|i: usize| Line::from(format!("{i:02}:{}", "w".repeat(i * 7 % 61))))
            .collect();
        let text = Text::from(lines);

        let (height, row_offsets) = measure(&text, width, false, &AtomicBool::new(false));
        // The per-line offsets must sum to the same height a single full wrap gives.
        let full = Paragraph::new(text.clone()).wrap(WRAP).line_count(width) as u32;
        assert_eq!(height, full, "summed offsets disagree with a full wrap");

        let rendered = Rendered {
            text: text.clone(),
            height,
            row_offsets,
            line_map: LineMap::default(),
            comment_rows: Vec::new(),
            row_starts: Vec::new(),
            changes: Vec::new(),
        };
        let draw = |paragraph: Paragraph| -> Buffer {
            let mut term = Terminal::new(TestBackend::new(width, view_h)).unwrap();
            term.draw(|f| f.render_widget(paragraph, f.area())).unwrap();
            term.backend().buffer().clone()
        };

        for scroll in 0..=height {
            let got = draw(rendered.visible(scroll, width, view_h, None));
            let want = draw(
                Paragraph::new(text.clone())
                    .wrap(WRAP)
                    .scroll((scroll as u16, 0)),
            );
            assert_eq!(
                got, want,
                "visible window differs from full wrap at scroll {scroll}"
            );
        }
    }
}

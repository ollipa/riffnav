//! Counting and skipping the rows a line wraps onto, as ratatui wraps it.

use std::collections::VecDeque;

use ratatui::buffer::CellWidth;
use ratatui::style::Style;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Wrap};

/// Word-wrap setting shared by the height measurement and the per-frame render,
/// so both lay text out identically. `trim: false` keeps leading whitespace
/// (diff indentation) on wrapped rows.
pub(super) const WRAP: Wrap = Wrap { trim: false };

/// The most columns `text` can take. A character is at most two wide, and what
/// counts as one here is sure to be one: laying out text is what's being avoided,
/// and its idea of width can go by more than a character at a time.
fn most_columns(text: &str) -> usize {
    if text.is_ascii() {
        return text.len();
    }
    let narrow =
        |c: char| c.is_ascii() || matches!(c, '\u{2200}'..='\u{22ff}' | '\u{2500}'..='\u{257f}');
    text.chars().map(|c| if narrow(c) { 1 } else { 2 }).sum()
}

/// The rows `line` wraps onto. Nearly every line fits, as lines are padded out to
/// the width, so that's checked for first: wrapping them all is most of the work
/// of a render that has no syntax to color.
pub(super) fn rows_of(line: &Line<'static>, width: u16) -> u32 {
    let most: usize = line.spans.iter().map(|s| most_columns(&s.content)).sum();
    if most <= usize::from(width) {
        return 1;
    }
    Paragraph::new(Text::from(line.clone()))
        .wrap(WRAP)
        .line_count(width)
        .max(1) as u32
}

/// Where a grapheme is in a line: its span and its byte in that span.
type Place = (usize, usize);

/// Take the first `rows` rows off `line`, as wrapped to `width`, leaving what the
/// rows after them would show. ratatui keeps its wrapping to itself, so this walks
/// the line the way its `WordWrapper` does with `trim: false`, down to which
/// whitespace a break swallows, only noting where each row starts. Text carried
/// over a break is carried from where it starts, so a row starts with it.
pub(super) fn skip_rows(line: &mut Line<'static>, width: u16, rows: u32) {
    let mut left = rows;
    let (mut full, mut empty) = (0, true); // the columns the row has settled on
    let (mut word, mut word_start) = (0, None);
    let (mut gap, mut spaces) = (0, VecDeque::<(Place, u16)>::new());
    let mut after_word = false;
    let mut next = None;
    'walk: for (index, span) in line.spans.iter().enumerate() {
        for grapheme in span.styled_graphemes(Style::default()) {
            let place = (
                index,
                grapheme.symbol.as_ptr() as usize - span.content.as_ptr() as usize,
            );
            if left == 0 {
                next = Some(next.unwrap_or(place));
                break 'walk;
            }
            let blank = grapheme.is_whitespace();
            let cols = grapheme.symbol.cell_width();
            if cols > width {
                continue;
            }
            if (after_word && blank) || (empty && word + gap + cols > width) {
                full += gap + word;
                empty = empty && spaces.is_empty() && word_start.is_none();
                (gap, word, word_start) = (0, 0, None);
                spaces.clear();
            }
            if full >= width || (cols > 0 && full + gap + word >= width) {
                let mut room = width.saturating_sub(full);
                (full, empty) = (0, true);
                while let Some(&(_, cols)) = spaces.front().filter(|(_, cols)| *cols <= room) {
                    gap -= cols;
                    room -= cols;
                    spaces.pop_front();
                }
                left -= 1;
                next = spaces.front().map(|(place, _)| *place).or(word_start);
                if blank && spaces.is_empty() {
                    continue;
                }
                next = next.or(Some(place));
            }
            if blank {
                gap += cols;
                spaces.push_back((place, cols));
            } else {
                word += cols;
                word_start.get_or_insert(place);
            }
            after_word = !blank;
        }
    }
    let Some((span, byte)) = next.filter(|_| left == 0) else {
        line.spans.clear();
        return;
    };
    line.spans.drain(..span);
    let first = &mut line.spans[0];
    first.content = first.content[byte..].to_string().into();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    use ratatui::buffer::{Buffer, CellWidth};
    use ratatui::layout::Rect;
    use ratatui::prelude::Widget;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span, Text};
    use ratatui::widgets::Paragraph;

    use crate::comment::LineMap;
    use crate::render::rendered::{Rendered, measure};

    /// What `line` shows from row `from`, for as many rows as `view` has.
    fn rows_from(line: Line<'static>, width: u16, view: u16, from: u16) -> Buffer {
        // With room to spare, as a word of wide characters can run a column over.
        let mut buffer = Buffer::empty(Rect::new(0, 0, width + 2, view + 1));
        let shown = Paragraph::new(line).wrap(WRAP).scroll((from, 0));
        shown.render(Rect::new(0, 0, width, view), &mut buffer);
        buffer
    }

    /// Cutting the rows off has to leave what ratatui would have shown below them,
    /// whatever the line is made of.
    #[test]
    fn skipping_rows_shows_what_scrolling_to_them_shows() {
        let pieces = [
            "a", "bc", "def", " ", "  ", "\t", "語", "é", "e\u{301}", "\u{200b}", "\u{a0}", "🙂",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut random = move |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 33) as usize % below
        };
        for _ in 0..3000 {
            let width = 1 + random(12) as u16;
            let mut spans = Vec::new();
            for span in 0..1 + random(5) {
                let text: String = (0..random(30))
                    .map(|_| pieces[random(pieces.len())])
                    .collect();
                spans.push(Span::styled(
                    text,
                    Style::new().fg(Color::Indexed(span as u8)),
                ));
            }
            let line = Line::from(spans);
            let rows = Paragraph::new(line.clone()).wrap(WRAP).line_count(width) as u32;
            for skip in 0..=rows {
                let mut cut = line.clone();
                skip_rows(&mut cut, width, skip);
                let want = rows_from(line.clone(), width, 4, skip as u16);
                // Starting over can't reproduce a row that overran, which ratatui
                // only lets through mid-line: it wraps it instead.
                if want[(width, 0)].symbol() != " "
                    || want[(width - 1, 0)].symbol().cell_width() > 1
                {
                    continue;
                }
                let got = rows_from(cut.clone(), width, 4, 0);
                assert_eq!(got, want, "{line:?} at {width}, {skip} rows off: {cut:?}");
            }
        }
    }

    /// One line can wrap past what a `Paragraph` can scroll to.
    #[test]
    fn a_line_taller_than_a_paragraph_can_scroll_shows_its_far_end() {
        let (width, view) = (8u16, 4u16);
        let words: String = (0..200_000).map(|i| format!("{i:06} ")).collect();
        let text = Text::from(Line::from(words));
        let (height, row_offsets) = measure(&text, width, false, &AtomicBool::new(false));
        assert!(height > u32::from(u16::MAX), "{height}");
        let rendered = Rendered {
            text,
            height,
            row_offsets,
            line_map: LineMap::default(),
            comment_rows: Vec::new(),
            row_starts: Vec::new(),
            changes: Vec::new(),
        };
        for scroll in [70_000, 150_000, height - u32::from(view)] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, view));
            rendered
                .visible(scroll, width, view, None)
                .render(buffer.area, &mut buffer);
            let row: String = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();
            assert_eq!(row.trim_end(), format!("{scroll:06}"), "{scroll}");
        }
    }

    /// A wrapped line counted wrong puts every row below it out by one.
    #[test]
    fn counting_a_lines_rows_quickly_agrees_with_wrapping_it() {
        let samples = [
            "",
            "plain ascii that goes on for a while, past most widths",
            "   ⋮   2│code",
            "│".repeat(30).as_str().to_string().leak(),
            "日本語のテキストがここに続きます",
            "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}",
            "❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}",
            "👩\u{200d}💻👩\u{200d}💻👩\u{200d}💻👩\u{200d}💻",
            "لالالالالالالالالالا",
            "🇫🇮🇫🇮🇫🇮🇫🇮🇫🇮",
            "a\u{7}b\tc\u{1b}d\re",
            "   leading and   spaced   out    words     ",
        ];
        for sample in samples {
            for width in 1..48 {
                let line = Line::from(vec![Span::raw("  1⋮  1│"), Span::raw(sample)]);
                let slow = Paragraph::new(Text::from(line.clone()))
                    .wrap(WRAP)
                    .line_count(width)
                    .max(1) as u32;
                assert_eq!(rows_of(&line, width), slow, "{sample:?} at {width}");
            }
        }
    }
}

//! Painting search matches over a cached render at draw time.
//!
//! The render cache never sees a query: [`Rendered::visible`] already clones the
//! lines it hands to the screen, so matches are cut into those copies. Typing in
//! the search prompt therefore costs a viewport's worth of work, not a render.
//! Going through the spans rather than the screen's cells is what keeps a match
//! on the right characters however ratatui wraps the line.
//!
//! [`Rendered::visible`]: super::Rendered::visible

use std::ops::Range;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::comment::anchor::Source;
use crate::search::Hit;

/// How many characters a screen cell is taken to hold at most, for the combining
/// marks that share one, when working out how far along a line can be on screen.
const CHARS_PER_CELL: u32 = 4;

/// One file's matches, to paint over its render.
pub struct Marks<'a> {
    /// The file's matches, by row.
    pub hits: &'a [Hit],
    pub current: Option<Hit>,
    /// Background of a match.
    pub bg: Color,
    /// Colors of the current match, which alone gives up its syntax color.
    pub current_fg: Color,
    pub current_bg: Color,
    /// The width of the pane they're being drawn in.
    pub width: u16,
}

impl Marks<'_> {
    /// How many characters of a line can be showing in its first `rows` rows.
    pub fn reach(&self, rows: u16) -> u32 {
        let cells = u32::from(rows) * u32::from(self.width);
        cells.saturating_mul(CHARS_PER_CELL)
    }

    /// `source`'s share of the matches up to its character `reach`, as ranges of
    /// its own characters.
    fn in_source(&self, source: &Source, reach: u32) -> Vec<(Range<u32>, Style)> {
        let first = self.hits.partition_point(|h| h.row < source.row);
        let row = &self.hits[first..];
        // A wrapped row is many sources, and a minified one can have a lot of
        // matches, so the part of the row before this source is skipped outright.
        let first = row.partition_point(|h| h.row == source.row && h.end <= source.first_char);
        row[first..]
            .iter()
            .take_while(|h| h.row == source.row && h.start < source.end_char.min(reach))
            .map(|hit| {
                let start = hit.start.max(source.first_char) - source.first_char;
                let end = hit.end.min(source.end_char) - source.first_char;
                let style = if Some(*hit) == self.current {
                    Style::new()
                        .fg(self.current_fg)
                        .bg(self.current_bg)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::new().bg(self.bg)
                };
                (start..end, style)
            })
            .collect()
    }

    /// Paint a rendered line's matches, given what its code came from. Nothing
    /// past character `reach` of the row is painted: a minified row can run to
    /// thousands of screens, and cutting all of it into spans for each frame to
    /// show one would cost more than drawing it does.
    pub fn paint(&self, line: &mut Line<'static>, sources: &[Option<Source>; 2], reach: u32) {
        // The right side first, so painting it can't move the left's spans.
        for source in sources.iter().rev().flatten() {
            let marks = self.in_source(source, reach);
            if !marks.is_empty() {
                paint_source(line, source.first_span, &marks);
            }
        }
    }
}

/// Recolor `marks`, counted in characters from the start of span `first`.
fn paint_source(line: &mut Line<'static>, first: usize, marks: &[(Range<u32>, Style)]) {
    let mut rest = marks.iter().peekable();
    let mut spans = Vec::with_capacity(line.spans.len() + marks.len() * 2);
    let mut at = 0;
    for (index, span) in std::mem::take(&mut line.spans).into_iter().enumerate() {
        if index < first || rest.peek().is_none() {
            spans.push(span);
            continue;
        }
        let chars = span.content.chars().count() as u32;
        let end = at + chars;
        if rest.peek().is_none_or(|(range, _)| range.start >= end) {
            spans.push(span);
            at = end;
            continue;
        }
        let text = span.content.as_ref();
        let bytes: Vec<usize> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain([text.len()])
            .collect();
        let mut cut = 0;
        while let Some((range, mark)) = rest.peek() {
            if range.start >= end {
                break;
            }
            let from = bytes[range.start.saturating_sub(at) as usize];
            let to = bytes[(range.end.min(end) - at) as usize];
            if from > cut {
                spans.push(Span::styled(text[cut..from].to_string(), span.style));
            }
            // Whitespace errors are drawn reversed, which would show this
            // background as their foreground instead.
            let style = span.style.remove_modifier(Modifier::REVERSED).patch(*mark);
            spans.push(Span::styled(text[from..to].to_string(), style));
            cut = to;
            if range.end > end {
                break; // It runs on into the next span.
            }
            rest.next();
        }
        if cut < text.len() {
            spans.push(Span::styled(text[cut..].to_string(), span.style));
        }
        at = end;
    }
    line.spans = spans;
}

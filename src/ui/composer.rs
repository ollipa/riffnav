//! The comment composer, drawn as a field under its line.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Padding, Paragraph};

use crate::app::App;

/// Tallest the comment composer grows before it scrolls internally, so a long
/// note never swallows the diff it's about.
const COMPOSER_MAX_ROWS: u16 = 8;

/// The keys that end a note, in the longest form the frame can hold: clipping
/// the hint would hide the very key that closes the field.
fn composer_hint(width: u16) -> &'static str {
    let room = width.saturating_sub(2) as usize;
    [
        " Enter save · Shift-Enter newline · Esc cancel · Ctrl-O $EDITOR ",
        " Enter save · S-Enter newline · Esc cancel · ^O editor ",
        " Enter save · S-Enter newline · Esc cancel ",
        " Enter save · Esc cancel ",
        " Enter · Esc ",
    ]
    .into_iter()
    .find(|hint| hint.chars().count() <= room)
    .unwrap_or("")
}

/// The note being typed, as a bordered field pinned to what it will hang on:
/// below it where there's room, otherwise above, so the code — or, for a reply,
/// the thread — under discussion stays on screen while it's written. A no-op
/// when nothing is being composed.
pub(super) fn render_composer(frame: &mut Frame, area: Rect, app: &App) {
    let Some(composer) = app.composer() else {
        return;
    };
    // Two border columns and a column of padding on each side.
    let Some(cols) = area.width.checked_sub(4).filter(|c| *c > 0) else {
        return;
    };
    if area.height < 3 {
        return;
    }
    let layout = composer.layout(cols as usize);

    // As tall as what's typed, within what the pane can spare; a longer note
    // scrolls inside the field.
    let max_body = area.height.saturating_sub(2).min(COMPOSER_MAX_ROWS);
    let height = (layout.rows.len() as u16).clamp(1, max_body) + 2;
    let y = match app.composer_rows() {
        Some((_, bottom)) if bottom + height <= area.height => area.y + bottom,
        Some((top, _)) if top >= height => area.y + top - height,
        // The cursor is off-screen, or hemmed in on both sides: fall back to the
        // foot of the pane, where the field can't cover the line being annotated
        // any worse than it already would.
        _ => area.bottom().saturating_sub(height),
    };
    let popup = Rect {
        x: area.x,
        y,
        width: area.width,
        height,
    };

    // Framed like the box this note will become once it's saved, in the same
    // theme accent, so the field reads as the comment taking shape in place.
    let accent = Style::new().fg(crate::comment::render::accent(app.diff_theme));
    let block = Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(accent)
        .title(Span::styled(
            composer.title(),
            accent.add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            composer_hint(popup.width),
            Style::new().add_modifier(Modifier::DIM),
        ))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    // Keep the cursor's row in view when the note outgrows the field.
    let first = layout
        .cursor
        .0
        .saturating_sub(inner.height.saturating_sub(1) as usize);
    let lines: Vec<Line> = layout
        .rows
        .iter()
        .skip(first)
        .take(inner.height as usize)
        .map(|row| Line::from(row.clone()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);

    // The terminal's own cursor marks the insertion point — ratatui shows it
    // only for the frames that ask for a position, which is exactly these.
    frame.set_cursor_position((
        inner.x + (layout.cursor.1 as u16).min(inner.width.saturating_sub(1)),
        inner.y + (layout.cursor.0 - first) as u16,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_composer_hint_shrinks_rather_than_being_clipped() {
        assert!(composer_hint(80).contains("Shift-Enter newline"));
        assert!(composer_hint(40).contains("Enter save"));
        // Whatever survives must still fit between the frame's corners.
        for width in 0..80u16 {
            let hint = composer_hint(width);
            assert!(
                hint.chars().count() <= width.saturating_sub(2) as usize,
                "hint {hint:?} overflows a {width}-wide frame"
            );
        }
    }
}

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::Paragraph;

use crate::app::{App, Focus};

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, diff_width: u16) {
    // Record the viewport height so PageUp/PageDown can jump by a screenful.
    app.diff_height = area.height;

    let Some(idx) = app.selected_file() else {
        placeholder(
            frame,
            area,
            "\n  Select a file in the tree to view its diff.",
        );
        return;
    };

    // Added files always render unified (an empty left pane wastes space), so
    // resolve the effective mode once to key the cache lookup.
    let side_by_side = app.side_by_side_for(idx);

    // Build a widget covering just the visible window, clamping the scroll to the
    // render's height. `visible` only ever lays out a viewport's worth of rows —
    // not the whole prefix up to the scroll offset — so deep scrolling stays
    // cheap. The returned paragraph owns its lines, so the cache borrow ends here
    // and we can write the clamped scroll back below.
    let built = app
        .cache
        .get(idx, diff_width, side_by_side, app.diff_theme)
        .map(|r| {
            let scroll = app.diff_scroll.min(r.height.saturating_sub(area.height));
            (r.visible(scroll, area.height), scroll)
        });

    let Some((paragraph, scroll)) = built else {
        placeholder(frame, area, "  rendering…");
        return;
    };
    app.diff_scroll = scroll;

    frame.render_widget(paragraph, area);

    // The cursor is painted over the finished paragraph rather than styled into
    // the text: the render is cached and shared across frames, so tinting a line
    // in it would leave the highlight behind when the cursor moves.
    if app.comments_enabled() && app.focus == Focus::Diff {
        highlight_cursor(frame, area, app, idx, side_by_side, scroll);
    }
}

/// Tint the row(s) the line cursor occupies. Only the background is replaced, so
/// syntax colors and the +/- foreground survive underneath it.
fn highlight_cursor(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    idx: usize,
    side_by_side: bool,
    scroll: u16,
) {
    let Some(render) = app
        .cache
        .get(idx, app.last_diff_width(), side_by_side, app.diff_theme)
    else {
        return;
    };
    // A wrapped line covers several rows, so highlight the whole run.
    let top = render.row_of(app.diff_cursor);
    let bottom = render.row_of(app.diff_cursor + 1).max(top + 1);
    let bg = app.diff_theme.cursor_line_bg();
    let buf = frame.buffer_mut();
    for row in top..bottom {
        // Rows above the viewport (or below it) simply have no cells to paint.
        let Some(y) = row.checked_sub(scroll).filter(|y| *y < area.height) else {
            continue;
        };
        for x in area.left()..area.right() {
            buf[(x, area.y + y)].set_bg(bg);
        }
    }
}

fn placeholder(frame: &mut Frame, area: Rect, msg: &str) {
    let text = Text::from(msg).style(Style::new().add_modifier(Modifier::DIM));
    frame.render_widget(Paragraph::new(text), area);
}

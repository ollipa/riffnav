use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::Paragraph;

use crate::app::{App, Focus};
use crate::render::Key;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, diff_width: u16) {
    // Record the viewport height so PageUp/PageDown can jump by a screenful.
    app.diff_height = area.height;
    // A new width can have pushed the line cursor off a pane this tall.
    app.keep_cursor_in_view();

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

    // `visible` lays out only a viewport's worth of rows, so deep scrolling stays
    // cheap. The paragraph owns its lines, so the cache borrow ends here and the
    // clamped scroll can be written back below.
    let marks = app.search_marks(idx, area.width);
    let key = Key {
        file: idx,
        width: diff_width,
        side_by_side,
        theme: app.diff_theme,
    };
    let built = app.cache.get(key).map(|r| {
        let scroll = app
            .diff_scroll
            .min(r.height.saturating_sub(area.height.into()));
        let shown = r.visible(scroll, area.width, area.height, marks.as_ref());
        let rows = scroll..scroll.saturating_add(area.height.into());
        (shown, scroll, rows.contains(&r.row_of(app.diff_cursor)))
    });

    let Some((paragraph, scroll, cursor_on_screen)) = built else {
        placeholder(frame, area, "  rendering…");
        return;
    };
    app.diff_scroll = scroll;
    app.cursor_on_screen = cursor_on_screen;

    frame.render_widget(paragraph, area);

    // The cursor is painted over the finished paragraph rather than styled into
    // the text: the render is cached and shared across frames, so tinting a line
    // in it would leave the highlight behind when the cursor moves.
    if app.comments_enabled() && app.focus == Focus::Diff {
        highlight_cursor(frame, area, app, idx, side_by_side, scroll);
    }
}

/// Tint the row(s) the line cursor occupies. Only the background is replaced, so
/// syntax colors and the +/- foreground survive underneath it. Search matches
/// keep theirs: the cursor follows the current match, which has to show on it.
fn highlight_cursor(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    idx: usize,
    side_by_side: bool,
    scroll: u32,
) {
    let key = Key {
        file: idx,
        width: app.last_diff_width(),
        side_by_side,
        theme: app.diff_theme,
    };
    let Some(render) = app.cache.get(key) else {
        return;
    };
    // A wrapped line covers several rows, so highlight the whole run.
    let top = render.row_of(app.diff_cursor);
    let bottom = render.row_of(app.diff_cursor + 1).max(top + 1);
    // Rows above the viewport (or below it) simply have no cells to paint, and a
    // minified line can have far more of them than there are on screen.
    let rows = top.max(scroll)..bottom.min(scroll.saturating_add(area.height.into()));
    let bg = app.diff_theme.cursor_line_bg();
    let matches = [app.diff_theme.match_bg(), app.diff_theme.current_match_bg()];
    let buf = frame.buffer_mut();
    for row in rows {
        let y = area.y + (row - scroll) as u16;
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            if !matches.contains(&cell.bg) {
                cell.set_bg(bg);
            }
        }
    }
}

fn placeholder(frame: &mut Frame, area: Rect, msg: &str) {
    let text = Text::from(msg).style(Style::new().add_modifier(Modifier::DIM));
    frame.render_widget(Paragraph::new(text), area);
}

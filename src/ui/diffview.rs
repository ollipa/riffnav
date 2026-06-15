use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::App;

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

    // Pull what we need out of the cache as owned values so the borrow ends
    // before we write back the clamped scroll offset.
    let rendered = app
        .cache
        .get(idx, diff_width, app.side_by_side, app.diff_theme)
        .map(|r| (r.text.clone(), r.lines));

    let Some((text, lines)) = rendered else {
        placeholder(frame, area, "  rendering…");
        return;
    };

    // delta wraps side-by-side output to the pane width itself, so its line
    // count is exact and we render as-is. Unified output is left unwrapped (delta
    // assumes a downstream pager), so we wrap it here — otherwise long lines are
    // truncated at the pane edge — and measure the wrapped height so scrolling
    // can still reach the bottom.
    let mut paragraph = Paragraph::new(text);
    let height = if app.side_by_side {
        lines
    } else {
        paragraph = paragraph.wrap(Wrap { trim: false });
        paragraph.line_count(area.width).min(u16::MAX as usize) as u16
    };

    let max_scroll = height.saturating_sub(area.height);
    let scroll = app.diff_scroll.min(max_scroll);
    app.diff_scroll = scroll;

    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

fn placeholder(frame: &mut Frame, area: Rect, msg: &str) {
    let text = Text::from(msg).style(Style::new().add_modifier(Modifier::DIM));
    frame.render_widget(Paragraph::new(text), area);
}

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::Paragraph;

use crate::app::App;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, diff_width: u16) {
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
        .get(idx, diff_width, app.side_by_side)
        .map(|r| (r.text.clone(), r.lines));

    let Some((text, lines)) = rendered else {
        placeholder(frame, area, "  rendering…");
        return;
    };

    let max_scroll = lines.saturating_sub(area.height);
    let scroll = app.diff_scroll.min(max_scroll);
    app.diff_scroll = scroll;

    frame.render_widget(Paragraph::new(text).scroll((scroll, 0)), area);
}

fn placeholder(frame: &mut Frame, area: Rect, msg: &str) {
    let text = Text::from(msg).style(Style::new().add_modifier(Modifier::DIM));
    frame.render_widget(Paragraph::new(text), area);
}

//! The overlay for picking which agent in the herdr tab gets the comments.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState};
use unicode_width::UnicodeWidthStr;

use super::centered_rect;
use crate::app::App;

const TITLE: &str = " Send comments to (Enter send · Esc cancel) ";

pub(super) fn render_picker(frame: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = app.agent_picker() else {
        return;
    };
    let label_w = picker
        .agents
        .iter()
        .map(|a| a.label.width())
        .max()
        .unwrap_or(0);
    let term_w = picker
        .agents
        .iter()
        .map(|a| a.terminal.width())
        .max()
        .unwrap_or(0);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let items: Vec<ListItem> = picker
        .agents
        .iter()
        .map(|a| {
            let pad = label_w - a.label.width();
            ListItem::new(Line::from(vec![
                Span::raw(format!(" {}{}  ", a.label, " ".repeat(pad))),
                Span::styled(format!("{:term_w$}  ", a.terminal), dim),
                Span::styled(a.status.as_deref().unwrap_or("-").to_string(), dim),
                Span::raw(" "),
            ]))
        })
        .collect();
    let row_w = 1 + label_w + 2 + term_w + 2 + "working".len() + 1;
    let width = (row_w.max(TITLE.width()) + 2) as u16;
    let height = (picker.agents.len() + 2) as u16;
    let popup = centered_rect(width, height, area);
    frame.render_widget(Clear, popup);
    let mut state = ListState::default();
    state.select(Some(picker.selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::bordered().title(TITLE))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        popup,
        &mut state,
    );
}

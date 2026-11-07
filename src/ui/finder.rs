//! The fuzzy file finder overlay.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use super::{centered_rect, filetree};
use crate::app::App;
use crate::diff::path::shown;

pub(super) fn render_finder(frame: &mut Frame, area: Rect, app: &App) {
    let Some(finder) = &app.finder else {
        return;
    };

    let popup = centered_rect(72, 18, area);
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(format!(
        " Find file ({} matches · Enter open · Esc cancel) ",
        finder.matches.len()
    ));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let [query_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::new().fg(Color::Cyan)),
            Span::raw(&finder.query),
            Span::styled("▏", Style::new().add_modifier(Modifier::SLOW_BLINK)),
        ])),
        query_area,
    );

    let items: Vec<ListItem> = finder
        .matches
        .iter()
        .map(|&i| {
            let file = &app.files[i];
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", file.status.sigil()),
                    Style::new().fg(filetree::status_color(file.status)),
                ),
                Span::raw(shown(file.path()).into_owned()),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    if !finder.matches.is_empty() {
        state.select(Some(finder.selected));
    }
    frame.render_stateful_widget(
        List::new(items).highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        list_area,
        &mut state,
    );
}

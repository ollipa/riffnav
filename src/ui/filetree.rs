use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::app::App;
use crate::diff::FileStatus;
use crate::tree::RowKind;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            match row.kind {
                RowKind::Dir { expanded } => {
                    let marker = if expanded { "▾" } else { "▸" };
                    ListItem::new(Line::from(vec![
                        Span::raw(indent),
                        Span::styled(
                            format!("{marker} {}/", row.name),
                            Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
                        ),
                    ]))
                }
                RowKind::File { diff_index } => {
                    let status = app.files[diff_index].status;
                    ListItem::new(Line::from(vec![
                        Span::raw(indent),
                        Span::styled(
                            format!("{} ", status.sigil()),
                            Style::new().fg(status_color(status)),
                        ),
                        Span::raw(row.name.clone()),
                    ]))
                }
            }
        })
        .collect();

    let list = List::new(items)
        .block(Block::new().borders(Borders::RIGHT))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    frame.render_stateful_widget(list, area, &mut app.tree_state);
}

fn status_color(status: FileStatus) -> Color {
    match status {
        FileStatus::Added => Color::Green,
        FileStatus::Modified => Color::Yellow,
        FileStatus::Deleted => Color::Red,
        FileStatus::Renamed => Color::Cyan,
        FileStatus::Copied => Color::Magenta,
    }
}

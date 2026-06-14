use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::app::App;
use crate::diff::FileStatus;
use crate::icons;
use crate::tree::RowKind;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, focused: bool) {
    let style = app.icon_style;
    let inner = (area.width as usize).saturating_sub(1); // content width, minus border

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            match row.kind {
                RowKind::Dir { expanded, .. } => {
                    let marker = icons::dir_icon(expanded, style);
                    ListItem::new(Line::from(vec![
                        Span::raw(indent),
                        Span::styled(
                            format!("{marker} {}/", row.name),
                            Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
                        ),
                    ]))
                }
                RowKind::File { diff_index } => {
                    let file = &app.files[diff_index];
                    let status = file.status;
                    let icon = icons::file_icon(file.path(), style);
                    let adds = format!("+{}", file.additions);
                    let dels = format!("-{}", file.deletions);

                    // Right-align the "+a -b" badge: pad between the name and badge.
                    let icon_w = if icon.is_empty() { 0 } else { icon.chars().count() + 1 };
                    let left = row.depth * 2 + 2 + icon_w + row.name.chars().count();
                    let badge = adds.len() + 1 + dels.len();
                    let pad = inner.saturating_sub(left + badge).max(1);

                    let mut spans = vec![
                        Span::raw(indent),
                        Span::styled(
                            format!("{} ", status.sigil()),
                            Style::new().fg(status_color(status)),
                        ),
                    ];
                    if !icon.is_empty() {
                        spans.push(Span::raw(format!("{icon} ")));
                    }
                    spans.push(Span::raw(row.name.clone()));
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans.push(Span::styled(
                        adds,
                        Style::new().fg(Color::Green).add_modifier(Modifier::DIM),
                    ));
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        dels,
                        Style::new().fg(Color::Red).add_modifier(Modifier::DIM),
                    ));
                    ListItem::new(Line::from(spans))
                }
            }
        })
        .collect();

    let border_style = if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().add_modifier(Modifier::DIM)
    };
    let list = List::new(items)
        .block(Block::new().borders(Borders::RIGHT).border_style(border_style))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    frame.render_stateful_widget(list, area, &mut app.tree_state);
}

pub(super) fn status_color(status: FileStatus) -> Color {
    match status {
        FileStatus::Added => Color::Green,
        FileStatus::Modified => Color::Yellow,
        FileStatus::Deleted => Color::Red,
        FileStatus::Renamed => Color::Cyan,
        FileStatus::Copied => Color::Magenta,
    }
}

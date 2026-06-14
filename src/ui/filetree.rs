use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::app::App;
use crate::diff::FileStatus;
use crate::tree::RowKind;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, focused: bool) {
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth);
            match row.kind {
                RowKind::Dir { expanded, .. } => {
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
                    let file = &app.files[diff_index];
                    let status = file.status;
                    let adds = format!("+{}", file.additions);
                    let dels = format!("-{}", file.deletions);

                    // Right-align the "+a -b" badge: pad between the name and badge.
                    let inner = (area.width as usize).saturating_sub(1); // minus border
                    let left = row.depth * 2 + 2 + row.name.chars().count();
                    let badge = adds.len() + 1 + dels.len();
                    let pad = inner.saturating_sub(left + badge).max(1);

                    ListItem::new(Line::from(vec![
                        Span::raw(indent),
                        Span::styled(
                            format!("{} ", status.sigil()),
                            Style::new().fg(status_color(status)),
                        ),
                        Span::raw(row.name.clone()),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(adds, Style::new().fg(Color::Green).add_modifier(Modifier::DIM)),
                        Span::raw(" "),
                        Span::styled(dels, Style::new().fg(Color::Red).add_modifier(Modifier::DIM)),
                    ]))
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

fn status_color(status: FileStatus) -> Color {
    match status {
        FileStatus::Added => Color::Green,
        FileStatus::Modified => Color::Yellow,
        FileStatus::Deleted => Color::Red,
        FileStatus::Renamed => Color::Cyan,
        FileStatus::Copied => Color::Magenta,
    }
}

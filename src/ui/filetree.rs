use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};

use crate::app::App;
use crate::diff::FileStatus;
use crate::icons;
use crate::tree::RowKind;

/// Rows kept visible above and below the selected row while scrolling.
const SCROLL_PADDING: usize = 5;

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
                    let viewed = app.is_viewed(diff_index);
                    let icon = icons::file_icon(file.path(), style);
                    let adds = format!("+{}", file.additions);
                    let dels = format!("-{}", file.deletions);
                    // Files carrying review comments get a count ahead of the
                    // ± badge, so an agent's notes are findable from the tree.
                    let notes = match app.comment_count(diff_index) {
                        0 => String::new(),
                        n => format!("💬{n} "),
                    };

                    // Right-align the "+a -b" badge: pad between the name and badge.
                    let icon_w = if icon.is_empty() {
                        0
                    } else {
                        icon.chars().count() + 1
                    };
                    // The speech bubble is double-width, so the badge is one
                    // column wider than its char count.
                    let notes_w = if notes.is_empty() {
                        0
                    } else {
                        notes.chars().count() + 1
                    };
                    let badge = notes_w + adds.len() + 1 + dels.len();
                    // The badge gets its columns before the name does. A name too
                    // long for the pane is cut short rather than shoving the
                    // counts off the right edge, where the widget would simply
                    // clip them — those counts, and the note badge especially, are
                    // what the tree is scanned for, and a hidden one reads as no
                    // comments at all.
                    let fixed = row.depth * 2 + 2 + icon_w + badge + 1;
                    let name = truncate(&row.name, inner.saturating_sub(fixed));
                    let left = row.depth * 2 + 2 + icon_w + name.chars().count();
                    let pad = inner.saturating_sub(left + badge).max(1);

                    // A reviewed file shows a green ✓ in place of its A/M/D sigil
                    // and dims its name — same column widths, so nothing shifts.
                    let (sigil, sigil_color) = if viewed {
                        ('✓', Color::Green)
                    } else {
                        (status.sigil(), status_color(status))
                    };
                    let name_style = if viewed {
                        Style::new().add_modifier(Modifier::DIM)
                    } else {
                        Style::new()
                    };

                    let mut spans = vec![
                        Span::raw(indent),
                        Span::styled(format!("{sigil} "), Style::new().fg(sigil_color)),
                    ];
                    if !icon.is_empty() {
                        spans.push(Span::styled(format!("{icon} "), name_style));
                    }
                    spans.push(Span::styled(name, name_style));
                    spans.push(Span::raw(" ".repeat(pad)));
                    if !notes.is_empty() {
                        spans.push(Span::styled(notes, Style::new().fg(Color::Yellow)));
                    }
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
        .block(
            Block::new()
                .borders(Borders::RIGHT)
                .border_style(border_style),
        )
        // Keep a few rows visible past the selection (like vim's `scrolloff`) so
        // the list scrolls before the cursor hits the edge and you can always see
        // what's coming next. Ratatui shrinks the padding on a short pane.
        .scroll_padding(SCROLL_PADDING)
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));

    frame.render_stateful_widget(list, area, &mut app.tree_state);
}

/// `name` cut to `room` columns, ending in an ellipsis when anything was
/// dropped so a truncated name doesn't read as the whole one. The tail is what
/// goes: file names are told apart by their beginnings far more often than by
/// their ends, and the extension is usually already given by the icon.
fn truncate(name: &str, room: usize) -> String {
    if name.chars().count() <= room {
        return name.to_string();
    }
    // Below two columns there's no room for a name *and* the mark that says it
    // was cut, so all that's left is the mark itself — or nothing.
    name.chars()
        .take(room.saturating_sub(1))
        .chain("…".chars().take(room.min(1)))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncation_never_exceeds_the_room_it_was_given() {
        assert_eq!(
            truncate("main.rs", 7),
            "main.rs",
            "an exact fit is untouched"
        );
        assert_eq!(truncate("main.rs", 9), "main.rs");
        assert_eq!(truncate("main.rs", 5), "main…");
        assert_eq!(truncate("main.rs", 1), "…");
        assert_eq!(truncate("main.rs", 0), "");
        for room in 0..10 {
            assert!(truncate("a_long_file_name.rs", room).chars().count() <= room);
        }
    }
}

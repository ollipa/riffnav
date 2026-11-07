//! The keybindings overlay.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::centered_rect;

pub(super) fn render_help(
    frame: &mut Frame,
    area: Rect,
    in_herdr: bool,
    has_forge: bool,
    auto_diff: bool,
    can_refresh: bool,
    comments: bool,
) {
    let mut entries = vec![
        ("j / k", "move selection / scroll diff (per focus)"),
        ("n / p", "next / previous file"),
        ("J / K", "next / previous change"),
        ("/", "search the text of the whole diff"),
        ("n / N", "next / previous search match"),
        ("Ctrl-d / Ctrl-u", "scroll diff half page"),
        ("Space / b", "page diff down / up"),
        ("PgDn / PgUp", "page down / up (focused pane)"),
        ("g / G", "top / bottom of diff"),
        ("Enter", "expand / collapse folder"),
        ("Tab / ← / →", "switch focus tree <-> diff"),
        ("t", "fuzzy find a file"),
        ("s", "toggle side-by-side / unified"),
        ("e", "toggle file tree"),
        ("i", "cycle icon style (nerd/unicode/ascii)"),
        ("T", "switch diff theme (github-dark/github-light)"),
        ("y", "copy file path"),
        ("o", "open file in $EDITOR"),
        ("v / V", "mark viewed / jump to next unviewed"),
        ("m", "other end of a move or copy"),
    ];
    if comments {
        entries.push(("c", "comment on the line (or reply, inside a thread)"));
        entries.push(("x", "delete the comment under the cursor (asks first)"));
        entries.push(("] / [", "next / previous comment"));
    }
    if can_refresh {
        entries.push(("r", "refresh the diff"));
        entries.push(("+ / -", "more / fewer context lines"));
    }
    if auto_diff {
        entries.push(("d", "cycle git diff source"));
        entries.push((
            "1 - 5",
            "all vs base / all uncommitted / staged / unstaged / branch vs base",
        ));
    }
    if has_forge {
        entries.push(("W", "open PR diff in browser"));
    }
    if in_herdr {
        entries.push(("z", "toggle herdr zoom"));
    }
    entries.push(("?", "toggle this help"));
    entries.push(("q / Esc", "quit (Esc clears a search first)"));
    let lines: Vec<Line> = entries
        .iter()
        .map(|(key, desc)| {
            Line::from(vec![
                Span::styled(
                    format!(" {key:<16}"),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{desc} ")),
            ])
        })
        .collect();

    let popup = centered_rect(52, entries.len() as u16 + 2, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keybindings (?/Esc to close) ")),
        popup,
    );
}

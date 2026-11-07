//! The keybindings overlay.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::centered_rect;

/// Which of the keys that come and go are live.
pub(super) struct Live {
    pub in_herdr: bool,
    pub has_forge: bool,
    pub auto_diff: bool,
    pub can_refresh: bool,
    pub comments: bool,
    pub can_send: bool,
}

pub(super) fn render_help(frame: &mut Frame, area: Rect, live: Live) {
    let Live {
        in_herdr,
        has_forge,
        auto_diff,
        can_refresh,
        comments,
        can_send,
    } = live;
    // With comments on, the diff has a line cursor for j/k to move.
    let move_keys = if comments {
        "move selection / line cursor (per focus)"
    } else {
        "move selection / scroll diff (per focus)"
    };
    let mut entries = vec![
        ("j / k  ↑ / ↓", move_keys),
        ("n / p", "next / previous file"),
        ("J / K", "next / previous change"),
        ("/", "search the text of the whole diff"),
        ("n / N", "next / previous search match"),
        ("Ctrl-d / Ctrl-u", "scroll diff half page"),
        ("Space / b", "page diff down / up"),
        ("PgDn / PgUp", "page down / up (focused pane)"),
        ("g / G", "top / bottom of diff"),
        ("Enter", "expand / collapse folder"),
        ("Tab", "switch focus tree <-> diff"),
        ("← / →", "focus the tree / the diff"),
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
    if can_send && comments {
        entries.push(("S", "send new comments to the agent in this herdr tab"));
    }
    entries.push(("?", "toggle this help"));
    entries.push(("q / Esc / Ctrl-c", "quit (Esc clears a search first)"));

    // As wide as the longest entry needs, and no wider than the screen.
    let keys = entries
        .iter()
        .map(|(key, _)| key.width())
        .max()
        .unwrap_or(0);
    let descs = entries
        .iter()
        .map(|(_, desc)| desc.width())
        .max()
        .unwrap_or(0);
    let inner = (1 + keys + 2 + descs + 1).max(TITLE.width());
    let width = (inner + 2).min(usize::from(area.width));
    let inner_width = width.saturating_sub(2);
    let mut lines = help_lines(&entries, keys, inner_width);
    // A screen too short for them all says there's more, rather than just ending.
    let room = usize::from(area.height).saturating_sub(2);
    if lines.len() > room && room > 0 {
        lines.truncate(room - 1);
        let more = Style::new().add_modifier(Modifier::DIM);
        let said = " … (a taller window shows the rest)";
        let said = if said.width() <= inner_width {
            said
        } else {
            " …"
        };
        lines.push(Line::from(Span::styled(said, more)));
    }
    let height = (lines.len() + 2).min(usize::from(area.height));
    let popup = centered_rect(width as u16, height as u16, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(TITLE)),
        popup,
    );
}

const TITLE: &str = " Keybindings (?/Esc to close) ";

/// The entries laid out `inner` columns wide, each key in a column `keys` wide
/// and its description beside it, wrapped at spaces to fit what's left. Too
/// narrow for that leaves no room worth having for the descriptions, so each
/// one goes under its key instead.
fn help_lines(entries: &[(&str, &str)], keys: usize, inner: usize) -> Vec<Line<'static>> {
    let key_style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let beside = inner.saturating_sub(1 + keys + 2 + 1);
    let stacked = beside < MIN_DESCRIPTION;
    let mut lines = Vec::new();
    for (key, desc) in entries {
        let (indent, room) = if stacked {
            lines.push(Line::from(Span::styled(format!(" {key}"), key_style)));
            (3, inner.saturating_sub(3 + 1))
        } else {
            (1 + keys + 2, beside)
        };
        for (at, part) in wrap_words(desc, room).into_iter().enumerate() {
            let lead = if at == 0 && !stacked {
                let pad = keys - key.width();
                Span::styled(format!(" {key}{}  ", " ".repeat(pad)), key_style)
            } else {
                Span::raw(" ".repeat(indent))
            };
            lines.push(Line::from(vec![lead, Span::raw(part)]));
        }
    }
    lines
}

/// Fewer columns than this for the descriptions puts them under their keys.
const MIN_DESCRIPTION: usize = 24;

/// `text` broken at spaces into lines of at most `cols` columns. A word longer
/// than that is cut, so nothing is lost off the edge.
fn wrap_words(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let mut word = word.to_string();
        while word.width() > cols {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let cut = cut_at(&word, cols);
            lines.push(word[..cut].to_string());
            word = word[cut..].to_string();
        }
        if !line.is_empty() && line.width() + 1 + word.width() > cols {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&word);
    }
    lines.push(line);
    lines
}

/// The byte offset of the end of the first `cols` columns of `text`.
fn cut_at(text: &str, cols: usize) -> usize {
    let mut used = 0;
    for (at, c) in text.char_indices() {
        used += c.width().unwrap_or(0);
        if used > cols {
            return at.max(c.len_utf8().min(text.len()));
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    /// The help drawn on a `width` by `height` screen with everything on, row
    /// by row.
    fn drawn(width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| {
                let live = Live {
                    in_herdr: true,
                    has_forge: true,
                    auto_diff: true,
                    can_refresh: true,
                    comments: true,
                    can_send: true,
                };
                render_help(f, f.area(), live)
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    /// Every word of every description is on screen, whatever the width.
    #[test]
    fn nothing_is_cut_off_at_any_width() {
        for width in [200, 90, 60, 40, 24] {
            let rows = drawn(width, 200);
            let squeezed = |text: &str| -> String {
                text.chars()
                    .filter(|c| !c.is_whitespace() && *c != '│')
                    .collect()
            };
            let screen = squeezed(&rows.concat());
            let wanted = [
                "all vs base / all uncommitted / staged / unstaged / branch vs base",
                "switch diff theme (github-dark/github-light)",
                "delete the comment under the cursor (asks first)",
                "move selection / line cursor (per focus)",
                "open PR diff in browser",
                "send new comments to the agent in this herdr tab",
            ];
            for desc in wanted {
                assert!(
                    screen.contains(&squeezed(desc)),
                    "{desc:?} at {width}: {rows:#?}"
                );
            }
        }
    }

    #[test]
    fn the_box_fits_its_longest_entry_and_no_more() {
        let rows = drawn(200, 80);
        let top = rows.iter().find(|row| row.contains('┌')).unwrap();
        let width = top.trim().chars().count();
        // The key column, two spaces and the longest description, inside a margin.
        let longest = "all vs base / all uncommitted / staged / unstaged / branch vs base";
        assert_eq!(
            width,
            2 + 1 + "q / Esc / Ctrl-c".len() + 2 + longest.len() + 1
        );
    }

    #[test]
    fn a_short_screen_says_there_is_more() {
        for width in [100, 30] {
            let rows = drawn(width, 12);
            assert!(rows[10].contains('…'), "{rows:#?}");
            assert!(rows[11].contains('└'), "{rows:#?}");
        }
    }
}

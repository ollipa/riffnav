mod diffview;
mod filetree;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use crate::app::{App, Focus};

pub fn draw(frame: &mut Frame, app: &mut App, diff_width: u16) {
    let tree_focused = app.focus == Focus::Tree;
    let diff_focused = app.focus == Focus::Diff;

    // The header and footer collapse to zero rows when disabled in config.
    let header_h = if app.show_header { 1 } else { 0 };
    let footer_h = if app.show_footer { 1 } else { 0 };
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Min(0),
        Constraint::Length(footer_h),
    ])
    .areas(frame.area());

    // The diff pane fills the body, minus the tree when it's shown.
    let diff_area = if app.show_tree {
        let [tree_area, diff_area] =
            Layout::horizontal([Constraint::Length(app.tree_width), Constraint::Min(0)])
                .areas(body);
        filetree::render(frame, tree_area, app, tree_focused);
        diff_area
    } else {
        body
    };

    let [diff_title, diff_body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(diff_area);

    if app.show_header {
        render_header(frame, header, app);
    }
    render_diff_title(frame, diff_title, app, diff_focused);
    diffview::render(frame, diff_body, app, diff_width);
    if app.show_footer {
        render_footer(frame, footer, app);
    }

    // Overlays (mutually exclusive): the finder takes precedence over help.
    if app.finder.is_some() {
        render_finder(frame, frame.area(), app);
    } else if app.show_help {
        render_help(frame, frame.area(), app.in_herdr());
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let (adds, dels) = app.totals();
    let mode = if app.side_by_side {
        "side-by-side"
    } else {
        "unified"
    };
    let mut spans = vec![
        Span::styled(
            " riffnav ",
            Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        Span::raw(format!("  {} files   ", app.files.len())),
        Span::styled(format!("+{adds}"), Style::new().fg(Color::Green)),
        Span::raw("  "),
        Span::styled(format!("-{dels}"), Style::new().fg(Color::Red)),
        Span::styled(
            format!("    {mode}"),
            Style::new().add_modifier(Modifier::DIM),
        ),
    ];
    if app.is_watching() {
        spans.push(Span::styled("   ● watch", Style::new().fg(Color::Green)));
    }
    if app.in_herdr() {
        spans.push(Span::styled(
            "   ⧉ herdr",
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_diff_title(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let title = match app.selected_file() {
        Some(idx) => format!(" {} ", app.files[idx].path()),
        None => " (directory) ".to_string(),
    };
    let mut style = Style::new().add_modifier(Modifier::BOLD);
    if focused {
        style = style.fg(Color::Cyan);
    }
    frame.render_widget(Paragraph::new(Line::from(title)).style(style), area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let (text, style) = match &app.status {
        Some(status) => (format!(" {status} "), Style::new().fg(Color::Yellow)),
        None => {
            let zoom = if app.in_herdr() { "z: zoom · " } else { "" };
            (
                format!(" j/k · n/p file · t: find · {zoom}Tab focus · ?: help · q: quit "),
                Style::new().add_modifier(Modifier::DIM),
            )
        }
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

fn render_help(frame: &mut Frame, area: Rect, in_herdr: bool) {
    let mut entries = vec![
        ("j / k", "move selection / scroll diff (per focus)"),
        ("n / p", "next / previous file"),
        ("Ctrl-d / Ctrl-u", "scroll diff half page"),
        ("PgDn / PgUp", "page down / up (per focus)"),
        ("g / G", "top / bottom of diff"),
        ("Enter / Space", "expand / collapse folder"),
        ("Tab / ← / →", "switch focus tree <-> diff"),
        ("t / /", "fuzzy find a file"),
        ("s", "toggle side-by-side / unified"),
        ("e", "toggle file tree"),
        ("i", "cycle icon style (nerd/unicode/ascii)"),
        ("y", "copy file path"),
        ("o", "open file in $EDITOR"),
    ];
    if in_herdr {
        entries.push(("z", "toggle herdr zoom"));
    }
    entries.push(("?", "toggle this help"));
    entries.push(("q / Esc", "quit"));
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

fn render_finder(frame: &mut Frame, area: Rect, app: &App) {
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
                Span::raw(file.path().to_string()),
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

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::diff::{FileDiff, FileStatus};
    use crate::icons::IconStyle;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::text::Text;

    fn file(path: &str, status: FileStatus, additions: u32, deletions: u32) -> FileDiff {
        FileDiff {
            old_path: None,
            new_path: Some(path.to_string()),
            status,
            additions,
            deletions,
            raw: String::new(),
        }
    }

    /// Render the buffer to plain text (symbols only, trailing space trimmed) so
    /// snapshots are stable and readable regardless of styling.
    fn buffer_text(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                line.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    fn sample_app(cfg: &Config) -> App {
        let files = vec![
            file("README.md", FileStatus::Modified, 3, 1),
            file("src/main.rs", FileStatus::Modified, 12, 4),
            file("src/diff/parser.rs", FileStatus::Added, 40, 0),
        ];
        App::new(files, false, false, cfg)
    }

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let diff_width = width.saturating_sub(app.tree_width);
        terminal.draw(|f| draw(f, app, diff_width)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn renders_tree_and_layout() {
        // ASCII icons keep the snapshot free of private-use glyphs.
        let cfg = Config {
            icon_style: IconStyle::Ascii,
            ..Config::default()
        };
        let mut app = sample_app(&cfg);
        insta::assert_snapshot!(render(&mut app, 64, 12));
    }

    #[test]
    fn hides_tree_when_disabled() {
        let cfg = Config {
            icon_style: IconStyle::Ascii,
            show_tree: false,
            ..Config::default()
        };
        let mut app = sample_app(&cfg);
        insta::assert_snapshot!(render(&mut app, 64, 8));
    }

    /// delta leaves unified diffs unwrapped, so a line wider than the pane must
    /// be wrapped by the viewer rather than truncated at the edge. Regression
    /// test for long markdown lines losing their tail.
    #[test]
    fn unified_long_line_wraps_not_truncated() {
        let cfg = Config {
            icon_style: IconStyle::Ascii,
            ..Config::default()
        };
        let mut app = sample_app(&cfg); // unified mode, first file selected
        let (width, height) = (64, 12);
        let diff_width = width - app.tree_width; // the pane delta wraps to

        // One line several pane-widths long, with no spaces so wrapping has to
        // break mid-token — exactly the case delta truncates in side-by-side.
        let long = "X".repeat(diff_width as usize * 3 + 5);
        let idx = app.selected_file().expect("a file is selected");
        app.cache
            .insert_for_test(idx, diff_width, false, Text::from(long.clone()));

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| draw(f, &mut app, diff_width)).unwrap();
        let out = buffer_text(terminal.backend().buffer());

        let shown = out.chars().filter(|&c| c == 'X').count();
        assert_eq!(
            shown,
            long.len(),
            "every column must survive wrapping; truncation would drop the tail\n{out}"
        );
    }
}

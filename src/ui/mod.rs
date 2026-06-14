mod diffview;
mod filetree;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use crate::app::{App, Focus, TREE_WIDTH};

pub fn draw(frame: &mut Frame, app: &mut App, diff_width: u16) {
    let tree_focused = app.focus == Focus::Tree;
    let diff_focused = app.focus == Focus::Diff;

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // The diff pane fills the body, minus the tree when it's shown.
    let diff_area = if app.show_tree {
        let [tree_area, diff_area] =
            Layout::horizontal([Constraint::Length(TREE_WIDTH), Constraint::Min(0)]).areas(body);
        filetree::render(frame, tree_area, app, tree_focused);
        diff_area
    } else {
        body
    };

    let [diff_title, diff_body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(diff_area);

    render_header(frame, header, app);
    render_diff_title(frame, diff_title, app, diff_focused);
    diffview::render(frame, diff_body, app, diff_width);
    render_footer(frame, footer, app);

    // Overlays (mutually exclusive): the finder takes precedence over help.
    if app.finder.is_some() {
        render_finder(frame, frame.area(), app);
    } else if app.show_help {
        render_help(frame, frame.area());
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let (adds, dels) = app.totals();
    let mode = if app.side_by_side { "side-by-side" } else { "unified" };
    let line = Line::from(vec![
        Span::styled(
            " riffnav ",
            Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        Span::raw(format!("  {} files   ", app.files.len())),
        Span::styled(format!("+{adds}"), Style::new().fg(Color::Green)),
        Span::raw("  "),
        Span::styled(format!("-{dels}"), Style::new().fg(Color::Red)),
        Span::styled(format!("    {mode}"), Style::new().add_modifier(Modifier::DIM)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
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
        None => (
            " j/k · n/p file · t: find · Tab focus · ?: help · q: quit ".to_string(),
            Style::new().add_modifier(Modifier::DIM),
        ),
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let entries = [
        ("j / k", "move selection / scroll diff (per focus)"),
        ("n / p", "next / previous file"),
        ("Ctrl-d / Ctrl-u", "scroll diff half page"),
        ("g / G", "top / bottom of diff"),
        ("Enter / Space", "expand / collapse folder"),
        ("Tab", "switch focus tree <-> diff"),
        ("t / /", "fuzzy find a file"),
        ("s", "toggle side-by-side / unified"),
        ("e", "toggle file tree"),
        ("i", "cycle icon style (nerd/unicode/ascii)"),
        ("y", "copy file path"),
        ("o", "open file in $EDITOR"),
        ("?", "toggle this help"),
        ("q / Esc", "quit"),
    ];
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

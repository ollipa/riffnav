mod diffview;
mod filetree;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, TREE_WIDTH};

pub fn draw(frame: &mut Frame, app: &mut App, diff_width: u16) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let [tree_area, diff_area] =
        Layout::horizontal([Constraint::Length(TREE_WIDTH), Constraint::Min(0)]).areas(body);

    let [diff_title, diff_body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(diff_area);

    render_header(frame, header, app);
    filetree::render(frame, tree_area, app);
    render_diff_title(frame, diff_title, app);
    diffview::render(frame, diff_body, app, diff_width);
    render_footer(frame, footer);
}

fn render_diff_title(frame: &mut Frame, area: Rect, app: &App) {
    let title = match app.selected_file() {
        Some(idx) => format!(" {} ", app.files[idx].path()),
        None => " (directory) ".to_string(),
    };
    frame.render_widget(
        Paragraph::new(Line::from(title)).style(Style::new().add_modifier(Modifier::BOLD)),
        area,
    );
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let (adds, dels) = app.totals();
    let line = Line::from(vec![
        Span::styled(
            " riffnav ",
            Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ),
        Span::raw(format!("  {} files   ", app.files.len())),
        Span::styled(format!("+{adds}"), Style::new().fg(Color::Green)),
        Span::raw("  "),
        Span::styled(format!("-{dels}"), Style::new().fg(Color::Red)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_footer(frame: &mut Frame, area: Rect) {
    let hint = " j/k tree · n/p file · Ctrl-d/u scroll · g/G top/bottom · q quit ";
    frame.render_widget(
        Paragraph::new(Line::from(hint)).style(Style::new().add_modifier(Modifier::DIM)),
        area,
    );
}

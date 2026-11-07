//! The header, the diff's title, the search prompt and the footer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::diff::path::shown;

pub(super) fn render_header(frame: &mut Frame, area: Rect, app: &App) {
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
    let viewed = app.viewed_count();
    if viewed > 0 {
        spans.push(Span::styled(
            format!("   ✓ {viewed}/{} viewed", app.files.len()),
            Style::new().fg(Color::Green),
        ));
    }
    if let Some(label) = app.diff_label() {
        spans.push(Span::styled(
            format!("   ◆ {label}"),
            Style::new().fg(Color::Cyan),
        ));
    }
    if let Some(context) = app.diff_context() {
        spans.push(Span::styled(
            format!("   ±{context} context"),
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    let comments = app.comment_total();
    if comments > 0 {
        spans.push(Span::styled(
            format!("   💬 {comments}"),
            Style::new().fg(Color::Yellow),
        ));
    }
    if app.in_herdr() {
        spans.push(Span::styled(
            "   ⧉ herdr",
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(super) fn render_diff_title(frame: &mut Frame, area: Rect, app: &App, focused: bool) {
    let title = match app.selected_file() {
        Some(idx) => format!(" {} ", shown(app.files[idx].path())),
        None => " (directory) ".to_string(),
    };
    let mut style = Style::new().add_modifier(Modifier::BOLD);
    if focused {
        style = style.fg(Color::Cyan);
    }
    frame.render_widget(Paragraph::new(Line::from(title)).style(style), area);
}

/// The prompt: the query on the left, as much of its end as will fit, and how
/// it's getting on at the right. The terminal's own cursor marks where typing goes.
pub(super) fn render_search_prompt(
    frame: &mut Frame,
    area: Rect,
    query: &str,
    count: Option<String>,
) {
    let count = count.map_or_else(String::new, |count| format!("  {count} "));
    let count_width = count.width() as u16;
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(count_width)]).areas(area);

    // Room for the leading ` /` and for the cursor after the query.
    let room = usize::from(left.width).saturating_sub(3);
    let mut shown = query;
    while shown.width() > room {
        let mut chars = shown.chars();
        chars.next();
        shown = chars.as_str();
    }
    frame.render_widget(Paragraph::new(format!(" /{shown}")), left);
    frame.render_widget(
        Paragraph::new(count).style(Style::new().add_modifier(Modifier::DIM)),
        right,
    );
    if left.width > 0 && left.height > 0 {
        let x = left.x + 2 + shown.width() as u16;
        frame.set_cursor_position((x.min(left.right().saturating_sub(1)), left.y));
    }
}

/// What the footer says about a search in force, ahead of its usual text.
fn search_summary(app: &App) -> Option<String> {
    let query = app.search_query()?;
    let count = app.search_count().unwrap_or_default();
    Some(format!(" /{query}  {count} "))
}

pub(super) fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(summary) = search_summary(app) {
        let (text, style) = match &app.status {
            Some(status) => (
                format!("· {} ", shown(status)),
                Style::new().fg(Color::Yellow),
            ),
            None => (
                "· n/N: next/previous match · Esc: clear search ".to_string(),
                Style::new().add_modifier(Modifier::DIM),
            ),
        };
        let line = Line::from(vec![
            Span::styled(summary, Style::new().fg(Color::Yellow)),
            Span::styled(text, style),
        ]);
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    let (text, style) = match &app.status {
        Some(status) => (
            format!(" {} ", shown(status)),
            Style::new().fg(Color::Yellow),
        ),
        None => {
            let src = if app.can_cycle_source() {
                "d: source · "
            } else {
                ""
            };
            let refresh = if app.can_refresh() {
                "r: refresh · "
            } else {
                ""
            };
            let web = if app.has_forge() { "W: web · " } else { "" };
            let zoom = if app.in_herdr() { "z: zoom · " } else { "" };
            let note = if app.comments_enabled() {
                "c: comment · "
            } else {
                ""
            };
            let keys = |changes: &str| {
                format!(
                    " j/k · n/p file · {changes}v: viewed · {note}{refresh}{src}/: search · t: find · T: theme · {web}{zoom}Tab focus · ?: help · q: quit "
                )
            };
            // Only if it costs nothing: on a narrow screen `?` is worth more.
            let full = keys("J/K change · ");
            let fits = full.chars().count() <= usize::from(area.width);
            (
                if fits { full } else { keys("") },
                Style::new().add_modifier(Modifier::DIM),
            )
        }
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

mod diffview;
mod filetree;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Padding, Paragraph};

use crate::app::{App, Focus};

/// Tallest the comment composer grows before it scrolls internally, so a long
/// note never swallows the diff it's about.
const COMPOSER_MAX_ROWS: u16 = 8;

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
        // Remember the tree rect so mouse events can hit-test rows.
        app.tree_area = Some(tree_area);
        filetree::render(frame, tree_area, app, tree_focused);
        diff_area
    } else {
        app.tree_area = None;
        body
    };

    let [diff_title, diff_body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(diff_area);
    // Remember the diff body rect for click-to-focus and wheel scrolling.
    app.diff_area = Some(diff_body);

    if app.show_header {
        render_header(frame, header, app);
    }
    render_diff_title(frame, diff_title, app, diff_focused);
    diffview::render(frame, diff_body, app, diff_width);
    if app.show_footer {
        render_footer(frame, footer, app);
    }
    // Drawn inside the diff pane rather than as a screen overlay: the note is
    // being written about a line that has to stay visible next to it.
    render_composer(frame, diff_body, app);

    // Overlays (mutually exclusive): the finder takes precedence over help.
    if app.finder.is_some() {
        render_finder(frame, frame.area(), app);
    } else if app.show_help {
        render_help(
            frame,
            frame.area(),
            app.in_herdr(),
            app.has_forge(),
            app.can_cycle_source(),
            app.can_refresh(),
            app.comments_enabled(),
        );
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
            (
                format!(
                    " j/k · n/p file · v: viewed · {note}{refresh}{src}t: find · T: theme · {web}{zoom}Tab focus · ?: help · q: quit "
                ),
                Style::new().add_modifier(Modifier::DIM),
            )
        }
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}

fn render_help(
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
        ("Ctrl-d / Ctrl-u", "scroll diff half page"),
        ("Space / b", "page diff down / up"),
        ("PgDn / PgUp", "page down / up (focused pane)"),
        ("g / G", "top / bottom of diff"),
        ("Enter", "expand / collapse folder"),
        ("Tab / ← / →", "switch focus tree <-> diff"),
        ("t / /", "fuzzy find a file"),
        ("s", "toggle side-by-side / unified"),
        ("e", "toggle file tree"),
        ("i", "cycle icon style (nerd/unicode/ascii)"),
        ("T", "cycle diff theme (delta/github-dark/github-light)"),
        ("y", "copy file path"),
        ("o", "open file in $EDITOR"),
        ("v / V", "mark viewed / jump to next unviewed"),
    ];
    if comments {
        entries.push(("c", "comment on the line (or reply, inside a thread)"));
        entries.push(("x", "delete the comment under the cursor"));
        entries.push(("] / [", "next / previous comment"));
    }
    if can_refresh {
        entries.push(("r", "refresh the diff"));
    }
    if auto_diff {
        entries.push(("d", "cycle git diff source"));
    }
    if has_forge {
        entries.push(("W", "open PR diff in browser"));
    }
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

/// The keys that end a note, in the longest form the frame can hold: clipping
/// the hint would hide the very key that closes the field.
fn composer_hint(width: u16) -> &'static str {
    let room = width.saturating_sub(2) as usize;
    [
        " Enter save · Shift-Enter newline · Esc cancel · Ctrl-O $EDITOR ",
        " Enter save · S-Enter newline · Esc cancel · ^O editor ",
        " Enter save · S-Enter newline · Esc cancel ",
        " Enter save · Esc cancel ",
        " Enter · Esc ",
    ]
    .into_iter()
    .find(|hint| hint.chars().count() <= room)
    .unwrap_or("")
}

/// The note being typed, as a bordered field pinned to the line it will hang on:
/// below that line where there's room, otherwise above it, so the code under
/// discussion stays on screen while it's written. A no-op when nothing is being
/// composed.
fn render_composer(frame: &mut Frame, area: Rect, app: &App) {
    let Some(composer) = app.composer() else {
        return;
    };
    // Two border columns and a column of padding on each side.
    let Some(cols) = area.width.checked_sub(4).filter(|c| *c > 0) else {
        return;
    };
    if area.height < 3 {
        return;
    }
    let layout = composer.layout(cols as usize);

    // As tall as what's typed, within what the pane can spare; a longer note
    // scrolls inside the field.
    let max_body = area.height.saturating_sub(2).min(COMPOSER_MAX_ROWS);
    let height = (layout.rows.len() as u16).clamp(1, max_body) + 2;
    let y = match app.cursor_rows() {
        Some((_, bottom)) if bottom + height <= area.height => area.y + bottom,
        Some((top, _)) if top >= height => area.y + top - height,
        // The cursor is off-screen, or hemmed in on both sides: fall back to the
        // foot of the pane, where the field can't cover the line being annotated
        // any worse than it already would.
        _ => area.bottom().saturating_sub(height),
    };
    let popup = Rect {
        x: area.x,
        y,
        width: area.width,
        height,
    };

    // Framed like the box this note will become once it's saved, in the same
    // theme accent, so the field reads as the comment taking shape in place.
    let accent = Style::new().fg(crate::comment::render::accent(app.diff_theme));
    let block = Block::bordered()
        .border_set(border::ROUNDED)
        .border_style(accent)
        .title(Span::styled(
            composer.title(),
            accent.add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            composer_hint(popup.width),
            Style::new().add_modifier(Modifier::DIM),
        ))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    // Keep the cursor's row in view when the note outgrows the field.
    let first = layout
        .cursor
        .0
        .saturating_sub(inner.height.saturating_sub(1) as usize);
    let lines: Vec<Line> = layout
        .rows
        .iter()
        .skip(first)
        .take(inner.height as usize)
        .map(|row| Line::from(row.clone()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);

    // The terminal's own cursor marks the insertion point — ratatui shows it
    // only for the frames that ask for a position, which is exactly these.
    frame.set_cursor_position((
        inner.x + (layout.cursor.1 as u16).min(inner.width.saturating_sub(1)),
        inner.y + (layout.cursor.0 - first) as u16,
    ));
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

    /// Moving down a long list must scroll before the selection reaches the last
    /// visible row, so the files still ahead stay in sight.
    #[test]
    fn tree_keeps_rows_visible_below_the_selection() {
        let cfg = Config {
            icon_style: IconStyle::Ascii,
            ..Config::default()
        };
        let files: Vec<_> = (0..30)
            .map(|i| file(&format!("file{i:02}.rs"), FileStatus::Modified, 1, 1))
            .collect();
        let mut app = App::new(files, false, false, &cfg);
        app.tree_state.select(Some(20));

        let out = render(&mut app, 64, 12);
        assert!(
            out.contains("file20.rs"),
            "selection must be visible\n{out}"
        );
        assert!(
            out.contains("file23.rs"),
            "rows past the selection must stay visible\n{out}"
        );
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
        app.cache.insert_for_test(
            idx,
            diff_width,
            false,
            app.diff_theme,
            Text::from(long.clone()),
        );

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

    /// An app whose selected file has a six-line diff and the given comments on
    /// it. The seeded gutter matches what delta emits under riffnav's pinned
    /// number formats, so `LineMap` has real numbers to anchor against.
    fn app_with_comments(comments: &[(u32, &str, &str)]) -> (App, u16, u16) {
        let cfg = Config {
            icon_style: IconStyle::Ascii,
            ..Config::default()
        };
        let mut app = sample_app(&cfg);
        let (width, height) = (72, 12);
        let idx = app.selected_file().expect("a file is selected");
        let path = app.files[idx].path().to_string();

        let mut store = crate::comment::CommentStore::disabled();
        for (line, author, body) in comments {
            store.add(crate::comment::Comment {
                id: String::new(),
                file: path.clone(),
                side: crate::comment::Side::New,
                line: *line,
                body: body.to_string(),
                author: author.to_string(),
                created: crate::state::now_unix(),
                reply_to: None,
                diff_hash: None,
            });
        }
        app.install_comments_for_test(store);

        let text = Text::from(
            (1..=6)
                .map(|n| Line::from(format!("{n:>5}⋮{n:>5}│let x{n} = {n};")))
                .collect::<Vec<_>>(),
        );
        app.seed_render_for_test(width - app.tree_width, text);
        (app, width, height)
    }

    /// A comment must land on the row under the line it names, pushing the rest of
    /// the diff down rather than covering it.
    #[test]
    fn comment_renders_beneath_its_anchor_line() {
        let (mut app, width, height) = app_with_comments(&[(3, "claude", "why the retry here?")]);
        let out = render(&mut app, width, height);

        let rows: Vec<&str> = out.lines().collect();
        let anchor = rows
            .iter()
            .position(|r| r.contains("let x3 = 3;"))
            .expect("the anchored line is on screen");
        assert!(
            rows[anchor + 1].contains("claude"),
            "the comment header must follow line 3\n{out}"
        );
        assert!(rows[anchor + 2].contains("why the retry here?"), "{out}");
        assert!(
            rows[anchor + 4].contains("let x4 = 4;"),
            "the diff must resume after the comment's box closes\n{out}"
        );
    }

    #[test]
    fn commented_files_show_a_count_in_the_tree() {
        let (mut app, width, height) = app_with_comments(&[(2, "a", "one"), (4, "b", "two")]);
        let out = render(&mut app, width, height);
        // The speech bubble is double-width, so it occupies two buffer cells and
        // the count lands one column after it.
        assert!(
            out.contains("💬 2 +40 -0"),
            "the tree badge must count both comments, left of the ± badge\n{out}"
        );
    }

    /// `c` opens a field over the diff pane, right under the line being
    /// annotated, and the note is typed into it — no editor, no lost screen.
    #[test]
    fn the_composer_opens_as_a_field_under_the_cursor_line() {
        use ratatui::crossterm::event::KeyCode;

        let (mut app, width, height) = app_with_comments(&[]);
        render(&mut app, width, height); // gives the pane a height to place in
        app.press_for_test(KeyCode::Char('c'), false);
        for ch in "why the retry?".chars() {
            app.press_for_test(KeyCode::Char(ch), false);
        }
        let out = render(&mut app, width, height);

        let rows: Vec<&str> = out.lines().collect();
        let top = rows
            .iter()
            .position(|r| r.contains("Comment on src/diff/parser.rs:1"))
            .expect("the field names the line it will hang on");
        assert!(
            rows[top - 1].contains("let x1 = 1;"),
            "the field opens directly under the anchored line\n{out}"
        );
        assert!(rows[top + 1].contains("why the retry?"), "{out}");
        assert!(
            rows[top + 2].contains("save") && rows[top + 2].contains("cancel"),
            "the keys that end the note are on the frame\n{out}"
        );

        // Saving stores the note and closes the field.
        app.press_for_test(KeyCode::Char('s'), true);
        let out = render(&mut app, width, height);
        assert!(!out.contains("save · Esc"), "the field closed\n{out}");
        assert_eq!(app.comment_total(), 1, "the note was stored\n{out}");
    }

    /// With no room below the anchored line, the field opens above it instead —
    /// covering the line being annotated would defeat the point of typing there.
    #[test]
    fn the_composer_moves_above_the_line_when_it_cannot_fit_below() {
        use ratatui::crossterm::event::KeyCode;

        let (mut app, width, _) = app_with_comments(&[]);
        let height = 9; // six diff lines in a pane with nothing to spare
        render(&mut app, width, height);
        app.press_for_test(KeyCode::Char('G'), false); // cursor to the last line
        app.press_for_test(KeyCode::Char('c'), false);
        let out = render(&mut app, width, height);

        let rows: Vec<&str> = out.lines().collect();
        let top = rows
            .iter()
            .position(|r| r.contains("Comment on"))
            .expect("the field is on screen");
        let anchored = rows
            .iter()
            .position(|r| r.contains("let x6 = 6;"))
            .expect("the anchored line stays visible");
        assert!(
            anchored > top + 2,
            "the field sits above the line it annotates\n{out}"
        );
    }

    #[test]
    fn the_composer_hint_shrinks_rather_than_being_clipped() {
        assert!(composer_hint(80).contains("Shift-Enter newline"));
        assert!(composer_hint(40).contains("Enter save"));
        // Whatever survives must still fit between the frame's corners.
        for width in 0..80u16 {
            let hint = composer_hint(width);
            assert!(
                hint.chars().count() <= width.saturating_sub(2) as usize,
                "hint {hint:?} overflows a {width}-wide frame"
            );
        }
    }

    /// `]` steps to the next comment; the cursor landing on it is what `c` and `x`
    /// then act on.
    #[test]
    fn jumping_steps_the_cursor_through_the_comments_and_wraps() {
        let (mut app, width, height) = app_with_comments(&[(2, "a", "first"), (5, "b", "second")]);
        render(&mut app, width, height); // gives the pane a height to scroll within

        let idx = app.selected_file().unwrap();
        let starts: Vec<usize> = app
            .cache
            .get(idx, width - app.tree_width, false, app.diff_theme)
            .expect("seeded render")
            .comment_rows
            .iter()
            .map(|b| b.start)
            .collect();
        // Line 2 is index 1, so its block starts right after it; the second
        // block is pushed down by the three rows the first one added.
        assert_eq!(starts, vec![2, 8], "one block per anchored thread");

        app.jump_comment(true);
        assert_eq!(app.diff_cursor, starts[0]);
        app.jump_comment(true);
        assert_eq!(app.diff_cursor, starts[1]);
        // Past the last one, wrap back to the first.
        app.jump_comment(true);
        assert_eq!(app.diff_cursor, starts[0]);
        // And backwards from the first wraps to the last.
        app.jump_comment(false);
        assert_eq!(app.diff_cursor, starts[1]);
    }
}

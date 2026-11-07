mod bars;
mod composer;
mod diffview;
mod filetree;
mod finder;
mod help;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};

use self::bars::{render_diff_title, render_footer, render_header, render_search_prompt};
use self::composer::render_composer;
use self::finder::render_finder;
use self::help::render_help;
use crate::app::{App, Focus};

pub fn draw(frame: &mut Frame, app: &mut App, diff_width: u16) {
    let tree_focused = app.focus == Focus::Tree;
    let diff_focused = app.focus == Focus::Diff;

    // The header and footer collapse to zero rows when disabled in config.
    let header_h = if app.show_header { 1 } else { 0 };
    // A search needs somewhere to say what it's doing, config or no config.
    let searching = app.search_prompt().is_some() || app.search_query().is_some();
    let footer_h = if app.show_footer || searching { 1 } else { 0 };
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
    if let Some(query) = app.search_prompt() {
        render_search_prompt(frame, footer, query, app.search_count());
    } else if app.show_footer || searching {
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
    use crate::comment::Comment;
    use crate::comment::testing::note;
    use crate::config::Config;
    use crate::diff::testing::bare;
    use crate::diff::{FileDiff, FileStatus};
    use crate::icons::IconStyle;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::text::{Line, Text};

    fn file(path: &str, status: FileStatus, additions: u32, deletions: u32) -> FileDiff {
        FileDiff {
            additions,
            deletions,
            ..bare(path, status)
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
        App::new(files, false, cfg)
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
        let mut app = App::new(files, false, &cfg);
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

    /// Unified diffs are left unwrapped, so a line wider than the pane must
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
        let diff_width = width - app.tree_width; // the diff pane

        // One line several pane-widths long, with no spaces so wrapping has to
        // break mid-token.
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
    /// it. The seeded gutter gives `LineMap` real numbers to anchor against.
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
            store.add(Comment {
                author: author.to_string(),
                created: crate::state::now_unix(),
                ..note(&path, *line, body)
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

    /// A name wider than the tree pane must give way to the badges, not push them
    /// off the edge: a clipped `💬` count reads as no comments at all.
    #[test]
    fn a_long_name_is_cut_short_so_its_badges_stay_on_screen() {
        let cfg = Config {
            icon_style: IconStyle::Ascii,
            ..Config::default()
        };
        let path = "a_file_whose_name_runs_well_past_the_pane_edge.rs";
        let mut app = App::new(vec![file(path, FileStatus::Modified, 40, 0)], false, &cfg);
        let mut store = crate::comment::CommentStore::disabled();
        store.add(Comment {
            author: "claude".to_string(),
            created: crate::state::now_unix(),
            ..note(path, 1, "a note")
        });
        app.install_comments_for_test(store);

        let out = render(&mut app, 72, 8);
        let row = out
            .lines()
            .find(|l| l.contains("a_file_whose"))
            .expect("the file is in the tree");
        // Everything left of the pane's border is the tree's own row.
        let row = row.split('│').next().expect("the tree pane's columns");
        assert!(row.contains("💬 1 +40 -0"), "the badges survive\n{out}");
        assert!(row.contains('…'), "and the name says it was cut\n{row}");
        assert!(
            row.chars().count() <= app.tree_width as usize,
            "the row still fits the pane\n{row}"
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

    /// Clicking a diff line puts the line cursor on it, so `c` then comments on
    /// the line that was clicked.
    #[test]
    fn clicking_a_diff_line_moves_the_cursor_there() {
        let (mut app, width, height) = app_with_comments(&[]);
        let out = render(&mut app, width, height);
        let area = app.diff_area.expect("the diff pane was laid out");

        let rows: Vec<&str> = out.lines().collect();
        let y = rows
            .iter()
            .position(|r| r.contains("let x4 = 4;"))
            .expect("line 4 is on screen") as u16;
        app.click(ratatui::layout::Position::new(area.x + 10, y));
        let out = render(&mut app, width, height);

        let line = app.selected_file().and_then(|idx| {
            let r = app
                .cache
                .get(idx, width - app.tree_width, false, app.diff_theme)?;
            Some(r.text.lines[app.diff_cursor].to_string())
        });
        assert!(
            line.is_some_and(|l| l.contains("let x4 = 4;")),
            "the cursor must land on the clicked line\n{out}"
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

    /// A reply's field opens under the *whole* thread, not under the one row the
    /// cursor happens to sit on: pinned to that row it would open through the
    /// middle of the box, hiding the very note being replied to.
    #[test]
    fn a_replys_field_opens_below_the_thread_it_answers() {
        use ratatui::crossterm::event::KeyCode;

        let (mut app, width, height) = app_with_comments(&[(2, "claude", "why the retry here?")]);
        render(&mut app, width, height);
        app.jump_comment(true); // onto the thread, where `c` replies
        app.press_for_test(KeyCode::Char('c'), false);
        let out = render(&mut app, width, height);
        let rows: Vec<&str> = out.lines().collect();
        let field = rows
            .iter()
            .position(|r| r.contains("Reply") || r.contains("Comment on"))
            .expect("the field is on screen");

        assert!(
            rows[..field]
                .iter()
                .any(|r| r.contains("why the retry here?")),
            "the note being replied to stays visible above the field\n{out}"
        );
        assert!(
            !rows[field..]
                .iter()
                .any(|r| r.contains("why the retry here?")),
            "and the field doesn't cut through the box\n{out}"
        );
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

    /// Arriving at a comment has to show the comment: `]` scrolls the whole box
    /// into view, not just the row the cursor lands on, or a note anchored near
    /// the foot of the pane is jumped to and still unreadable.
    #[test]
    fn jumping_brings_the_whole_comment_box_into_view() {
        // A short pane, and a thread deep enough that landing the cursor on its
        // first row alone would leave the rest of the box below the fold.
        let (mut app, width, _) = app_with_comments(&[
            (6, "claude", "why the retry here?"),
            (6, "ollipa", "the upstream flaps"),
            (6, "claude", "then it wants backoff"),
        ]);
        let height = 14;
        render(&mut app, width, height);

        app.jump_comment(true);
        let render = app
            .cache
            .get(
                app.selected_file().unwrap(),
                width - app.tree_width,
                false,
                app.diff_theme,
            )
            .expect("seeded render");
        let block = render.comment_rows.first().expect("the note is spliced in");
        let (top, bottom) = (
            render.row_of(block.start),
            render.row_of(block.start + block.len),
        );
        assert!(
            bottom > app.diff_height.into(),
            "the box must overrun the pane from the top, or this proves nothing"
        );
        assert!(
            top >= app.diff_scroll && bottom <= app.diff_scroll + u32::from(app.diff_height),
            "the whole box is on screen: rows {top}..{bottom}, \
             view {}..{}",
            app.diff_scroll,
            app.diff_scroll + u32::from(app.diff_height)
        );
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

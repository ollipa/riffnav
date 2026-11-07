//! The `m` key: from a moved or copied line to its other end.

use super::App;
use crate::comment::{Anchor, Side};
use crate::render::{Jump, Target};

/// An `m` jump on its way to a line, and whether it set out from that line's file.
#[derive(Clone, Copy)]
pub(super) struct MoveLanding {
    target: Target,
    same_file: bool,
}

/// Where an `m` onto a copy's other end came from, and where it landed.
#[derive(Clone, Copy)]
pub(super) struct CopyReturn {
    pub(super) from: Target,
    pub(super) landed: Target,
}

/// One end of what `m` can follow from a row.
pub(super) struct MoveEnd {
    pub(super) from: Target,
    pub(super) jump: Jump,
    /// The row's new line number and this end's side, if the row has an end on each.
    pub(super) turn: Option<(u32, Side)>,
}

impl App {
    /// What `m` should follow, and from where: the line under the cursor, or the
    /// first moved or copied line on screen when there's no cursor to aim with. On
    /// a side-by-side row with a move on both sides, the new side goes first, as it
    /// does for comments, and then the two take turns.
    pub(super) fn jump_target(&self) -> Option<MoveEnd> {
        let file = self.selected_file()?;
        let render = self.current_render()?;
        let rows = if self.comments_on {
            self.diff_cursor..self.diff_cursor + 1
        } else {
            let last = u32::from(self.diff_height.max(1)) - 1;
            let bottom = self.diff_scroll.saturating_add(last);
            render.line_at(self.diff_scroll)..render.line_at(bottom) + 1
        };
        rows.into_iter().find_map(|row| {
            let numbers = render.line_map.get(render.code_line(row));
            let end = |side, line: Option<u32>| {
                let anchor = Anchor { side, line: line? };
                let from = Target { file, anchor };
                let jump = self.cache.jump(file, anchor)?;
                Some(MoveEnd {
                    from,
                    jump,
                    turn: None,
                })
            };
            let (new, old) = (end(Side::New, numbers.new), end(Side::Old, numbers.old));
            match (new, old) {
                // Ends that face each other have nowhere to take turns going.
                (Some(new), Some(old)) if new.jump != Jump::Moved(old.from) => {
                    let line = new.from.anchor.line;
                    let last = self.move_turns.get(&(file, line));
                    let (mut end, side) = match last {
                        Some(Side::New) => (old, Side::Old),
                        _ => (new, Side::New),
                    };
                    end.turn = Some((line, side));
                    Some(end)
                }
                (new, old) => new.or(old),
            }
        })
    }

    /// Whether the cursor is where the last jump onto a copy put it.
    pub(super) fn on_copy_landing(&self, landed: Target) -> bool {
        let here = self.current_render().and_then(|render| {
            let line = render.code_line(self.diff_cursor);
            render.line_map.get(line).anchor()
        });
        self.selected_file() == Some(landed.file) && here == Some(landed.anchor)
    }

    /// Jump to the other end of a moved or copied line, or say where it is if the
    /// diff doesn't show it. Both ends point at each other, so pressing `m` again
    /// comes straight back. A row with an end on each side says which it took.
    pub(super) fn jump_move(&mut self) {
        if let Some(back) = self.copy_return.take()
            && self.on_copy_landing(back.landed)
        {
            return self.jump_to(back.from);
        }
        let Some(end) = self.jump_target() else {
            return self.set_status("Not on a moved or copied line");
        };
        let at = end.turn.map(|(line, side)| {
            self.move_turns.insert((end.from.file, line), side);
            if side == Side::New { 1 } else { 2 }
        });
        let what = if matches!(end.jump, Jump::Moved(_)) {
            "Moved"
        } else {
            "Copied"
        };
        match end.jump {
            Jump::Moved(target) => self.jump_to(target),
            Jump::Copied(target) => {
                self.copy_return = Some(CopyReturn {
                    from: end.from,
                    landed: target,
                });
                self.jump_to(target);
            }
            Jump::Elsewhere(place) => {
                return self.set_status(match at {
                    Some(at) => format!("{place} (line {at} of 2)"),
                    None => place,
                });
            }
        }
        if let Some(at) = at {
            self.set_status(format!("{what} line {at} of 2"));
        }
    }

    fn jump_to(&mut self, target: Target) {
        let same_file = self.selected_file() == Some(target.file);
        if !same_file {
            self.reveal_file(target.file);
        }
        self.move_landing = Some(MoveLanding { target, same_file });
    }

    /// Finish an `m` jump once its file is rendered.
    pub(super) fn land_on_move(&mut self) {
        let Some(MoveLanding { target, same_file }) = self.move_landing else {
            return;
        };
        if self.selected_file() != Some(target.file) {
            self.move_landing = None; // The view has moved on without it.
            return;
        }
        let Some(render) = self.current_render() else {
            return;
        };
        let line = render.line_map.row_for(target.anchor);
        let here = render.code_line(self.diff_cursor);
        let new = line.and_then(|line| render.line_map.get(line).new);
        self.move_landing = None;
        let Some(line) = line else {
            return;
        };
        // A file opens with the cursor on its first line, which says nothing.
        if same_file && here == line {
            return self.set_status("Both ends are on this line");
        }
        // Come in by a row's old side and `m` has to go back out that way, which
        // isn't the side it would start with.
        if let (Side::Old, Some(new)) = (target.anchor.side, new) {
            self.move_turns
                .entry((target.file, new))
                .or_insert(Side::New);
        }
        self.land_cursor(line);
    }
}

#[cfg(test)]
mod tests {

    use crossterm::event::KeyCode;

    use crate::app::test_support::app_with;
    use crate::app::{App, Focus};
    use crate::comment::{Anchor, CommentStore, Composer, Side};
    use crate::config::Config;
    use crate::render::{CommentLayer, Jump, Request, Target};
    use crate::tree::{self, RowKind};

    /// A block moved from a.rs to line 12 of src/deep/b.rs, with an unmoved line
    /// around each end.
    const MOVED_BETWEEN_FILES: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n\
         @@ -1,4 +1,2 @@\n keep\n-fn alpha_value() {\n-    first_call(1);\n other\n\
         diff --git a/src/deep/b.rs b/src/deep/b.rs\n--- a/src/deep/b.rs\n+++ b/src/deep/b.rs\n\
         @@ -11,2 +11,4 @@\n keep\n+fn alpha_value() {\n+    first_call(1);\n other\n";

    fn app_with_a_move() -> App {
        let mut app = app_with(crate::diff::parse(MOVED_BETWEEN_FILES));
        app.comments_on = true;
        app.focus = Focus::Diff;
        app.diff_height = 20;
        app.reveal_file(0); // The tree lists folders first, so it opened on b.rs.
        draw_for_move(&mut app);
        app
    }

    /// What the event loop does between keypresses, rendering for real because
    /// the rows a jump lands on have to come from a real render.
    fn draw_for_move(app: &mut App) {
        let idx = app.selected_file().unwrap();
        let side_by_side = app.side_by_side_for(idx);
        app.last_width = 60;
        app.cache.ensure(&Request {
            file: idx,
            diff: &app.files[idx],
            width: 60,
            side_by_side,
            theme: app.diff_theme,
            comments: CommentLayer::none(),
        });
        app.keep_view_on_copies();
        app.resync_cursor();
        app.land_on_move();
    }

    fn put_cursor_on(app: &mut App, side: Side, line: u32) {
        let render = app.current_render().unwrap();
        let row = render.line_map.row_for(Anchor { side, line }).unwrap();
        app.set_cursor(row);
    }

    fn cursor_line(app: &App) -> (Option<u32>, Option<u32>) {
        let numbers = app.current_render().unwrap().line_map.get(app.diff_cursor);
        (numbers.old, numbers.new)
    }

    const COPIED: [&str; 5] = [
        "fn load_settings(path: &Path) -> Result<Settings> {",
        "    let text = fs::read_to_string(path).context(\"reading settings\")?;",
        "    let parsed = toml::from_str(&text).context(\"parsing settings\")?;",
        "    validate_settings(&parsed).context(\"checking settings\")?;",
        "    Ok(apply_defaults(parsed))",
    ];

    /// A file of 80 kept lines with a block at line 31 and a copy of it added at
    /// line 61, and one more added at line 71 of the new side.
    fn copies_diff(copies: usize) -> String {
        let mut raw = format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,80 +1,{} @@\n",
            80 + 5 * copies
        );
        for line in 1..=80 {
            match line {
                31..=35 => raw.push_str(&format!(" {}\n", COPIED[line - 31])),
                _ => raw.push_str(&format!(" kept_line({line});\n")),
            }
            if line == 60 || (line == 70 && copies > 1) {
                COPIED
                    .iter()
                    .for_each(|code| raw.push_str(&format!("+{code}\n")));
            }
        }
        raw
    }

    fn app_with_copies(copies: usize) -> App {
        let mut app = app_with(crate::diff::parse(&copies_diff(copies)));
        app.comments_on = true;
        app.focus = Focus::Diff;
        app.diff_height = 20;
        app.reveal_file(0);
        draw_for_move(&mut app);
        app
    }

    fn land_copies(app: &mut App) {
        app.start_copies();
        app.cache.wait_for_copies();
        // The event loop's first frame has it start the render, and later ones
        // pick that up.
        for _ in 0..3 {
            draw_for_move(app);
        }
    }

    fn lines_of(app: &App) -> Vec<String> {
        let text = &app.current_render().unwrap().text;
        let plain = |line: &ratatui::text::Line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        text.lines.iter().map(plain).collect()
    }

    #[test]
    fn copies_arrive_after_the_first_draw_with_a_note() {
        let mut app = app_with_copies(1);
        assert!(!lines_of(&app).concat().contains("copied from"));
        land_copies(&mut app);
        let lines = lines_of(&app);
        let note = lines
            .iter()
            .position(|l| l.contains("copied from"))
            .unwrap();
        assert!(
            lines[note].contains("≡ copied from line 31"),
            "{}",
            lines[note]
        );
        assert!(lines[note + 1].contains("fn load_settings"));
    }

    #[test]
    fn a_landing_note_leaves_the_cursor_and_the_view_on_the_same_code() {
        let mut app = app_with_copies(1);
        put_cursor_on(&mut app, Side::New, 75);
        app.diff_scroll = app.current_render().unwrap().row_of(app.diff_cursor) - 5;
        let screen = |app: &App| {
            let render = app.current_render().unwrap();
            render.row_of(app.diff_cursor) - app.diff_scroll
        };
        let before = screen(&app);
        land_copies(&mut app);
        assert_eq!(cursor_line(&app), (Some(70), Some(75)));
        assert_eq!(screen(&app), before);
    }

    #[test]
    fn a_landing_note_with_no_cursor_leaves_the_view_on_the_same_code() {
        let mut app = app_with_copies(1);
        app.comments_on = false;
        let render = app.current_render().unwrap();
        let line = render
            .line_map
            .row_for(Anchor {
                side: Side::New,
                line: 75,
            })
            .unwrap();
        app.diff_scroll = render.row_of(line);
        land_copies(&mut app);
        let render = app.current_render().unwrap();
        let top = render.line_at(app.diff_scroll);
        assert_eq!(render.line_map.get(top).new, Some(75));
    }

    #[test]
    fn a_landing_note_above_the_view_cant_scroll_the_top_of_the_file_away() {
        let mut app = app_with_copies(1);
        land_copies(&mut app);
        assert_eq!(app.diff_scroll, 0);
    }

    #[test]
    fn a_landing_note_leaves_an_open_composer_alone() {
        let mut app = app_with_copies(1);
        app.install_comments_for_test(CommentStore::disabled());
        app.comments_on = true;
        put_cursor_on(&mut app, Side::New, 75);
        app.press_for_test(KeyCode::Char('c'), false);
        app.press_for_test(KeyCode::Char('h'), false);
        assert!(app.composer().is_some());
        land_copies(&mut app);
        assert_eq!(app.composer().map(Composer::body).as_deref(), Some("h"));
        assert_eq!(cursor_line(&app), (Some(70), Some(75)));
    }

    #[test]
    fn m_on_a_copy_jumps_to_its_source_and_back() {
        let mut app = app_with_copies(1);
        land_copies(&mut app);
        put_cursor_on(&mut app, Side::New, 62);
        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(cursor_line(&app), (Some(32), Some(32)));
        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(cursor_line(&app), (None, Some(62)));
    }

    #[test]
    fn m_comes_back_to_the_copy_it_left_when_a_source_has_two() {
        let mut app = app_with_copies(2);
        land_copies(&mut app);
        put_cursor_on(&mut app, Side::New, 77);
        let (_, here) = cursor_line(&app);
        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(cursor_line(&app), (Some(32), Some(32)));
        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(cursor_line(&app).1, here);
    }

    #[test]
    fn m_on_a_note_goes_from_the_first_line_of_the_block() {
        let mut app = app_with_copies(1);
        land_copies(&mut app);
        put_cursor_on(&mut app, Side::New, 61);
        app.set_cursor(app.diff_cursor - 1);
        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(cursor_line(&app), (Some(31), Some(31)));
    }

    #[test]
    fn m_says_where_a_source_outside_the_diff_is() {
        // Below a diff that only shows the added block and no context at all, the
        // source only exists as far as a jump can't reach. A context-only diff can't
        // know that, so the index stands in for what a blob would have found.
        let mut app = app_with_copies(1);
        land_copies(&mut app);
        let new = |line| Anchor {
            side: Side::New,
            line,
        };
        let jump = Jump::Elsewhere("Copied from src/b.rs:40, not in this diff".to_string());
        app.cache.set_jump_for_test(0, new(62), jump);
        put_cursor_on(&mut app, Side::New, 62);
        let line = app.diff_cursor;
        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(
            app.status.as_deref(),
            Some("Copied from src/b.rs:40, not in this diff")
        );
        assert_eq!(app.diff_cursor, line);
    }

    #[test]
    fn a_note_on_a_copy_is_no_stop_for_j_and_k() {
        let mut app = app_with_copies(1);
        land_copies(&mut app);
        let render = app.current_render().unwrap();
        let lines = lines_of(&app);
        let note = lines
            .iter()
            .position(|l| l.contains("copied from"))
            .unwrap();
        assert_eq!(render.changes, [note + 1]);
    }

    #[test]
    fn the_note_stands_for_its_block_when_a_comment_is_left_on_it() {
        let mut app = app_with_copies(1);
        land_copies(&mut app);
        put_cursor_on(&mut app, Side::New, 61);
        let code = app.diff_cursor;
        let render = app.current_render().unwrap();
        assert_eq!(render.code_line(code - 1), code);
    }

    #[test]
    fn a_refresh_with_the_same_diff_keeps_the_copies() {
        let mut app = app_with_copies(1);
        land_copies(&mut app);
        app.reload_files(crate::diff::parse(&copies_diff(1)));
        draw_for_move(&mut app);
        assert!(!app.cache.wants_copies());
        assert!(lines_of(&app).concat().contains("copied from"));
    }

    #[test]
    fn a_refresh_with_another_diff_forgets_the_copies() {
        let mut app = app_with_copies(1);
        land_copies(&mut app);
        app.reload_files(crate::diff::parse(&copies_diff(2)));
        draw_for_move(&mut app);
        assert!(app.cache.wants_copies());
        assert!(!lines_of(&app).concat().contains("copied from"));
        land_copies(&mut app);
        let notes = lines_of(&app).concat().matches("copied from").count();
        assert_eq!(notes, 2);
    }

    #[test]
    fn a_new_width_costs_the_render_and_not_the_search() {
        let mut app = app_with_copies(1);
        land_copies(&mut app);
        app.last_width = 90;
        app.cache.clear();
        draw_for_move(&mut app);
        assert!(!app.cache.wants_copies());
        assert!(!app.cache.expecting());
        assert!(lines_of(&app).concat().contains("copied from"));
    }

    #[test]
    fn turning_copies_off_leaves_moves_alone_and_the_other_way_round() {
        let mut config = Config {
            detect_copies: false,
            ..Config::default()
        };
        let files = crate::diff::parse(&copies_diff(1));
        let mut app = App::new(files, false, &config);
        assert!(!app.cache.wants_copies());
        app.start_copies();
        assert!(!app.cache.expecting());

        config.detect_copies = true;
        config.detect_moves = false;
        let files = crate::diff::parse(&copies_diff(1));
        let mut app = App::new(files, false, &config);
        app.focus = Focus::Diff;
        app.diff_height = 20;
        draw_for_move(&mut app);
        land_copies(&mut app);
        assert!(lines_of(&app).concat().contains("copied from"));
    }

    #[test]
    fn m_crosses_to_the_matching_line_and_back() {
        let mut app = app_with_a_move();
        put_cursor_on(&mut app, Side::Old, 3);

        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(
            app.files[app.selected_file().unwrap()].path(),
            "src/deep/b.rs"
        );
        assert_eq!(cursor_line(&app), (None, Some(13)));

        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(app.files[app.selected_file().unwrap()].path(), "a.rs");
        assert_eq!(cursor_line(&app), (Some(3), None));
    }

    #[test]
    fn m_on_the_note_above_a_block_goes_to_the_blocks_first_line() {
        let mut app = app_with_a_move();
        put_cursor_on(&mut app, Side::Old, 2);
        let note = app.diff_cursor - 1;
        app.set_cursor(note);

        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(cursor_line(&app), (None, Some(12)));
    }

    #[test]
    fn m_off_a_moved_line_says_so_and_stays_put() {
        let mut app = app_with_a_move();
        put_cursor_on(&mut app, Side::New, 1);
        let row = app.diff_cursor;

        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(app.selected_file(), Some(0));
        assert_eq!(app.diff_cursor, row);
        assert_eq!(app.status.as_deref(), Some("Not on a moved or copied line"));
    }

    #[test]
    fn m_opens_a_folded_folder_on_the_way_in() {
        let mut app = app_with_a_move();
        put_cursor_on(&mut app, Side::Old, 2);
        app.collapsed.insert("src".to_string());
        app.rows = tree::flatten(&app.nodes, &app.collapsed);
        let at = app
            .rows
            .iter()
            .position(|r| matches!(r.kind, RowKind::File { diff_index: 0 }));
        app.tree_state.select(at);
        assert!(
            app.rows
                .iter()
                .all(|r| !matches!(r.kind, RowKind::File { diff_index: 1 }))
        );

        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(app.selected_file(), Some(1));
        assert_eq!(cursor_line(&app), (None, Some(12)));
    }

    #[test]
    fn m_with_no_cursor_follows_the_first_moved_line_on_screen() {
        let mut app = app_with_a_move();
        app.comments_on = false;
        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(app.selected_file(), Some(1));
        assert_eq!(cursor_line(&app), (None, Some(12)));
    }

    /// With comments off `m` reads the lines in view, and the row just below the
    /// pane isn't in it.
    #[test]
    fn m_with_no_cursor_leaves_a_move_just_below_the_pane_alone() {
        let mut app = app_with_a_move();
        app.comments_on = false;
        let render = app.current_render().unwrap();
        let old = Anchor {
            side: Side::Old,
            line: 2,
        };
        let moved = render
            .line_map
            .row_for(old)
            .expect("the moved line is in the render");
        let block = (0..render.lines())
            .find(|&line| render.code_line(line) == moved)
            .unwrap();
        let height = render.row_of(block) as u16;
        app.diff_scroll = 0;
        app.diff_height = height;
        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(app.selected_file(), Some(0), "it stayed put");
        assert_eq!(app.status.as_deref(), Some("Not on a moved or copied line"));
    }

    #[test]
    fn m_works_side_by_side() {
        let mut app = app_with_a_move();
        app.side_by_side = true;
        app.cursor_token = None;
        draw_for_move(&mut app);
        put_cursor_on(&mut app, Side::Old, 2);

        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        let (_, new) = cursor_line(&app);
        assert_eq!(new, Some(12));
    }

    #[test]
    fn m_works_from_a_wrapped_side_by_side_row() {
        let long = "first_call(an_argument_long_enough_to_wrap_in_a_narrow_panel, 1);";
        let raw = format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n\
             -fn alpha_value() {{\n-{long}\n keep\n+fn alpha_value() {{\n+{long}\n"
        );
        let mut app = app_with(crate::diff::parse(&raw));
        app.comments_on = true;
        app.side_by_side = true;
        app.focus = Focus::Diff;
        app.diff_height = 20;
        draw_for_move(&mut app);
        put_cursor_on(&mut app, Side::Old, 2);
        let wrapped = app.diff_cursor + 1;
        app.set_cursor(wrapped);
        assert_eq!(cursor_line(&app), (None, None), "this row has no number");

        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(cursor_line(&app), (None, Some(3)));
    }

    #[test]
    fn m_says_so_when_both_ends_share_a_side_by_side_row() {
        let raw = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n\
                   -first_call(1);\n-second_call(2);\n\
                   +    first_call(1);\n+    second_call(2);\n";
        let mut app = app_with(crate::diff::parse(raw));
        app.comments_on = true;
        app.side_by_side = true;
        app.focus = Focus::Diff;
        app.diff_height = 20;
        draw_for_move(&mut app);
        put_cursor_on(&mut app, Side::Old, 2);
        let row = app.diff_cursor;

        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(app.diff_cursor, row);
        assert_eq!(app.status.as_deref(), Some("Both ends are on this line"));
    }

    /// A change in a.rs whose removed block went to old_end.rs and whose added block
    /// came from new_end.rs, so the two sit opposite each other side by side.
    const MOVES_ON_BOTH_SIDES: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n\
         @@ -1,3 +1,3 @@\n keep\n-fn alpha_value() {\n-    first_call(1);\n\
         +fn gamma_value() {\n+    third_call(3);\n\
         diff --git a/new_end.rs b/new_end.rs\n--- a/new_end.rs\n+++ b/new_end.rs\n\
         @@ -21,3 +21,1 @@\n keep\n-fn gamma_value() {\n-    third_call(3);\n\
         diff --git a/old_end.rs b/old_end.rs\n--- a/old_end.rs\n+++ b/old_end.rs\n\
         @@ -11,1 +11,3 @@\n keep\n+fn alpha_value() {\n+    first_call(1);\n";

    fn side_by_side_app(raw: &str) -> App {
        let mut app = app_with(crate::diff::parse(raw));
        app.comments_on = true;
        app.side_by_side = true;
        app.focus = Focus::Diff;
        app.diff_height = 20;
        app.reveal_file(0);
        draw_for_move(&mut app);
        app
    }

    fn press_m(app: &mut App) {
        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(app);
    }

    /// The file the cursor is in and the line numbers it's on.
    fn place(app: &App) -> (&str, (Option<u32>, Option<u32>)) {
        let file = app.selected_file().unwrap();
        (app.files[file].path(), cursor_line(app))
    }

    const ON_THE_ROW: (&str, (Option<u32>, Option<u32>)) = ("a.rs", (Some(2), Some(2)));

    const AT_NEW_END: (&str, (Option<u32>, Option<u32>)) = ("new_end.rs", (Some(22), None));

    const AT_OLD_END: (&str, (Option<u32>, Option<u32>)) = ("old_end.rs", (None, Some(12)));

    #[test]
    fn m_takes_the_sides_of_a_row_in_turn_and_says_which() {
        let mut app = side_by_side_app(MOVES_ON_BOTH_SIDES);
        put_cursor_on(&mut app, Side::New, 2);
        assert_eq!(place(&app), ON_THE_ROW);

        for _ in 0..2 {
            press_m(&mut app);
            assert_eq!(place(&app), AT_NEW_END);
            assert_eq!(app.status.as_deref(), Some("Moved line 1 of 2"));
            press_m(&mut app);
            assert_eq!(place(&app), ON_THE_ROW);
            assert_eq!(app.status, None);
            press_m(&mut app);
            assert_eq!(place(&app), AT_OLD_END);
            assert_eq!(app.status.as_deref(), Some("Moved line 2 of 2"));
            press_m(&mut app);
            assert_eq!(place(&app), ON_THE_ROW);
        }
    }

    #[test]
    fn m_keeps_a_rows_turn_when_the_cursor_wanders_off_and_back() {
        let mut app = side_by_side_app(MOVES_ON_BOTH_SIDES);
        put_cursor_on(&mut app, Side::New, 2);
        press_m(&mut app);
        press_m(&mut app);
        app.press_for_test(KeyCode::Char('j'), false);
        app.press_for_test(KeyCode::Char('k'), false);
        press_m(&mut app);
        assert_eq!(place(&app), AT_OLD_END);
    }

    #[test]
    fn m_goes_back_the_way_it_came_onto_a_row_with_two_sides() {
        let mut app = side_by_side_app(MOVES_ON_BOTH_SIDES);
        app.reveal_file(2);
        draw_for_move(&mut app);
        put_cursor_on(&mut app, Side::New, 12);

        press_m(&mut app);
        assert_eq!(place(&app), ON_THE_ROW);
        press_m(&mut app);
        assert_eq!(place(&app), AT_OLD_END);
        press_m(&mut app);
        assert_eq!(place(&app), ON_THE_ROW);
        press_m(&mut app);
        assert_eq!(place(&app), AT_NEW_END, "the side it hasn't taken yet");
    }

    #[test]
    fn m_keeps_each_rows_turn_when_both_ends_have_two_sides() {
        // Three blocks that each moved to the next change along, so every row has an
        // end on each side, and its two sides lead to different rows.
        let raw = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,8 +1,8 @@\n\
                   -fn alpha_value() {\n-    first_call(1);\n\
                   +fn beta_value() {\n+    second_call(2);\n kept\n\
                   -fn beta_value() {\n-    second_call(2);\n\
                   +fn gamma_value() {\n+    third_call(3);\n kept\n\
                   -fn gamma_value() {\n-    third_call(3);\n\
                   +fn alpha_value() {\n+    first_call(1);\n";
        let row = |line| (Some(line), Some(line));
        let mut app = side_by_side_app(raw);
        put_cursor_on(&mut app, Side::New, 1);
        assert_eq!(cursor_line(&app), row(1));

        for (lands_on, note) in [
            (row(4), "Moved line 1 of 2"), // Its new side.
            (row(1), "Moved line 2 of 2"), // Back again, by that row's old side.
            (row(7), "Moved line 2 of 2"), // Its old side.
            (row(1), "Moved line 1 of 2"), // Back again, by that row's new side.
            (row(4), "Moved line 1 of 2"), // Its new side, and so on.
        ] {
            press_m(&mut app);
            assert_eq!(
                (cursor_line(&app), app.status.as_deref()),
                (lands_on, Some(note))
            );
        }
    }

    #[test]
    fn m_onto_the_first_line_of_another_file_is_no_both_ends() {
        let raw = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,1 @@\n keep\n\
                   -fn alpha_value() {\n-    first_call(1);\n\
                   diff --git a/b.rs b/b.rs\nnew file mode 100644\n--- /dev/null\n+++ b/b.rs\n\
                   @@ -0,0 +1,2 @@\n+fn alpha_value() {\n+    first_call(1);\n";
        let mut app = app_with(crate::diff::parse(raw));
        app.comments_on = true;
        app.focus = Focus::Diff;
        app.diff_height = 20;
        app.reveal_file(0);
        draw_for_move(&mut app);
        put_cursor_on(&mut app, Side::Old, 2);

        press_m(&mut app);
        assert_eq!(place(&app), ("b.rs", (None, Some(1))));
        assert_eq!(app.status, None);
    }

    #[test]
    fn m_starts_a_row_over_on_a_new_diff() {
        let mut app = side_by_side_app(MOVES_ON_BOTH_SIDES);
        put_cursor_on(&mut app, Side::New, 2);
        press_m(&mut app);
        press_m(&mut app);
        assert!(!app.move_turns.is_empty());
        app.reload_files(crate::diff::parse(MOVES_ON_BOTH_SIDES));
        assert!(app.move_turns.is_empty());
    }

    #[test]
    fn m_says_nothing_more_on_a_row_with_one_side_moved() {
        let mut app = app_with_a_move();
        app.side_by_side = true;
        app.cursor_token = None;
        draw_for_move(&mut app);
        put_cursor_on(&mut app, Side::Old, 2);
        press_m(&mut app);
        assert_eq!(app.status, None);
        assert!(app.move_turns.is_empty());
    }

    /// A change in a.rs whose removed block went to old_end.rs, opposite added lines
    /// a test can say are copied.
    const MOVE_OPPOSITE_A_COPY: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n\
         @@ -1,3 +1,3 @@\n keep\n-fn alpha_value() {\n-    first_call(1);\n\
         +plain_one();\n+plain_two();\n\
         diff --git a/old_end.rs b/old_end.rs\n--- a/old_end.rs\n+++ b/old_end.rs\n\
         @@ -11,1 +11,3 @@\n keep\n+fn alpha_value() {\n+    first_call(1);\n\
         diff --git a/source.rs b/source.rs\n--- a/source.rs\n+++ b/source.rs\n\
         @@ -31,2 +31,3 @@\n kept_one\n+added\n kept_two\n";

    fn new_line(line: u32) -> Anchor {
        Anchor {
            side: Side::New,
            line,
        }
    }

    #[test]
    fn m_takes_turns_between_a_copy_and_a_move_on_one_row() {
        let mut app = side_by_side_app(MOVE_OPPOSITE_A_COPY);
        let source = Target {
            file: 2,
            anchor: new_line(31),
        };
        app.cache
            .set_jump_for_test(0, new_line(2), Jump::Copied(source));
        put_cursor_on(&mut app, Side::New, 2);

        press_m(&mut app);
        assert_eq!(place(&app), ("source.rs", (Some(31), Some(31))));
        assert_eq!(app.status.as_deref(), Some("Copied line 1 of 2"));
        press_m(&mut app);
        assert_eq!(place(&app), ON_THE_ROW);
        press_m(&mut app);
        assert_eq!(place(&app), AT_OLD_END);
        assert_eq!(app.status.as_deref(), Some("Moved line 2 of 2"));
        press_m(&mut app);
        assert_eq!(place(&app), ON_THE_ROW);
        press_m(&mut app);
        assert_eq!(place(&app).0, "source.rs");
    }

    #[test]
    fn m_gets_past_a_copy_the_diff_doesnt_show_to_the_move_beside_it() {
        let mut app = side_by_side_app(MOVE_OPPOSITE_A_COPY);
        let place_of_copy = "Copied from src/b.rs:40, not in this diff".to_string();
        app.cache
            .set_jump_for_test(0, new_line(2), Jump::Elsewhere(place_of_copy));
        put_cursor_on(&mut app, Side::New, 2);

        press_m(&mut app);
        assert_eq!(place(&app), ON_THE_ROW);
        assert_eq!(
            app.status.as_deref(),
            Some("Copied from src/b.rs:40, not in this diff (line 1 of 2)")
        );
        press_m(&mut app);
        assert_eq!(place(&app), AT_OLD_END);
    }

    #[test]
    fn m_on_a_hunk_header_above_a_moved_block_does_nothing() {
        let raw = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,3 +1,3 @@\n\
                   -fn alpha_value() {\n-    first_call(1);\n keep\n\
                   +fn alpha_value() {\n+    first_call(1);\n";
        let mut app = app_with(crate::diff::parse(raw));
        app.comments_on = true;
        app.focus = Focus::Diff;
        app.diff_height = 20;
        draw_for_move(&mut app);
        put_cursor_on(&mut app, Side::Old, 1);
        let header = app.diff_cursor - 2; // Past the note, onto the box's lower edge.
        app.set_cursor(header);

        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(app.diff_cursor, header);
        assert_eq!(app.status.as_deref(), Some("Not on a moved or copied line"));
    }

    #[test]
    fn m_leaves_a_target_off_screen_a_third_of_the_way_down() {
        let filler: String = (0..200).map(|i| format!(" line {i}\n")).collect();
        let raw = format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,202 +1,202 @@\n\
             -fn alpha_value() {{\n-    first_call(1);\n{filler}\
             +fn alpha_value() {{\n+    first_call(1);\n"
        );
        let mut app = app_with(crate::diff::parse(&raw));
        app.comments_on = true;
        app.focus = Focus::Diff;
        app.diff_height = 30;
        draw_for_move(&mut app);
        put_cursor_on(&mut app, Side::Old, 1);

        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(cursor_line(&app), (None, Some(201)));
        let top = app.current_render().unwrap().row_of(app.diff_cursor);
        assert_eq!(top - app.diff_scroll, 10);
    }

    #[test]
    fn m_is_for_the_diff_pane() {
        let mut app = app_with_a_move();
        put_cursor_on(&mut app, Side::Old, 2);
        app.focus = Focus::Tree;
        app.press_for_test(KeyCode::Char('m'), false);
        draw_for_move(&mut app);
        assert_eq!(app.selected_file(), Some(0));
    }
}

//! The `J` and `K` keys: moving between changes, across files.

use std::collections::VecDeque;

use super::{App, By, Focus};
use crate::comment::Anchor;
use crate::diff;
use crate::render::Rendered;

/// Jumps in progress.
#[derive(Default)]
pub(super) struct ChangeJumps {
    /// Presses still to be acted on, oldest first, `true` for `J`. One has to wait
    /// when the render it would read is out of date, so a burst of them is never
    /// cut short by a file that's slow to render.
    queued: VecDeque<bool>,
    /// A change waiting on the event loop to render the file it's in.
    landing: Option<Landing>,
    /// The file and line the last jump stopped on, so a new layout can keep the
    /// cursor at the start of that change and comments off can tell the view has
    /// left it.
    stopped: Option<(usize, Anchor)>,
    /// The file and line it stopped on instead, if that line has no number, as a
    /// line only a merge's other parent lost doesn't. Good for this render only.
    stopped_unnumbered: Option<(usize, usize)>,
}

/// Which of a file's changes to go to, once it's rendered.
#[derive(Clone, Copy)]
struct Landing {
    file: usize,
    /// Its first change, or its last.
    first: bool,
}

impl App {
    /// The `J` and `K` keys. They act on the diff from either pane.
    pub(super) fn jump_change(&mut self, forward: bool) {
        self.focus = Focus::Diff;
        self.head(forward, By::Change);
        self.change_jumps.queued.push_back(forward);
        // A jump on its way somewhere goes first, as it was asked for first.
        if self.comment_landing.is_none()
            && self.move_landing.is_none()
            && !self.heading_for_match()
        {
            self.run_change_jumps();
        }
    }

    /// Act on as many waiting presses as the renders on hand allow. The event loop
    /// comes back here once whatever they're waiting for has landed.
    pub(super) fn run_change_jumps(&mut self) {
        while self.render_ready() {
            if let Some(landing) = self.change_jumps.landing.take() {
                self.land_on_change(landing);
            } else if let Some(forward) = self.change_jumps.queued.pop_front() {
                self.step_change(forward);
            } else {
                return;
            }
        }
    }

    /// Forget a jump into a file: the indices it was given no longer hold.
    pub(super) fn abandon_change_landing(&mut self) {
        self.change_jumps.landing = None;
    }

    /// Keep the cursor at the start of the change it stopped on. A removed line
    /// and the added line beside it share a screen line side by side, but not in
    /// unified, where going by the anchor would leave the cursor partway down the
    /// change, for `K` to stop at its start all over again.
    pub(super) fn follow_change(&mut self) {
        let here = self.selected_file().zip(self.cursor_anchor);
        // A match the cursor is on has its own idea of where to be.
        if here.is_none() || here != self.change_jumps.stopped || self.holding_match() {
            return;
        }
        let Some(render) = self.current_render() else {
            return;
        };
        let cursor = self.diff_cursor;
        let at = render.changes.partition_point(|&line| line <= cursor);
        let Some(&start) = render.changes[..at].last() else {
            return;
        };
        let same = |line| render.line_map.change_on(line);
        if start != cursor && same(cursor).is_some() && same(cursor) == same(start) {
            self.set_cursor(start);
            self.change_jumps.stopped = self.selected_file().zip(self.cursor_anchor);
        }
    }

    /// Whether there are presses left over, waiting on a render.
    pub(super) fn change_jumps_waiting(&self) -> bool {
        self.change_jumps.landing.is_some() || !self.change_jumps.queued.is_empty()
    }

    /// Whether the cursor's index is good for the render on show. A folder has no
    /// render to wait for.
    pub(super) fn render_ready(&self) -> bool {
        self.selected_file().is_none_or(|file| {
            self.render_of(file).is_some() && self.cursor_token == Some(self.render_token(file))
        })
    }

    /// Finish a jump into a file, unless something else has since moved the view
    /// on to another one: the later key wins, along with what was queued behind.
    fn land_on_change(&mut self, landing: Landing) {
        if self.selected_file() != Some(landing.file) {
            return self.change_jumps.queued.clear();
        }
        let changes = self.current_render().map(|r| r.changes.as_slice());
        let line = changes.and_then(|c| if landing.first { c.first() } else { c.last() });
        if let Some(&line) = line {
            self.stop_at_change(line);
        }
    }

    /// One press: the next change in this file, or the next file with one.
    fn step_change(&mut self, forward: bool) {
        match self.next_change_here(forward) {
            Some(line) => self.stop_at_change(line),
            None => self.step_change_file(forward),
        }
    }

    /// The line to go to in the selected file. `K` inside a change finds its start,
    /// as that's the nearest one before the cursor.
    fn next_change_here(&self, forward: bool) -> Option<usize> {
        let render = self.current_render()?;
        let here = self.change_position(render);
        let changes = &render.changes;
        if forward {
            let next = changes.partition_point(|&line| line <= here);
            changes.get(next).copied()
        } else {
            let next = changes.partition_point(|&line| line < here);
            changes[..next].last().copied()
        }
    }

    /// Where a jump starts from: the numbered line the cursor stands for, as the
    /// note above a moved block isn't a place of its own. With comments off the
    /// cursor is hidden, and a scroll only drags it as far as the edge of the view,
    /// so it only counts while it's where the last jump left it, which the end of
    /// the file can stop the view from showing. Otherwise it's the top of the view.
    fn change_position(&self, render: &Rendered) -> usize {
        let here = render.code_line(self.diff_cursor);
        let stopped = self.change_jumps.stopped;
        let on_it = || {
            let anchor = render.line_map.get(here).anchor();
            stopped.is_some_and(|(file, line)| {
                Some(file) == self.selected_file() && Some(line) == anchor
            })
        };
        let unnumbered = self.selected_file().map(|file| (file, here));
        if self.comments_on || on_it() || self.change_jumps.stopped_unnumbered == unnumbered {
            here
        } else {
            render.line_at(self.diff_scroll)
        }
    }

    /// Stop on `line` of the selected file. With no cursor to show for it, the
    /// scroll is all a jump has to say it did anything, so it always scrolls.
    fn stop_at_change(&mut self, line: usize) {
        self.land_cursor(line);
        if !self.comments_on
            && let Some(top) = self.current_render().map(|r| r.row_of(line))
        {
            self.diff_scroll = top.saturating_sub(u32::from(self.diff_height / 3));
        }
        self.change_jumps.stopped = self.selected_file().zip(self.cursor_anchor);
        let numbered = self.current_render().is_some_and(|render| {
            render
                .line_map
                .get(render.code_line(line))
                .anchor()
                .is_some()
        });
        self.change_jumps.stopped_unnumbered = self
            .selected_file()
            .map(|file| (file, line))
            .filter(|_| !numbered);
    }

    /// Go on to the nearest file with a change in the direction of travel, which
    /// may be this one again, the long way round.
    fn step_change_file(&mut self, forward: bool) {
        let order = self.tree_order();
        let target = self
            .ranks_onward(forward, order.len())
            .map(|(rank, wrapped)| (order[rank], wrapped))
            .find(|&(file, _)| diff::has_changes(&self.files[file]));
        let Some((file, wrapped)) = target else {
            self.change_jumps.queued.clear();
            return self.set_status("No changes in this diff");
        };
        if self.selected_file() != Some(file) {
            self.reveal_file(file);
        }
        self.change_jumps.landing = Some(Landing {
            file,
            first: forward,
        });
        if wrapped {
            let end = if forward { "top" } else { "bottom" };
            self.set_status(format!("Changes wrapped to the {end}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode::{self, Char, Enter};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use std::time::Duration;

    use super::*;
    use crate::comment::{Comment, CommentStore, Side};
    use crate::config::Config;
    use crate::diff::parse;
    use crate::render::test_support::Gate;

    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 16;

    /// One hunk from line 1.
    fn hunk(path: &str, body: &str) -> String {
        let old = body.lines().filter(|l| !l.starts_with('+')).count();
        let new = body.lines().filter(|l| !l.starts_with('-')).count();
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,{old} +1,{new} @@\n{body}"
        )
    }

    /// `changes` one-line edits with `gap` lines of context around each.
    fn edits(changes: usize, gap: usize) -> String {
        let mut body = String::new();
        for at in 0..changes {
            for line in 0..gap {
                body.push_str(&format!(" context {at} {line}\n"));
            }
            body.push_str(&format!("-old {at}\n+new {at}\n"));
        }
        for line in 0..gap {
            body.push_str(&format!(" tail {line}\n"));
        }
        body
    }

    fn app_with(raw: &str, side_by_side: bool, comments: bool) -> App {
        let mut app = App::new(parse(raw), side_by_side, &Config::default());
        if comments {
            app.install_comments_for_test(CommentStore::disabled());
        }
        app.focus = Focus::Diff;
        frame(&mut app);
        app
    }

    fn app(raw: &str) -> App {
        app_with(raw, false, true)
    }

    /// A pass of the event loop.
    fn frame(app: &mut App) {
        frame_at(app, WIDTH, HEIGHT);
    }

    fn frame_at(app: &mut App, width: u16, height: u16) {
        let diff_width = app.diff_pane_width(width);
        app.prepare_frame(diff_width);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| crate::ui::draw(f, app, diff_width))
            .unwrap();
    }

    fn press(app: &mut App, code: KeyCode) {
        app.press_for_test(code, false);
        frame(app);
    }

    fn file_name(app: &App) -> Option<String> {
        let file = app.selected_file()?;
        Some(app.files[file].path().to_string())
    }

    /// The old and new number of the cursor's line.
    fn at(app: &App) -> (Option<u32>, Option<u32>) {
        let numbers = app.current_render().unwrap().line_map.get(app.diff_cursor);
        (numbers.old, numbers.new)
    }

    fn old(line: u32) -> (Option<u32>, Option<u32>) {
        (Some(line), None)
    }

    fn cursor_row(app: &App) -> i64 {
        let render = app.current_render().unwrap();
        i64::from(render.row_of(app.diff_cursor)) - i64::from(app.diff_scroll)
    }

    /// Everywhere `J` stops, from where the view is, until it comes back round.
    fn lap(app: &mut App, key: char) -> Vec<(String, usize)> {
        let mut stops = Vec::new();
        for _ in 0..200 {
            press(app, Char(key));
            let stop = (file_name(app).unwrap(), app.diff_cursor);
            if stops.contains(&stop) {
                return stops;
            }
            stops.push(stop);
        }
        panic!("never came back round: {stops:?}");
    }

    /// A merge in which three changes are lines only the second parent lost, so
    /// they carry no number, each with enough context around it to scroll.
    fn unnumbered_changes() -> String {
        let context = "  x\n".repeat(20);
        let body = format!("{context} -a\n{context} -b\n{context} -c\n{context}");
        format!("diff --cc f\nindex 1,2..3\n--- a/f\n+++ b/f\n@@@ -1,80 -1,83 +1,80 @@@\n{body}")
    }

    #[test]
    fn a_line_with_no_number_says_why_it_takes_no_comment() {
        let mut app = app(&unnumbered_changes());
        press(&mut app, Char('J'));
        assert_eq!(at(&app), (None, None));
        press(&mut app, Char('c'));
        assert!(app.composer.is_none());
        let status = app.status.clone().unwrap();
        assert!(status.contains("another parent"), "{status}");
    }

    #[test]
    fn j_stops_on_each_change_that_has_no_line_number() {
        for comments in [true, false] {
            for side_by_side in [true, false] {
                let mut app = app(&unnumbered_changes());
                app.comments_on = comments;
                app.side_by_side = side_by_side;
                frame(&mut app);
                let stops = lap(&mut app, 'J');
                assert_eq!(stops.len(), 3, "{comments} {side_by_side}: {stops:?}");
                let back = lap(&mut app, 'K');
                assert_eq!(back.len(), 3, "{comments} {side_by_side}: {back:?}");
            }
        }
    }

    #[test]
    fn a_hunk_can_hold_several_changes() {
        let mut app = app(&hunk("f", " a\n-b\n+c\n d\n-e\n f\n+g\n h\n"));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(2));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(4));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), (None, Some(5)));
    }

    #[test]
    fn k_inside_a_change_goes_to_its_start_first() {
        let mut app = app(&hunk("f", " a\n-b\n+c\n d\n-e\n+f\n+g\n h\n"));
        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        press(&mut app, Char('j'));
        press(&mut app, Char('j'));
        assert_eq!(at(&app), (None, Some(5)));

        press(&mut app, Char('K'));
        assert_eq!(at(&app), old(4), "the start of the change it's in");
        press(&mut app, Char('K'));
        assert_eq!(at(&app), old(2), "and then the one before");
    }

    #[test]
    fn j_inside_a_change_goes_to_the_next_one() {
        let mut app = app(&hunk("f", " a\n-b\n-c\n d\n-e\n f\n"));
        press(&mut app, Char('J'));
        press(&mut app, Char('j'));
        assert_eq!(at(&app), old(3));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(5));
    }

    #[test]
    fn a_change_off_screen_lands_a_third_of_the_way_down() {
        let mut app = app(&hunk("f", &edits(2, 40)));
        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        let height = i64::from(app.diff_height);
        assert_eq!(cursor_row(&app), height / 3);
    }

    #[test]
    fn a_hunk_header_ends_a_change_even_with_no_context() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n\
                   @@ -1 +1 @@\n-a\n+b\n@@ -5 +5 @@\n-c\n+d\n";
        let mut app = app(raw);
        assert_eq!(at(&app), old(1), "opens on the first change");
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(5));
    }

    #[test]
    fn side_by_side_a_change_is_one_stop() {
        let raw = hunk("f", " a\n-b\n-c\n+B\n+C\n d\n-e\n+E\n f\n");
        let mut app = app_with(&raw, true, true);
        assert_eq!(app.current_render().unwrap().changes.len(), 2);
        press(&mut app, Char('J'));
        assert_eq!(at(&app), (Some(2), Some(2)));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), (Some(5), Some(5)));
        press(&mut app, Char('K'));
        assert_eq!(at(&app), (Some(2), Some(2)));
    }

    #[test]
    fn a_rewrite_with_a_removal_between_additions_is_one_change() {
        let raw = hunk("f", " a\n-b\n+B\n-c\n+C\n d\n");
        for side_by_side in [false, true] {
            let app = app_with(&raw, side_by_side, true);
            assert_eq!(app.current_render().unwrap().changes.len(), 1);
        }
    }

    #[test]
    fn a_missing_newline_marker_inside_a_change_is_not_a_stop() {
        let raw = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n x\n-a\n\
                   \\ No newline at end of file\n+b\n\\ No newline at end of file\n";
        for side_by_side in [false, true] {
            let app = app_with(raw, side_by_side, true);
            assert_eq!(app.current_render().unwrap().changes.len(), 1);
        }
    }

    /// A long line wraps onto rows that carry no number, and neither they nor the
    /// context around them are stops.
    #[test]
    fn wrapped_lines_add_no_stops() {
        let long = "x".repeat(300);
        let raw = hunk(
            "f",
            &format!(" {long}\n-{long}\n+{long}y\n {long}\n-e\n+f\n g\n"),
        );
        for side_by_side in [false, true] {
            let mut app = app_with(&raw, side_by_side, true);
            assert_eq!(app.current_render().unwrap().changes.len(), 2);
            press(&mut app, Char('J'));
            press(&mut app, Char('J'));
            assert_eq!(at(&app).0, Some(4));
        }
    }

    /// Taller than the pane, so scrolling its foot into view would hide its start.
    #[test]
    fn a_change_wrapped_taller_than_the_pane_shows_where_it_starts() {
        let long = "x".repeat(2000);
        let mut app = app(&hunk(
            "f",
            &format!(" a\n b\n c\n-d\n e\n f\n-{long}\n g\n"),
        ));
        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(7));
        let row = cursor_row(&app);
        assert!((0..i64::from(app.diff_height)).contains(&row), "{row}");
    }

    /// The moved block's note sits above its first line.
    #[test]
    fn a_move_note_is_not_a_stop_and_stands_for_its_line() {
        let raw = format!(
            "{}{}",
            hunk(
                "a",
                " keep\n-fn alpha_value() {\n-    first_call(1);\n-    second_call(2);\n other\n"
            ),
            hunk(
                "b",
                " keep\n+fn alpha_value() {\n+    first_call(1);\n+    second_call(2);\n other\n"
            ),
        );
        let mut app = app(&raw);
        press(&mut app, Char('J'));
        let start = app.diff_cursor;
        let render = app.current_render().unwrap();
        assert_eq!(render.changes, [start]);
        assert_eq!(render.code_line(start - 1), start, "the note is above it");

        app.diff_cursor = start - 1;
        press(&mut app, Char('K'));
        assert_eq!(file_name(&app).as_deref(), Some("b"), "not back onto it");
    }

    fn note(path: &str, line: u32) -> Comment {
        Comment {
            id: format!("{path}{line}"),
            file: path.to_string(),
            side: Side::New,
            line,
            body: "a note".to_string(),
            author: "tester".to_string(),
            created: crate::state::now_unix(),
            reply_to: None,
            diff_hash: None,
        }
    }

    fn with_note(raw: &str, side_by_side: bool, line: u32) -> App {
        let mut app = App::new(parse(raw), side_by_side, &Config::default());
        let mut store = CommentStore::disabled();
        store.add(note("f", line));
        app.install_comments_for_test(store);
        app.focus = Focus::Diff;
        frame(&mut app);
        app
    }

    #[test]
    fn a_comment_inside_a_change_does_not_split_it() {
        let raw = hunk("f", " a\n+b\n+c\n+d\n e\n+f\n g\n");
        for side_by_side in [false, true] {
            let mut app = with_note(&raw, side_by_side, 2);
            assert_eq!(app.current_render().unwrap().changes.len(), 2);
            press(&mut app, Char('J'));
            assert_eq!(at(&app).1, Some(2));
            press(&mut app, Char('J'));
            assert_eq!(at(&app).1, Some(6));
        }
    }

    #[test]
    fn from_inside_a_comment_k_goes_to_the_start_of_the_change_it_hangs_on() {
        let mut app = with_note(&hunk("f", " a\n+b\n+c\n d\n"), false, 3);
        press(&mut app, Char(']'));
        assert!(app.block_at_cursor().is_some());
        press(&mut app, Char('K'));
        assert_eq!(at(&app).1, Some(2));
    }

    #[test]
    fn a_new_comment_moves_the_stops_with_it() {
        let mut app = app(&hunk("f", &edits(2, 3)));
        press(&mut app, Char('J'));
        let before = app.current_render().unwrap().changes.clone();
        app.comments.add(note("f", 4));
        app.bump_comments();
        frame(&mut app);
        let after = app.current_render().unwrap().changes.clone();
        assert_eq!(after[0], before[0]);
        assert!(after[1] > before[1], "the second is pushed down");
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(8));
    }

    #[test]
    fn the_stops_are_right_after_a_resize_a_toggle_and_a_theme_change() {
        let long = "word ".repeat(40);
        let raw = hunk(
            "f",
            &format!(" a\n-{long}\n+{long}x\n b\n-c\n+d\n e\n-f\n g\n"),
        );
        let mut app = app(&raw);
        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(4));

        frame_at(&mut app, 50, HEIGHT);
        assert_eq!(at(&app), old(4), "still on it at the new width");
        app.press_for_test(Char('J'), false);
        frame_at(&mut app, 50, HEIGHT);
        assert_eq!(at(&app), old(6));

        press(&mut app, Char('s'));
        press(&mut app, Char('K'));
        assert_eq!(at(&app), (Some(4), Some(4)));
        press(&mut app, Char('T'));
        press(&mut app, Char('K'));
        assert_eq!(at(&app), (Some(2), Some(2)));
    }

    #[test]
    fn a_stop_is_still_the_start_of_its_change_in_the_other_layout() {
        let raw = hunk("f", " a\n-b\n x\n-c\n-d\n+C\n+D\n e\n");
        let mut app = app_with(&raw, true, true);
        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), (Some(4), Some(3)));

        press(&mut app, Char('s'));
        assert_eq!(
            at(&app),
            old(4),
            "the first row, not the added row beside it"
        );
        press(&mut app, Char('K'));
        assert_eq!(at(&app), old(2), "so K goes on to the one before");
    }

    #[test]
    fn a_line_moved_to_by_hand_stays_put_in_the_other_layout() {
        let raw = hunk("f", " a\n-b\n-c\n+B\n+C\n d\n");
        let mut app = app(&raw);
        press(&mut app, Char('J'));
        press(&mut app, Char('j'));
        press(&mut app, Char('j'));
        assert_eq!(at(&app), (None, Some(2)));
        press(&mut app, Char('s'));
        assert_eq!(at(&app).1, Some(2));
        assert_eq!(app.diff_cursor, app.current_render().unwrap().changes[0]);
        press(&mut app, Char('s'));
        assert_eq!(
            at(&app),
            (None, Some(2)),
            "not pulled up to the change's start"
        );
    }

    #[test]
    fn a_press_straight_after_a_toggle_waits_for_the_new_render() {
        let mut app = app(&hunk("f", &edits(3, 3)));
        press(&mut app, Char('J'));
        app.press_for_test(Char('s'), false);
        app.press_for_test(Char('J'), false);
        assert_eq!(app.change_jumps.queued.len(), 1, "no render to read yet");
        frame(&mut app);
        assert_eq!(at(&app), (Some(8), Some(8)));
    }

    #[test]
    fn past_the_last_change_it_carries_on_into_the_next_file() {
        let raw = format!(
            "{}{}",
            hunk("a", " x\n-y\n z\n"),
            hunk("b", " p\n-q\n-r\n s\n-t\n")
        );
        let mut app = app(&raw);
        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("b"));
        assert_eq!(at(&app), old(2), "its first change");

        press(&mut app, Char('K'));
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("a"));
        press(&mut app, Char('J'));
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("b"));
        press(&mut app, Char('J'));
        press(&mut app, Char('K'));
        press(&mut app, Char('K'));
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("a"));
        press(&mut app, Char('J'));
        frame(&mut app);
        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        frame(&mut app);
        assert_eq!(app.status.as_deref(), Some("Changes wrapped to the top"));
        assert_eq!(file_name(&app).as_deref(), Some("a"));
    }

    #[test]
    fn k_before_the_first_change_goes_to_the_last_change_of_the_previous_file() {
        let raw = format!("{}{}", hunk("a", "-x\n y\n-z\n w\n"), hunk("b", " p\n-q\n"));
        let mut app = app(&raw);
        press(&mut app, Char('n'));
        assert_eq!(file_name(&app).as_deref(), Some("b"));
        press(&mut app, Char('K'));
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("a"));
        assert_eq!(at(&app), old(3));
    }

    #[test]
    fn it_wraps_at_both_ends_and_says_so() {
        let raw = format!("{}{}", hunk("a", " x\n-y\n"), hunk("b", " p\n-q\n"));
        let mut app = app(&raw);
        press(&mut app, Char('K'));
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("b"));
        assert_eq!(app.status.as_deref(), Some("Changes wrapped to the bottom"));
        press(&mut app, Char('J'));
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("a"));
        assert_eq!(app.status.as_deref(), Some("Changes wrapped to the top"));
    }

    #[test]
    fn with_one_file_it_wraps_inside_it() {
        let mut app = app(&hunk("a", " x\n-y\n z\n-w\n"));
        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(4));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(2));
        assert_eq!(app.status.as_deref(), Some("Changes wrapped to the top"));
    }

    #[test]
    fn files_with_no_text_changes_are_skipped() {
        let raw = format!(
            "{}{}{}{}{}",
            hunk("a", " x\n-y\n"),
            "diff --git a/b b/b\nindex 1..2 100644\nBinary files a/b and b/b differ\n",
            "diff --git a/c b/c\nold mode 100644\nnew mode 100755\n",
            "diff --git a/d b/e\nsimilarity index 100%\nrename from d\nrename to e\n",
            hunk("f", " p\n-q\n"),
        );
        let mut app = app(&raw);
        let stops = lap(&mut app, 'J');
        let names: Vec<_> = stops.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["a", "f"]);
        assert_eq!(lap(&mut app, 'K').len(), 2);
    }

    #[test]
    fn starting_on_a_file_with_no_changes_finds_the_next_one() {
        let raw = format!(
            "{}{}",
            "diff --git a/a b/a\nold mode 100644\nnew mode 100755\n",
            hunk("b", " p\n-q\n"),
        );
        let mut app = app(&raw);
        assert_eq!(file_name(&app).as_deref(), Some("a"));
        press(&mut app, Char('J'));
        frame(&mut app);
        assert_eq!(at(&app), old(2));
    }

    #[test]
    fn a_diff_with_no_changes_says_so() {
        let mut app = app("diff --git a/a b/a\nold mode 100644\nnew mode 100755\n");
        press(&mut app, Char('J'));
        press(&mut app, Char('K'));
        assert_eq!(app.status.as_deref(), Some("No changes in this diff"));
        assert!(app.change_jumps.queued.is_empty());

        let mut empty = App::new(Vec::new(), false, &Config::default());
        frame(&mut empty);
        press(&mut empty, Char('J'));
        assert_eq!(empty.status.as_deref(), Some("No changes in this diff"));
    }

    #[test]
    fn a_collapsed_folder_opens_on_the_way_in() {
        let raw = format!("{}{}", hunk("z/deep/b", " p\n-q\n"), hunk("a", " x\n-y\n"));
        let cfg = Config {
            open_depth: 0,
            ..Config::default()
        };
        let mut app = App::new(parse(&raw), false, &cfg);
        app.install_comments_for_test(CommentStore::disabled());
        app.focus = Focus::Diff;
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("a"));
        assert!(app.collapsed.contains("z"));

        press(&mut app, Char('K'));
        assert_eq!(file_name(&app).as_deref(), Some("z/deep/b"));
        assert!(!app.collapsed.contains("z"));
        assert_eq!(at(&app), old(2));
    }

    #[test]
    fn from_the_tree_they_act_on_the_diff_and_take_focus() {
        let mut app = app(&hunk("f", &edits(2, 3)));
        app.focus = Focus::Tree;
        press(&mut app, Char('J'));
        assert_eq!(app.focus, Focus::Diff);
        assert_eq!(at(&app), old(4));
    }

    #[test]
    fn with_a_folder_selected_j_starts_inside_it() {
        let raw = format!(
            "{}{}{}",
            hunk("d/a", " x\n-y\n"),
            hunk("e/b", " p\n-q\n"),
            hunk("f", " r\n-s\n"),
        );
        let mut app = app(&raw);
        let folder = |name: &str, app: &App| {
            app.rows.iter().position(
                |r| matches!(&r.kind, crate::tree::RowKind::Dir { path, .. } if path == name),
            )
        };
        app.tree_state.select(folder("e", &app));
        frame(&mut app);
        press(&mut app, Char('J'));
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("e/b"));

        app.tree_state.select(folder("e", &app));
        frame(&mut app);
        press(&mut app, Char('K'));
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("d/a"));
    }

    #[test]
    fn with_comments_off_each_press_scrolls_to_the_next_change() {
        let mut app = app_with(&hunk("f", &edits(4, 30)), false, false);
        let mut scrolls = Vec::new();
        for _ in 0..4 {
            press(&mut app, Char('J'));
            scrolls.push(app.diff_scroll);
        }
        assert!(
            scrolls.is_sorted() && scrolls[0] < scrolls[3],
            "{scrolls:?}"
        );
        press(&mut app, Char('K'));
        assert_eq!(app.diff_scroll, scrolls[2]);
    }

    /// The end of the file holds the view back from a third of the way down, so
    /// the view alone can't say which change the last press stopped on.
    #[test]
    fn with_comments_off_presses_advance_where_the_view_cannot_follow() {
        let mut app = app_with(&hunk("f", " a\n-b\n c\n-d\n e\n-f\n g\n"), false, false);
        let stops = lap(&mut app, 'J');
        assert_eq!(stops.len(), 3);
        assert_eq!(app.status.as_deref(), Some("Changes wrapped to the top"));
        assert_eq!(lap(&mut app, 'K').len(), 3);
    }

    #[test]
    fn with_comments_off_a_change_on_the_first_line_is_not_skipped() {
        let mut app = app_with(&hunk("f", &format!("-a\n{}", edits(1, 40))), false, false);
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(1));
    }

    #[test]
    fn with_comments_off_a_scroll_starts_the_next_jump_from_the_view() {
        let mut app = app_with(&hunk("f", &edits(8, 6)), false, false);
        for _ in 0..6 {
            press(&mut app, Char('J'));
        }
        // Scrolling up parks the hidden cursor at the bottom of the view.
        app.scroll_diff(-20);
        frame(&mut app);
        let top = app.diff_scroll;
        press(&mut app, Char('J'));
        let line = app.diff_cursor;
        let render = app.current_render().unwrap();
        let first_in_view = render.changes.iter().find(|&&l| l > render.line_at(top));
        assert_eq!(Some(&line), first_in_view);
    }

    #[test]
    fn a_burst_of_presses_crosses_files_without_losing_any() {
        let raw = format!(
            "{}{}{}",
            hunk("a", " x\n-y\n z\n-w\n"),
            hunk("b", " x\n-y\n z\n-w\n"),
            hunk("c", " x\n-y\n z\n-w\n"),
        );
        let mut app = app(&raw);
        // All between frames: a's two, b's two, then into c. No key follows to
        // wake the event loop, so the one frame has to get there.
        for _ in 0..5 {
            app.press_for_test(Char('J'), false);
        }
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("c"));
        assert_eq!(at(&app), old(2));
        assert!(app.change_jumps.queued.is_empty());
    }

    #[test]
    fn a_burst_keeps_its_order() {
        let raw = format!("{}{}", hunk("a", " x\n-y\n"), hunk("b", " x\n-y\n z\n-w\n"));
        let mut app = app(&raw);
        press(&mut app, Char('J'));
        for key in ['J', 'J', 'K'] {
            app.press_for_test(Char(key), false);
        }
        for _ in 0..3 {
            frame(&mut app);
        }
        assert_eq!(file_name(&app).as_deref(), Some("b"));
        assert_eq!(at(&app), old(2));
    }

    #[test]
    fn another_key_that_changes_file_wins_over_a_jump_in_flight() {
        let raw = format!(
            "{}{}{}",
            hunk("a", " x\n-y\n"),
            hunk("b", &edits(2, 30)),
            hunk("c", " x\n-y\n"),
        );
        let mut app = app(&raw);
        press(&mut app, Char('J'));
        app.press_for_test(Char('J'), false); // Into b, waiting on its render.
        app.press_for_test(Char('J'), false);
        app.press_for_test(Char('n'), false);
        frame(&mut app);
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("c"));
        assert!(app.change_jumps.queued.is_empty());
        assert!(app.change_jumps.landing.is_none());
    }

    #[test]
    fn a_press_behind_a_comment_jump_goes_second() {
        let raw = format!(
            "{}{}",
            hunk("f", " a\n+b\n c\n"),
            hunk("g", " x\n-y\n z\n-w\n")
        );
        let mut app = App::new(parse(&raw), false, &Config::default());
        let mut store = CommentStore::disabled();
        store.add(note("g", 1));
        app.install_comments_for_test(store);
        app.focus = Focus::Diff;
        frame(&mut app);
        app.press_for_test(Char(']'), false);
        app.press_for_test(Char('J'), false);
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("g"));
        assert_eq!(at(&app), old(2));
    }

    #[test]
    fn a_move_in_the_same_file_goes_before_a_press_behind_it() {
        let raw = hunk(
            "f",
            " keep\n-fn alpha_value() {\n-    first_call(1);\n-    second_call(2);\n mid\n\
             -tail\n mid\n+fn alpha_value() {\n+    first_call(1);\n+    second_call(2);\n end\n",
        );
        let mut app = app(&raw);
        press(&mut app, Char('J'));
        app.press_for_test(Char('m'), false);
        app.press_for_test(Char('K'), false);
        frame(&mut app);
        assert_eq!(at(&app), old(6), "K from the far end of the move");
    }

    #[test]
    fn a_reload_abandons_a_jump_into_a_file_that_may_have_moved() {
        let raw = format!("{}{}", hunk("a", " x\n-y\n"), hunk("b", " p\n-q\n"));
        let mut app = app(&raw);
        press(&mut app, Char('J'));
        app.press_for_test(Char('J'), false);
        app.reload_files(parse(&hunk("a", " x\n-y\n")));
        frame(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("a"));
        assert!(app.change_jumps.landing.is_none());
    }

    #[test]
    fn with_comments_off_a_refresh_does_not_lose_the_stop() {
        let raw = hunk("f", " a\n-b\n c\n-d\n e\n");
        let mut app = app_with(&raw, false, false);
        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        app.reload_in_place(parse(&raw));
        frame(&mut app);
        press(&mut app, Char('K'));
        assert_eq!(at(&app), old(2));
    }

    #[test]
    fn a_press_straight_after_a_reload_reads_the_new_diff() {
        let raw = hunk("a", " x\n-y\n");
        let mut app = app(&raw);
        app.reload_in_place(parse(&hunk("a", " x\n y\n z\n-w\n")));
        app.press_for_test(Char('J'), false);
        frame(&mut app);
        assert_eq!(at(&app), old(4));
    }

    #[test]
    fn a_search_keeps_its_place_through_a_jump() {
        let raw = hunk("f", " find\n-a\n find\n-b\n find\n-c\n find\n");
        let mut app = app(&raw);
        press(&mut app, Char('/'));
        for c in "find".chars() {
            press(&mut app, Char(c));
        }
        press(&mut app, Enter);
        frame(&mut app);
        let before = app.search_count();
        assert_eq!(before.as_deref(), Some("1/4"));

        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        assert_eq!(at(&app), old(4));
        assert_eq!(
            app.search_count(),
            before,
            "the current match is left alone"
        );
        assert!(app.searching());

        press(&mut app, Char('n'));
        assert_eq!(at(&app).1, Some(3), "`n` goes on from the cursor");
        assert_eq!(app.search_count().as_deref(), Some("3/4"));
    }

    /// The stop and the match are on one side-by-side line, whose two sides are on
    /// different lines in unified: the match is what the cursor stays with.
    #[test]
    fn a_match_on_a_stop_keeps_the_cursor_through_a_layout_change() {
        let raw = hunk("f", " a\n-b\n+find\n c\n");
        let mut app = app_with(&raw, true, true);
        press(&mut app, Char('/'));
        for c in "find".chars() {
            press(&mut app, Char(c));
        }
        press(&mut app, Enter);
        frame(&mut app);
        press(&mut app, Char('K'));
        frame(&mut app);
        press(&mut app, Char('J'));
        frame(&mut app);
        press(&mut app, Char('n'));
        frame(&mut app);
        press(&mut app, Char('s'));
        frame(&mut app);
        assert_eq!(at(&app), (None, Some(2)));
    }

    #[test]
    fn anything_with_the_keyboard_keeps_them() {
        let raw = hunk("f", &edits(2, 3));
        let mut typing = app(&raw);
        press(&mut typing, Char('c'));
        press(&mut typing, Char('J'));
        assert_eq!(typing.composer.as_ref().unwrap().body(), "J");
        assert!(typing.change_jumps.queued.is_empty());

        let mut finding = app(&raw);
        press(&mut finding, Char('t'));
        press(&mut finding, Char('J'));
        assert_eq!(finding.finder.as_ref().unwrap().query, "J");

        let mut searching = app(&raw);
        press(&mut searching, Char('/'));
        press(&mut searching, Char('K'));
        assert_eq!(searching.search_prompt(), Some("K"));

        let mut helping = app(&raw);
        let before = at(&helping);
        press(&mut helping, Char('?'));
        press(&mut helping, Char('J'));
        assert!(helping.show_help);
        assert_eq!(at(&helping), before);
    }

    /// Renders that are late: a frame goes ahead without them.
    fn late(app: &mut App) -> Gate {
        let gate = Gate::default();
        app.cache.wait(Duration::ZERO);
        app.cache.hooks().start = Some(gate.clone());
        gate
    }

    /// Passes of the event loop until the renders have all landed.
    fn settle(app: &mut App) {
        for _ in 0..200 {
            frame(app);
            app.cache.finish();
            frame(app);
            if !app.change_jumps_waiting() && app.held_keys.is_empty() {
                return;
            }
        }
        panic!("never settled");
    }

    /// Every file has its render land after a frame has gone by without it, so each
    /// hop of the burst really does have to wait.
    #[test]
    fn a_burst_survives_every_render_being_late() {
        let files: String = (0..10)
            .map(|at| hunk(&format!("f{at}"), " x\n-y\n z\n-w\n q\n-r\n"))
            .collect();
        let mut late_app = app(&files);
        let mut prompt = app(&files);
        late(&mut late_app).open();
        for _ in 0..25 {
            late_app.press_for_test(Char('J'), false);
            press(&mut prompt, Char('J'));
        }
        settle(&mut late_app);
        assert_eq!(file_name(&late_app), file_name(&prompt));
        assert_eq!(at(&late_app), at(&prompt));
    }

    #[test]
    fn a_press_held_behind_a_jump_keeps_its_place_in_line() {
        let raw = format!(
            "{}{}",
            hunk("a", " x\n-y\n"),
            hunk("b", " p\n q\n-r\n s\n t\n-u\n")
        );
        let mut app = app(&raw);
        press(&mut app, Char('J'));
        late(&mut app).open();
        for key in ['J', 'j', 'J', 'k'] {
            app.press_for_test(Char(key), false);
        }
        settle(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("b"));
        // Into b's first change, down a line, on to its second, and back a line.
        assert_eq!(at(&app), (Some(5), Some(4)));
    }

    #[test]
    fn a_key_that_changes_file_leaves_what_was_held_behind() {
        let raw = format!(
            "{}{}{}",
            hunk("a", " x\n-y\n"),
            hunk("b", &edits(2, 3)),
            hunk("c", " p\n-q\n")
        );
        let mut app = app(&raw);
        let gate = late(&mut app);
        press(&mut app, Char('J'));
        press(&mut app, Char('J'));
        assert_eq!(file_name(&app).as_deref(), Some("b"));
        assert!(app.change_jumps_waiting());
        press(&mut app, Char('j'));
        assert_eq!(app.held_keys.len(), 1);

        press(&mut app, Char('n'));
        assert!(app.held_keys.is_empty());
        gate.open();
        settle(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("c"));
        assert_eq!(
            at(&app),
            (Some(1), Some(1)),
            "where a first look puts the cursor"
        );
    }

    #[test]
    fn quitting_does_not_wait_for_a_render() {
        let mut app = app(&hunk("f", &edits(2, 3)));
        late(&mut app);
        app.cache.clear();
        frame(&mut app);
        assert!(app.current_render().is_none());
        press(&mut app, Char('j'));
        press(&mut app, Char('q'));
        assert!(app.quit);
    }

    /// Nothing here has a render to read, and all of it has to survive that.
    #[test]
    fn every_key_copes_with_a_file_that_has_no_render_yet() {
        let raw = format!("{}{}", hunk("a", &edits(3, 3)), hunk("b", &edits(3, 3)));
        let keys = [
            Char('j'),
            Char('k'),
            Char('g'),
            Char('G'),
            Char(' '),
            Char('b'),
            Char('c'),
            Char('m'),
            Char('x'),
            Char(']'),
            Char('['),
            Char('J'),
            Char('K'),
            Char('y'),
            Char('v'),
            Char('i'),
            Char('e'),
            Char('s'),
            Char('T'),
            KeyCode::PageDown,
            KeyCode::PageUp,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Tab,
            KeyCode::Left,
            KeyCode::Right,
        ];
        for key in keys {
            let mut app = app(&raw);
            let gate = late(&mut app);
            app.cache.clear();
            frame(&mut app);
            assert!(app.current_render().is_none());
            press(&mut app, key);
            press(&mut app, key);
            frame_at(&mut app, 60, HEIGHT);
            gate.open();
            settle(&mut app);
            if app.composer.is_some() {
                press(&mut app, KeyCode::Esc);
                press(&mut app, KeyCode::Esc);
            }
            if let Some(render) = app.current_render() {
                assert!(app.diff_cursor < render.lines().max(1), "{key:?}");
            }
        }
    }

    /// The cursor's token used to be noted before the render was there, which would
    /// leave the first look at a file with its cursor on the header.
    #[test]
    fn a_late_render_still_starts_the_cursor_on_the_first_line_of_code() {
        let raw = format!("{}{}", hunk("a", " x\n-y\n"), hunk("b", " p\n q\n"));
        let mut app = app(&raw);
        let gate = late(&mut app);
        press(&mut app, Char('n'));
        frame(&mut app);
        assert!(app.current_render().is_none());
        gate.open();
        settle(&mut app);
        let first = app.current_render().unwrap().line_map.first_code_row();
        assert_eq!(Some(app.diff_cursor), first);
    }

    #[test]
    fn a_late_render_after_s_puts_the_cursor_back_on_its_line() {
        let mut app = app(&hunk("f", &edits(6, 2)));
        for _ in 0..4 {
            press(&mut app, Char('J'));
        }
        let line = at(&app);
        let gate = late(&mut app);
        press(&mut app, Char('s'));
        assert!(app.current_render().is_none());
        press(&mut app, Char('j'));
        gate.open();
        settle(&mut app);
        assert_eq!(at(&app).0, line.0.map(|old| old + 1));
    }

    #[test]
    fn a_comment_jump_waits_for_its_file_and_keeps_its_aim() {
        let raw = format!("{}{}", hunk("a", " x\n y\n"), hunk("b", " p\n q\n r\n s\n"));
        let mut store = CommentStore::disabled();
        store.add(note("b", 3));
        let mut app = App::new(parse(&raw), false, &Config::default());
        app.install_comments_for_test(store);
        app.focus = Focus::Diff;
        frame(&mut app);
        let gate = late(&mut app);
        press(&mut app, Char(']'));
        assert_eq!(file_name(&app).as_deref(), Some("b"));
        frame(&mut app);
        assert!(app.comment_landing.is_some(), "still on its way");
        gate.open();
        settle(&mut app);
        assert!(app.comment_landing.is_none());
        let block = app.current_render().unwrap().comment_rows[0].start;
        assert_eq!(app.diff_cursor, block);
    }

    #[test]
    fn a_jump_a_later_key_overtook_does_not_land_on_the_wrong_file() {
        let raw = format!(
            "{}{}{}",
            hunk("a", " x\n y\n"),
            hunk("b", " p\n q\n r\n s\n"),
            hunk("c", " p\n q\n r\n s\n"),
        );
        let mut store = CommentStore::disabled();
        store.add(note("b", 3));
        store.add(note("c", 2));
        let mut app = App::new(parse(&raw), false, &Config::default());
        app.install_comments_for_test(store);
        app.focus = Focus::Diff;
        frame(&mut app);
        let gate = late(&mut app);
        press(&mut app, Char(']'));
        press(&mut app, Char('n'));
        gate.open();
        settle(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("c"));
        assert!(app.comment_landing.is_none());
        let first = app.current_render().unwrap().line_map.first_code_row();
        assert_eq!(Some(app.diff_cursor), first, "c has its own start");
    }

    #[test]
    fn a_move_jump_waits_for_its_file() {
        let body =
            "fn moved_away() {\n    first_call(1);\n    second_call(2);\n    third_call(3);\n}\n";
        let minus: String = body.lines().map(|l| format!("-{l}\n")).collect();
        let plus: String = body.lines().map(|l| format!("+{l}\n")).collect();
        let raw = format!("{}{}", hunk("a", &minus), hunk("b", &plus));
        let mut app = app(&raw);
        let gate = late(&mut app);
        press(&mut app, Char('m'));
        assert_eq!(file_name(&app).as_deref(), Some("b"));
        frame(&mut app);
        assert!(app.move_landing.is_some(), "waiting for b");
        press(&mut app, Char('m')); // Would read a render that isn't there.
        assert_eq!(app.held_keys.len(), 1);
        gate.open();
        settle(&mut app);
        assert_eq!(file_name(&app).as_deref(), Some("a"), "there and back");
    }

    #[test]
    fn opening_the_search_drops_held_keys_rather_than_typing_them() {
        let mut app = app(&hunk("f", &edits(3, 3)));
        let gate = late(&mut app);
        app.cache.clear();
        frame(&mut app);
        press(&mut app, Char('j'));
        press(&mut app, Char('/'));
        press(&mut app, Char('z'));
        gate.open();
        settle(&mut app);
        assert_eq!(app.search_prompt(), Some("z"));
    }

    #[test]
    fn a_change_of_focus_drops_keys_meant_for_the_other_pane() {
        let mut app = app(&hunk("f", &edits(3, 3)));
        let gate = late(&mut app);
        app.cache.clear();
        frame(&mut app);
        press(&mut app, Char('j'));
        press(&mut app, KeyCode::Tab);
        assert!(app.held_keys.is_empty());
        gate.open();
        settle(&mut app);
        assert_eq!(app.focus, Focus::Tree);
    }

    #[test]
    fn a_comment_key_held_for_a_render_opens_the_composer_for_what_follows() {
        let mut app = app(&hunk("f", &edits(3, 3)));
        let gate = late(&mut app);
        app.cache.clear();
        frame(&mut app);
        press(&mut app, Char('c'));
        press(&mut app, Char('j')); // Held behind it, and so text.
        gate.open();
        settle(&mut app);
        assert_eq!(app.composer.as_ref().unwrap().body(), "j");
    }

    #[test]
    fn the_wheel_over_a_file_with_no_render_goes_nowhere() {
        let mut app = app(&hunk("f", &edits(9, 3)));
        late(&mut app);
        app.cache.clear();
        frame(&mut app);
        app.scroll_at(ratatui::layout::Position::new(70, 5), 1);
        assert_eq!(app.diff_scroll, 0);
    }

    /// On a reload, `+` holds the cursor's row on screen until the render exists.
    #[test]
    fn a_held_screen_row_waits_for_a_late_render() {
        let mut app = app(&hunk("f", &edits(8, 3)));
        for _ in 0..5 {
            press(&mut app, Char('J'));
        }
        let row = cursor_row(&app);
        app.hold_cursor_on_screen();
        let gate = late(&mut app);
        app.reload_in_place(parse(&hunk("f", &edits(8, 4))));
        frame(&mut app);
        assert!(app.cursor_screen_row.is_some());
        gate.open();
        settle(&mut app);
        assert!(app.cursor_screen_row.is_none());
        assert_eq!(cursor_row(&app), row);
    }

    /// A file taller than a `u16` can count, the way each layout gets there.
    #[test]
    fn rows_past_a_u16_can_all_be_reached() {
        let mut body: String = (0..70_000).map(|i| format!(" context {i}\n")).collect();
        body.push_str("-old\n+new\n tail\n");
        for side_by_side in [false, true] {
            let mut app = app_with(&hunk("a", &body), side_by_side, true);
            press(&mut app, Char('G'));
            let render = app.current_render().unwrap();
            assert!(render.height > u32::from(u16::MAX), "{}", render.height);
            assert!(app.diff_scroll > u32::from(u16::MAX), "{}", app.diff_scroll);
            assert_eq!(app.diff_cursor, render.lines() - 1);

            press(&mut app, Char('g'));
            press(&mut app, Char('J'));
            let (row, scroll) = (cursor_row(&app), app.diff_scroll);
            assert!(scroll > 70_000 - u32::from(HEIGHT), "{scroll}");
            assert!((0..i64::from(app.diff_height)).contains(&row), "{row}");
            assert_eq!(at(&app).0, Some(70_001));

            app.scroll_diff(-1);
            assert_eq!(app.diff_scroll, scroll - 1, "a step, not a wrap");
        }
    }

    /// `G` sets the scroll for the next draw to clamp, which a key can get in ahead of.
    #[test]
    fn a_key_straight_after_g_is_not_thrown_by_its_scroll() {
        let mut app = app(&hunk("a", &edits(3, 30)));
        app.press_for_test(Char('G'), false);
        app.press_for_test(Char('k'), false);
        app.press_for_test(Char('j'), false);
        frame(&mut app);
        assert_eq!(app.diff_cursor, app.current_render().unwrap().lines() - 1);
    }
}

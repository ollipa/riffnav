//! Clicks, the wheel, and dragging the divider.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Position;

use crate::tree::RowKind;

use super::{App, Focus, MIN_DIFF_WIDTH};

impl App {
    /// Route a mouse event to the pane under the cursor. Overlays own the whole
    /// screen, so clicks and scrolls beneath them are ignored.
    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        let prompt = self.search_prompt().is_some();
        if self.finder.is_some() || self.show_help || self.composer.is_some() || prompt {
            return;
        }
        let pos = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.on_divider(pos) {
                    self.dragging_divider = true;
                } else {
                    self.click(pos);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_divider => {
                self.drag_divider(pos.x);
            }
            MouseEventKind::Up(_) => self.dragging_divider = false,
            MouseEventKind::ScrollDown => self.scroll_at(pos, 1),
            MouseEventKind::ScrollUp => self.scroll_at(pos, -1),
            _ => {}
        }
    }

    /// Whether `pos` sits on the tree pane's right border, which doubles as the
    /// drag handle for resizing the panes.
    fn on_divider(&self, pos: Position) -> bool {
        self.tree_area
            .is_some_and(|a| a.contains(pos) && pos.x == a.right().saturating_sub(1))
    }

    /// Move the divider to screen column `x`: the tree grows or shrinks so its
    /// border lands under the cursor, clamped between the configured width (the
    /// floor) and whatever leaves the diff its minimum width.
    fn drag_divider(&mut self, x: u16) {
        let (Some(tree), Some(diff)) = (self.tree_area, self.diff_area) else {
            return;
        };
        let total = tree.width + diff.width;
        let max = total
            .saturating_sub(MIN_DIFF_WIDTH)
            .max(self.tree_width_min);
        self.tree_width = (x.saturating_sub(tree.x) + 1).clamp(self.tree_width_min, max);
    }

    /// Left-click: select the tree row under the cursor (folding/unfolding a
    /// directory, like a file explorer), or focus the diff pane and put the line
    /// cursor on the clicked line.
    pub(crate) fn click(&mut self, pos: Position) {
        if let Some(area) = self.tree_area
            && area.contains(pos)
        {
            // The list has no top border, so its first visible row sits at the
            // pane's top edge; add the scroll offset to map a screen row to a
            // row index.
            let line = (pos.y - area.y) as usize + self.tree_state.offset();
            if line < self.rows.len() {
                self.focus = Focus::Tree;
                self.select(line);
                if matches!(self.rows[line].kind, RowKind::Dir { .. }) {
                    self.toggle_fold();
                }
            }
        } else if let Some(area) = self.diff_area
            && area.contains(pos)
        {
            self.focus = Focus::Diff;
            self.click_diff_row(pos.y - area.y);
        }
    }

    /// Put the line cursor on whatever line occupies screen row `y` of the diff
    /// pane. The line is already on screen, so the view is left where it is
    /// rather than scrolled to pad the cursor away from the edge.
    fn click_diff_row(&mut self, y: u16) {
        let Some(render) = self.current_render() else {
            return;
        };
        // Clicks on the empty space below a short diff land nowhere.
        let row = self.diff_scroll.saturating_add(y.into());
        if row >= render.height {
            return;
        }
        self.diff_cursor = render.line_at(row);
        self.remember_cursor_anchor();
    }

    /// Wheel scrolling acts on whichever pane the cursor is over, independent of
    /// keyboard focus: the tree moves its selection, the diff scrolls.
    pub(super) fn scroll_at(&mut self, pos: Position, dir: i32) {
        if self.tree_area.is_some_and(|a| a.contains(pos)) {
            self.move_selection(dir as isize);
        } else if self.current_render().is_some() {
            self.scroll_diff(dir * 3);
        }
    }
}

#[cfg(test)]
mod tests {

    use ratatui::layout::{Position, Rect};

    use crate::app::test_support::{app_with, file};
    use crate::app::{Focus, MIN_DIFF_WIDTH};

    #[test]
    fn divider_drag_resizes_within_bounds() {
        let mut app = app_with(vec![file("a.rs")]);
        // Default config: tree is 32 wide, so its border sits on column 31.
        app.tree_area = Some(Rect::new(0, 0, 32, 10));
        app.diff_area = Some(Rect::new(32, 1, 68, 9));

        assert!(app.on_divider(Position::new(31, 4)));
        assert!(!app.on_divider(Position::new(30, 4)));
        assert!(!app.on_divider(Position::new(32, 4)));

        // Dragging right widens the tree so the border follows the cursor.
        app.drag_divider(50);
        assert_eq!(app.tree_width, 51);
        // The starting width is the floor…
        app.drag_divider(5);
        assert_eq!(app.tree_width, 32);
        // …and the diff keeps its minimum width at the other extreme.
        app.drag_divider(99);
        assert_eq!(app.tree_width, 100 - MIN_DIFF_WIDTH);
    }

    #[test]
    fn click_selects_the_file_row_under_the_cursor() {
        let mut app = app_with(vec![file("a.rs"), file("b.rs"), file("c.rs")]);
        app.tree_area = Some(Rect::new(0, 0, 30, 10));

        // The three files flatten to rows 0..3; a click on the second screen
        // row selects that row and pulls focus to the tree.
        app.click(Position::new(4, 1));
        assert_eq!(app.selected_index(), 1);
        assert_eq!(app.focus, Focus::Tree);
        assert!(app.selected_file().is_some());
    }

    #[test]
    fn click_honors_the_list_scroll_offset() {
        let mut app = app_with(vec![
            file("a.rs"),
            file("b.rs"),
            file("c.rs"),
            file("d.rs"),
            file("e.rs"),
        ]);
        app.tree_area = Some(Rect::new(0, 0, 30, 3));
        // The list is scrolled so row 2 is at the top of the pane.
        *app.tree_state.offset_mut() = 2;

        // Second visible screen row -> rows[2 + 1].
        app.click(Position::new(4, 1));
        assert_eq!(app.selected_index(), 3);
    }

    #[test]
    fn click_on_a_directory_toggles_its_fold() {
        // open_depth defaults to 64, so `dir/` starts expanded: rows are
        // [dir, a.rs, b.rs].
        let mut app = app_with(vec![file("dir/a.rs"), file("dir/b.rs")]);
        app.tree_area = Some(Rect::new(0, 0, 30, 10));
        assert_eq!(app.rows.len(), 3);

        // Clicking the directory row collapses it, hiding its children.
        app.click(Position::new(2, 0));
        assert_eq!(app.selected_index(), 0);
        assert_eq!(app.rows.len(), 1);

        // Clicking it again expands it back.
        app.click(Position::new(2, 0));
        assert_eq!(app.rows.len(), 3);
    }

    #[test]
    fn click_below_the_last_row_is_ignored() {
        let mut app = app_with(vec![file("a.rs")]);
        app.tree_area = Some(Rect::new(0, 0, 30, 10));
        app.focus = Focus::Diff;

        // Empty space well past the single row: nothing selected, focus stays.
        app.click(Position::new(4, 7));
        assert_eq!(app.selected_index(), 0);
        assert_eq!(app.focus, Focus::Diff);
    }

    #[test]
    fn click_in_the_diff_pane_focuses_it() {
        let mut app = app_with(vec![file("a.rs")]);
        app.tree_area = Some(Rect::new(0, 0, 30, 10));
        app.diff_area = Some(Rect::new(30, 1, 50, 9));
        app.focus = Focus::Tree;

        app.click(Position::new(40, 4));
        assert_eq!(app.focus, Focus::Diff);
    }
}

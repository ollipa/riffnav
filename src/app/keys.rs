//! Routing keys to what they do, and holding them while a render is due.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, Direction, Focus, HALF_PAGE};

impl App {
    /// Whether `key` has to wait its turn. A key that only makes sense against the
    /// render does, as does any after it, to stay in order. Anything else can go
    /// straight away, which is what keeps `q` and `n` working on a file that's
    /// taking its time.
    fn must_wait(&self, key: KeyEvent) -> bool {
        let reads_render = self.reads_render(key);
        // `J` and `K` keep a queue of their own, for what comes after them.
        let queues = matches!(key.code, KeyCode::Char('J' | 'K'));
        reads_render && (!self.held_keys.is_empty() || !(queues || self.view_settled()))
    }

    fn reads_render(&self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let diff = self.focus == Focus::Diff;
        let comments = self.comments_on;
        match key.code {
            KeyCode::Char('J' | 'K' | 'g' | 'G') => true,
            KeyCode::Char('n' | 'N') => self.searching(),
            KeyCode::Char('d' | 'u') => ctrl,
            KeyCode::Char('j' | 'k' | ' ' | 'b' | 'm') => diff,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => diff,
            KeyCode::Char('c') => comments && !ctrl,
            KeyCode::Char('x' | ']' | '[') => comments,
            _ => false,
        }
    }

    /// Act on the keys that have been waiting, for as long as they can. One of them
    /// can move the view on, or open the composer, and the rest go where they would
    /// have gone had the render been there all along.
    pub(super) fn replay_held_keys(&mut self) {
        while self.view_settled() {
            let Some(key) = self.held_keys.pop_front() else {
                return;
            };
            let _ = self.route_key(key, true);
        }
    }

    pub(super) fn handle_event(&mut self) -> Result<()> {
        let mut ev = event::read()?;
        // Coalesce a burst of resize events (e.g. a drag) into the last one.
        while matches!(ev, Event::Resize(..)) && event::poll(Duration::ZERO)? {
            ev = event::read()?;
        }
        if let Event::Mouse(mouse) = ev {
            let selected = self.selected_index();
            self.handle_mouse(mouse);
            if self.selected_index() != selected {
                self.held_keys.clear();
            }
            return Ok(());
        }
        let Event::Key(key) = ev else {
            return Ok(());
        };
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        self.dispatch_key(key)
    }

    /// Act on one keypress. A key that moves the view or the focus on, or takes over
    /// the keyboard, leaves anything still waiting behind: the later key wins, and
    /// what was held was meant for a view that's gone.
    pub(super) fn dispatch_key(&mut self, key: KeyEvent) -> Result<()> {
        let (selected, focus) = (self.selected_index(), self.focus);
        let owned = self.keyboard_taken();
        let result = self.route_key(key, false);
        let moved_on = self.selected_index() != selected || self.focus != focus;
        if moved_on || (!owned && self.keyboard_taken()) {
            self.held_keys.clear();
        }
        result
    }

    /// Whether something other than the panes is getting the keys.
    fn keyboard_taken(&self) -> bool {
        self.composer.is_some()
            || self.finder.is_some()
            || self.search_prompt().is_some()
            || self.show_help
            || self.confirm_delete.is_some()
    }

    /// Route one keypress to whatever currently owns input: the composer, the
    /// finder, the help overlay, or the panes themselves. `held` is for a key that
    /// has already waited its turn.
    fn route_key(&mut self, key: KeyEvent, held: bool) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // A note being typed captures all input, printable keys included.
        if self.composer.is_some() {
            self.composer_key(key);
            return Ok(());
        }

        // The fuzzy finder captures all input while open.
        if self.finder.is_some() {
            match key.code {
                KeyCode::Esc => self.finder = None,
                KeyCode::Enter => {
                    let target = self
                        .finder
                        .as_ref()
                        .and_then(|f| f.matches.get(f.selected).copied());
                    self.finder = None;
                    if let Some(idx) = target {
                        self.reveal_file(idx);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(f) = self.finder.as_mut() {
                        f.query.pop();
                    }
                    self.finder_recompute();
                }
                KeyCode::Up => {
                    if let Some(f) = self.finder.as_mut() {
                        f.selected = f.selected.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    if let Some(f) = self.finder.as_mut()
                        && f.selected + 1 < f.matches.len()
                    {
                        f.selected += 1;
                    }
                }
                KeyCode::Char('p') if ctrl => {
                    if let Some(f) = self.finder.as_mut() {
                        f.selected = f.selected.saturating_sub(1);
                    }
                }
                KeyCode::Char('n') if ctrl => {
                    if let Some(f) = self.finder.as_mut()
                        && f.selected + 1 < f.matches.len()
                    {
                        f.selected += 1;
                    }
                }
                KeyCode::Char(c) if !ctrl => {
                    if let Some(f) = self.finder.as_mut() {
                        f.query.push(c);
                    }
                    self.finder_recompute();
                }
                _ => {}
            }
            return Ok(());
        }

        // So does the search prompt.
        if self.search_prompt().is_some() {
            self.prompt_key(key);
            return Ok(());
        }

        // The help overlay swallows all input until dismissed.
        if self.show_help {
            if matches!(
                key.code,
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')
            ) {
                self.show_help = false;
            }
            return Ok(());
        }

        // A delete waiting on y/N owns the next key, whatever it is — including
        // `q` and `Esc`, which read as backing out of the question rather than
        // out of riffnav.
        if self.confirm_delete.is_some() {
            self.answer_delete(key.code);
            return Ok(());
        }

        if !held && self.must_wait(key) {
            self.held_keys.push_back(key);
            return Ok(());
        }

        self.clear_status();
        match key.code {
            // A search is the first thing Esc backs out of.
            KeyCode::Esc if self.searching() => self.clear_search(),
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            // In the diff pane j/k move the line cursor (scrolling the pane once
            // it nears an edge), matching how the tree moves its selection. With
            // comments off there's no cursor, so they scroll as they always did.
            KeyCode::Char('j') | KeyCode::Down => match (self.focus, self.comments_on) {
                (Focus::Tree, _) => self.move_selection(1),
                (Focus::Diff, true) => self.move_cursor(1),
                (Focus::Diff, false) => self.scroll_diff(1),
            },
            KeyCode::Char('k') | KeyCode::Up => match (self.focus, self.comments_on) {
                (Focus::Tree, _) => self.move_selection(-1),
                (Focus::Diff, true) => self.move_cursor(-1),
                (Focus::Diff, false) => self.scroll_diff(-1),
            },
            KeyCode::Char('n') if self.searching() => self.step_match(Direction::Forward),
            KeyCode::Char('N') if self.searching() => self.step_match(Direction::Back),
            KeyCode::Char('n') => self.jump_file(Direction::Forward),
            KeyCode::Char('p') | KeyCode::Char('N') => self.jump_file(Direction::Back),
            KeyCode::Char('J') => self.jump_change(Direction::Forward),
            KeyCode::Char('K') => self.jump_change(Direction::Back),
            KeyCode::Char('d') if ctrl => self.scroll_diff(HALF_PAGE),
            KeyCode::Char('u') if ctrl => self.scroll_diff(-HALF_PAGE),
            KeyCode::PageDown => self.page_move(true),
            KeyCode::PageUp => self.page_move(false),
            KeyCode::Char('g') => {
                self.diff_scroll = 0;
                self.set_cursor(0);
            }
            KeyCode::Char('G') => {
                self.diff_scroll = u32::MAX; // clamped on draw
                let last = self.current_render().map(|r| r.lines().saturating_sub(1));
                if let Some(last) = last {
                    self.diff_cursor = last;
                    self.remember_cursor_anchor();
                }
            }
            KeyCode::Enter => self.toggle_fold(),
            // less-style paging of the diff: Space forward, b back. Diff-focused
            // only — in the tree, Enter folds and paging the selection with Space
            // would surprise.
            KeyCode::Char(' ') if self.focus == Focus::Diff => self.page_move(true),
            KeyCode::Char('b') if self.focus == Focus::Diff => self.page_move(false),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Tree => Focus::Diff,
                    Focus::Diff => Focus::Tree,
                }
            }
            KeyCode::Left => {
                if self.show_tree {
                    self.focus = Focus::Tree;
                }
            }
            KeyCode::Right => self.focus = Focus::Diff,
            KeyCode::Char('s') => self.side_by_side = !self.side_by_side,
            KeyCode::Char('e') => {
                self.show_tree = !self.show_tree;
                if !self.show_tree {
                    self.focus = Focus::Diff;
                }
            }
            KeyCode::Char('t') => self.open_finder(),
            KeyCode::Char('/') => self.open_search(),
            KeyCode::Char('i') => {
                self.icon_style = self.icon_style.next();
                self.status = Some(format!("Icons: {}", self.icon_style.name()));
            }
            KeyCode::Char('T') => self.cycle_theme(),
            KeyCode::Char('y') => self.copy_path(),
            KeyCode::Char('v') => self.toggle_viewed(),
            KeyCode::Char('V') => self.jump_unviewed(),
            // Only bound on a bare launch (auto-diff mode); inert otherwise.
            KeyCode::Char('d') if self.can_cycle_source() => self.cycle_diff_source(),
            // The same views `d` steps through, by their position in that cycle,
            // for jumping straight to one. A digit past the end is inert.
            KeyCode::Char(c) if c.is_ascii_digit() && self.can_cycle_source() => {
                self.select_diff_source(c.to_digit(10).unwrap_or(0) as usize);
            }
            // Only bound where the diff can be re-read; a piped one can't be.
            KeyCode::Char('r') if self.can_refresh() => self.refresh_diff(),
            // Context around each change; re-runs the diff, so the same rule.
            KeyCode::Char('+') if self.can_refresh() => self.change_context(true),
            KeyCode::Char('-') if self.can_refresh() => self.change_context(false),
            // Only bound inside herdr; an inert no-op elsewhere.
            KeyCode::Char('z') if self.herdr.is_some() => self.toggle_herdr_zoom(),
            // Only bound when a supported forge (e.g. GitHub) is detected.
            KeyCode::Char('W') if self.forge.is_some() => self.open_web_diff(),
            KeyCode::Char('o') => {
                if let Some(idx) = self.selected_file() {
                    self.pending_editor = Some(self.files[idx].path().to_string());
                }
            }
            // Comment keys, bound only when comments are on — like `d`/`z`/`W`,
            // they're inert rather than misleading when the feature is off.
            KeyCode::Char('c') if self.comments_on => self.start_comment(),
            KeyCode::Char('x') if self.comments_on => self.delete_comment(),
            KeyCode::Char('m') if self.focus == Focus::Diff => self.jump_move(),
            KeyCode::Char(']') if self.comments_on => self.jump_comment(Direction::Forward),
            KeyCode::Char('[') if self.comments_on => self.jump_comment(Direction::Back),
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
        Ok(())
    }
}

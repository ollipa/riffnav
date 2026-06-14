use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::widgets::ListState;

use crate::delta::RenderCache;
use crate::diff::FileDiff;
use crate::tree::{self, Node, Row, RowKind};

/// Total width of the file-tree pane, including its 1-column right border.
pub const TREE_WIDTH: u16 = 32;
const MIN_DIFF_WIDTH: u16 = 20;
const HALF_PAGE: u16 = 15;

pub struct App {
    pub files: Vec<FileDiff>,
    pub rows: Vec<Row>,
    pub tree_state: ListState,
    pub diff_scroll: u16,
    pub side_by_side: Option<bool>,
    pub cache: RenderCache,
    quit: bool,
    // Diff-pane width of the last render; a change means the terminal resized.
    last_width: u16,
    // Retained so collapsible folders (Phase 3) can re-flatten into `rows`.
    #[allow(dead_code)]
    nodes: Vec<Node>,
}

impl App {
    pub fn new(files: Vec<FileDiff>, side_by_side: Option<bool>) -> Self {
        let nodes = tree::build(&files);
        let rows = tree::flatten(&nodes);
        let first_file = rows
            .iter()
            .position(|r| matches!(r.kind, RowKind::File { .. }))
            .unwrap_or(0);
        let mut tree_state = ListState::default();
        tree_state.select(Some(first_file));

        Self {
            files,
            rows,
            tree_state,
            diff_scroll: 0,
            side_by_side,
            cache: RenderCache::default(),
            quit: false,
            last_width: 0,
            nodes,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        // ratatui::init() enters the alternate screen, enables raw mode, and
        // installs a panic hook that restores the terminal on a crash.
        let mut terminal = ratatui::init();
        let result = self.event_loop(&mut terminal);
        ratatui::restore();
        result
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            let diff_width = diff_pane_width(terminal.size()?.width);

            // On resize, drop renders made at the old width so the cache stays
            // bounded and the current file re-renders to the new width.
            if diff_width != self.last_width {
                self.cache.clear();
                self.last_width = diff_width;
            }

            // Render the selected file (lazily, cached) before drawing.
            if let Some(idx) = self.selected_file() {
                let raw = &self.files[idx].raw;
                self.cache.ensure(idx, raw, diff_width, self.side_by_side)?;
            }

            terminal.draw(|frame| crate::ui::draw(frame, self, diff_width))?;
            self.handle_event()?;
        }
        Ok(())
    }

    pub fn selected_index(&self) -> usize {
        self.tree_state.selected().unwrap_or(0)
    }

    /// The diff index of the selected row, if it is a file (not a directory).
    pub fn selected_file(&self) -> Option<usize> {
        match self.rows.get(self.selected_index())?.kind {
            RowKind::File { diff_index } => Some(diff_index),
            RowKind::Dir { .. } => None,
        }
    }

    pub fn totals(&self) -> (u32, u32) {
        self.files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions))
    }

    fn select(&mut self, index: usize) {
        if index != self.selected_index() {
            self.tree_state.select(Some(index));
            self.diff_scroll = 0;
        }
    }

    /// Move the selection by `delta` rows (directories included).
    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.select((self.selected_index() as isize + delta).clamp(0, max) as usize);
    }

    /// Jump to the next/previous file row, skipping directories.
    fn jump_file(&mut self, forward: bool) {
        let cur = self.selected_index();
        let is_file = |i: &usize| matches!(self.rows[*i].kind, RowKind::File { .. });
        let next = if forward {
            (cur + 1..self.rows.len()).find(is_file)
        } else {
            (0..cur).rev().find(is_file)
        };
        if let Some(i) = next {
            self.select(i);
        }
    }

    fn handle_event(&mut self) -> Result<()> {
        let mut ev = event::read()?;
        // Coalesce a burst of resize events (e.g. a drag) into the last one so
        // we redraw once instead of thrashing delta at every intermediate width.
        while matches!(ev, Event::Resize(..)) && event::poll(Duration::ZERO)? {
            ev = event::read()?;
        }

        let Event::Key(key) = ev else {
            return Ok(());
        };
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('n') => self.jump_file(true),
            KeyCode::Char('p') | KeyCode::Char('N') => self.jump_file(false),
            KeyCode::Char('d') if ctrl => {
                self.diff_scroll = self.diff_scroll.saturating_add(HALF_PAGE)
            }
            KeyCode::Char('u') if ctrl => {
                self.diff_scroll = self.diff_scroll.saturating_sub(HALF_PAGE)
            }
            KeyCode::Char('g') => self.diff_scroll = 0,
            KeyCode::Char('G') => self.diff_scroll = u16::MAX, // clamped on draw
            _ => {}
        }
        Ok(())
    }
}

fn diff_pane_width(total: u16) -> u16 {
    total.saturating_sub(TREE_WIDTH).max(MIN_DIFF_WIDTH)
}

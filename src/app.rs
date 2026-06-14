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

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        let next = (self.selected_index() as isize + delta).clamp(0, max) as usize;
        if next != self.selected_index() {
            self.tree_state.select(Some(next));
            self.diff_scroll = 0;
        }
    }

    fn handle_event(&mut self) -> Result<()> {
        let Event::Key(key) = event::read()? else {
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

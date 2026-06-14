use std::collections::HashSet;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::DefaultTerminal;
use ratatui::widgets::ListState;

use crate::delta::RenderCache;
use crate::diff::FileDiff;
use crate::icons::IconStyle;
use crate::tree::{self, Node, Row, RowKind};

/// Total width of the file-tree pane, including its 1-column right border.
pub const TREE_WIDTH: u16 = 32;
const MIN_DIFF_WIDTH: u16 = 20;
const HALF_PAGE: i32 = 15;

/// Which pane the j/k keys act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Diff,
}

/// State of the fuzzy file-finder overlay.
pub struct Finder {
    pub query: String,
    /// File indices that match `query`, best first.
    pub matches: Vec<usize>,
    /// Index into `matches` of the highlighted row.
    pub selected: usize,
}

pub struct App {
    pub files: Vec<FileDiff>,
    pub rows: Vec<Row>,
    pub tree_state: ListState,
    pub diff_scroll: u16,
    pub side_by_side: bool,
    pub show_tree: bool,
    pub focus: Focus,
    pub show_help: bool,
    pub status: Option<String>,
    pub icon_style: IconStyle,
    pub finder: Option<Finder>,
    pub cache: RenderCache,
    matcher: SkimMatcherV2,
    nodes: Vec<Node>,
    collapsed: HashSet<String>,
    last_width: u16,
    quit: bool,
    pending_editor: Option<String>,
}

impl App {
    pub fn new(files: Vec<FileDiff>, side_by_side: bool, config_sbs: bool) -> Self {
        let nodes = tree::build(&files);
        let collapsed = HashSet::new();
        let rows = tree::flatten(&nodes, &collapsed);
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
            show_tree: true,
            focus: Focus::Tree,
            show_help: false,
            status: None,
            icon_style: IconStyle::Nerd,
            finder: None,
            cache: RenderCache::new(config_sbs),
            matcher: SkimMatcherV2::default(),
            nodes,
            collapsed,
            last_width: 0,
            quit: false,
            pending_editor: None,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let mut terminal = ratatui::init();
        let result = self.event_loop(&mut terminal);
        ratatui::restore();
        result
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            let diff_width = self.diff_pane_width(terminal.size()?.width);

            // On resize, drop renders made at the old width so the cache stays
            // bounded and the current file re-renders to the new width.
            if diff_width != self.last_width {
                self.cache.clear();
                self.last_width = diff_width;
            }

            if let Some(idx) = self.selected_file() {
                let raw = &self.files[idx].raw;
                self.cache.ensure(idx, raw, diff_width, self.side_by_side)?;
            }

            terminal.draw(|frame| crate::ui::draw(frame, self, diff_width))?;
            self.handle_event()?;

            // Suspending the TUI to run an editor needs the owned terminal.
            if let Some(path) = self.pending_editor.take() {
                self.open_editor(terminal, &path)?;
            }
        }
        Ok(())
    }

    fn diff_pane_width(&self, total: u16) -> u16 {
        let used = if self.show_tree { TREE_WIDTH } else { 0 };
        total.saturating_sub(used).max(MIN_DIFF_WIDTH)
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

    fn scroll_diff(&mut self, delta: i32) {
        self.diff_scroll = (self.diff_scroll as i32 + delta).max(0) as u16;
    }

    /// Expand/collapse the selected directory and re-flatten the visible rows.
    fn toggle_fold(&mut self) {
        let path = match self.rows.get(self.selected_index()) {
            Some(Row { kind: RowKind::Dir { path, .. }, .. }) => path.clone(),
            _ => return,
        };
        if !self.collapsed.remove(&path) {
            self.collapsed.insert(path);
        }
        let sel = self.selected_index();
        self.rows = tree::flatten(&self.nodes, &self.collapsed);
        self.tree_state
            .select(Some(sel.min(self.rows.len().saturating_sub(1))));
    }

    fn open_finder(&mut self) {
        self.finder = Some(Finder {
            query: String::new(),
            matches: (0..self.files.len()).collect(),
            selected: 0,
        });
    }

    /// Recompute finder matches after the query changes.
    fn finder_recompute(&mut self) {
        let query = match &self.finder {
            Some(f) => f.query.clone(),
            None => return,
        };
        let matches: Vec<usize> = if query.is_empty() {
            (0..self.files.len()).collect()
        } else {
            let mut scored: Vec<(i64, usize)> = self
                .files
                .iter()
                .enumerate()
                .filter_map(|(i, f)| self.matcher.fuzzy_match(f.path(), &query).map(|s| (s, i)))
                .collect();
            scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
            scored.into_iter().map(|(_, i)| i).collect()
        };
        if let Some(f) = self.finder.as_mut() {
            f.selected = f.selected.min(matches.len().saturating_sub(1));
            f.matches = matches;
        }
    }

    /// Select a file by diff index, expanding any collapsed ancestor folders so
    /// its row is visible. Used when jumping from the finder.
    fn reveal_file(&mut self, diff_index: usize) {
        let path = self.files[diff_index].path().to_string();
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let mut acc = String::new();
        for part in &parts[..parts.len().saturating_sub(1)] {
            acc = if acc.is_empty() { part.to_string() } else { format!("{acc}/{part}") };
            self.collapsed.remove(&acc);
        }
        self.rows = tree::flatten(&self.nodes, &self.collapsed);
        if let Some(i) = self
            .rows
            .iter()
            .position(|r| matches!(r.kind, RowKind::File { diff_index: d } if d == diff_index))
        {
            self.tree_state.select(Some(i));
            self.diff_scroll = 0;
        }
    }

    fn copy_path(&mut self) {
        let Some(idx) = self.selected_file() else {
            self.status = Some("No file selected to copy".into());
            return;
        };
        let path = self.files[idx].path().to_string();
        self.status = Some(
            match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(path.clone())) {
                Ok(()) => format!("Copied {path}"),
                Err(e) => format!("Copy failed: {e}"),
            },
        );
    }

    /// Suspend the TUI, run `$VISUAL`/`$EDITOR` on `path`, then resume.
    fn open_editor(&mut self, terminal: &mut DefaultTerminal, path: &str) -> Result<()> {
        ratatui::restore();
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        let status = Command::new(&editor).arg(path).status();

        *terminal = ratatui::init();
        let _ = terminal.clear();
        self.last_width = 0; // force a re-render into the fresh screen

        self.status = Some(match status {
            Ok(s) if s.success() => format!("Edited {path}"),
            Ok(s) => format!("{editor} exited: {s}"),
            Err(e) => format!("Couldn't launch {editor}: {e}"),
        });
        Ok(())
    }

    fn handle_event(&mut self) -> Result<()> {
        let mut ev = event::read()?;
        // Coalesce a burst of resize events (e.g. a drag) into the last one.
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

        self.status = None;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => {
                if self.focus == Focus::Tree {
                    self.move_selection(1)
                } else {
                    self.scroll_diff(1)
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.focus == Focus::Tree {
                    self.move_selection(-1)
                } else {
                    self.scroll_diff(-1)
                }
            }
            KeyCode::Char('n') => self.jump_file(true),
            KeyCode::Char('p') | KeyCode::Char('N') => self.jump_file(false),
            KeyCode::Char('d') if ctrl => self.scroll_diff(HALF_PAGE),
            KeyCode::Char('u') if ctrl => self.scroll_diff(-HALF_PAGE),
            KeyCode::Char('g') => self.diff_scroll = 0,
            KeyCode::Char('G') => self.diff_scroll = u16::MAX, // clamped on draw
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_fold(),
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Tree => Focus::Diff,
                    Focus::Diff => Focus::Tree,
                }
            }
            KeyCode::Char('s') => self.side_by_side = !self.side_by_side,
            KeyCode::Char('e') => {
                self.show_tree = !self.show_tree;
                if !self.show_tree {
                    self.focus = Focus::Diff;
                }
            }
            KeyCode::Char('t') | KeyCode::Char('/') => self.open_finder(),
            KeyCode::Char('i') => {
                self.icon_style = self.icon_style.next();
                self.status = Some(format!("Icons: {}", self.icon_style.name()));
            }
            KeyCode::Char('y') => self.copy_path(),
            KeyCode::Char('o') => {
                if let Some(idx) = self.selected_file() {
                    self.pending_editor = Some(self.files[idx].path().to_string());
                }
            }
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::FileStatus;

    fn file(path: &str) -> FileDiff {
        FileDiff {
            old_path: None,
            new_path: Some(path.to_string()),
            status: FileStatus::Modified,
            additions: 0,
            deletions: 0,
            raw: String::new(),
        }
    }

    #[test]
    fn finder_empty_query_lists_all_files() {
        let mut app = App::new(vec![file("a.rs"), file("b.rs")], false, false);
        app.open_finder();
        app.finder_recompute();
        assert_eq!(app.finder.as_ref().unwrap().matches.len(), 2);
    }

    #[test]
    fn finder_ranks_best_match_first() {
        let files = vec![file("src/main.rs"), file("src/diff/parser.rs"), file("README.md")];
        let mut app = App::new(files, false, false);
        app.open_finder();
        for c in "parser".chars() {
            app.finder.as_mut().unwrap().query.push(c);
        }
        app.finder_recompute();
        let best = app.finder.as_ref().unwrap().matches[0];
        assert_eq!(app.files[best].path(), "src/diff/parser.rs");
    }

    #[test]
    fn reveal_file_expands_collapsed_ancestors() {
        let files = vec![file("src/diff/parser.rs")];
        let mut app = App::new(files, false, false);
        app.collapsed.insert("src".to_string());
        app.collapsed.insert("src/diff".to_string());
        app.rows = tree::flatten(&app.nodes, &app.collapsed);
        app.reveal_file(0);
        // The file's row is now visible and selected.
        assert!(matches!(
            app.rows[app.selected_index()].kind,
            RowKind::File { diff_index: 0 }
        ));
    }
}

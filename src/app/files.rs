//! The file tree: selection, folders, and the file finder.

use std::collections::HashSet;

use fuzzy_matcher::FuzzyMatcher;

use super::App;
use super::prefetch::By;
use crate::diff::FileStatus;
use crate::tree::{self, Row, RowKind};

/// State of the fuzzy file-finder overlay.
pub struct Finder {
    pub query: String,
    /// File indices that match `query`, best first.
    pub matches: Vec<usize>,
    /// Index into `matches` of the highlighted row.
    pub selected: usize,
}

impl App {
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

    /// The view mode actually used to render `idx`. Added files always render
    /// unified: side-by-side would just show an empty left pane and waste the
    /// scarce horizontal space, so they ignore the global toggle.
    pub fn side_by_side_for(&self, idx: usize) -> bool {
        self.side_by_side && self.files[idx].status != FileStatus::Added
    }

    pub fn totals(&self) -> (u32, u32) {
        self.files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions))
    }

    pub(super) fn select(&mut self, index: usize) {
        if index != self.selected_index() {
            self.tree_state.select(Some(index));
            self.diff_scroll = 0;
            self.reset_cursor();
        }
    }

    /// Send the line cursor back to the top. Called whenever the diff pane starts
    /// showing a different file, where a carried-over line index would be
    /// meaningless and the remembered anchor belongs to the old file.
    pub(super) fn reset_cursor(&mut self) {
        self.diff_cursor = 0;
        self.cursor_anchor = None;
        self.cursor_token = None;
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.head(delta > 0, By::File);
        self.select((self.selected_index() as isize + delta).clamp(0, max) as usize);
    }

    /// Jump to the next/previous file row, skipping directories.
    pub(super) fn jump_file(&mut self, forward: bool) {
        self.head(forward, By::File);
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

    /// Diff indices in tree order with nothing folded, so a file inside a collapsed
    /// folder is still reachable; `reveal_file` opens the folder on the way in.
    pub(super) fn tree_order(&self) -> Vec<usize> {
        tree::flatten(&self.nodes, &HashSet::new())
            .iter()
            .filter_map(|row| match row.kind {
                RowKind::File { diff_index } => Some(diff_index),
                RowKind::Dir { .. } => None,
            })
            .collect()
    }

    /// Expand/collapse the selected directory and re-flatten the visible rows.
    pub(super) fn toggle_fold(&mut self) {
        let path = match self.rows.get(self.selected_index()) {
            Some(Row {
                kind: RowKind::Dir { path, .. },
                ..
            }) => path.clone(),
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

    pub(super) fn open_finder(&mut self) {
        self.finder = Some(Finder {
            query: String::new(),
            matches: (0..self.files.len()).collect(),
            selected: 0,
        });
    }

    /// Recompute finder matches after the query changes.
    pub(super) fn finder_recompute(&mut self) {
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
    pub(super) fn reveal_file(&mut self, diff_index: usize) {
        let path = self.files[diff_index].path().to_string();
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        let mut acc = String::new();
        for part in &parts[..parts.len().saturating_sub(1)] {
            acc = if acc.is_empty() {
                part.to_string()
            } else {
                format!("{acc}/{part}")
            };
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
            self.reset_cursor();
        }
    }

    pub(super) fn copy_path(&mut self) {
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
}

#[cfg(test)]
mod tests {

    use crate::app::App;
    use crate::app::test_support::{app_with, file};
    use crate::config::Config;
    use crate::diff::{FileDiff, FileStatus};
    use crate::tree::{self, RowKind};

    #[test]
    fn added_files_render_unified_even_in_side_by_side() {
        let added = FileDiff {
            status: FileStatus::Added,
            ..file("new.rs")
        };
        // side_by_side enabled globally; only the added file overrides it.
        let app = App::new(vec![file("mod.rs"), added], true, &Config::default());
        assert!(app.side_by_side_for(0), "modified file honors the toggle");
        assert!(!app.side_by_side_for(1), "added file forces unified");
    }

    #[test]
    fn finder_empty_query_lists_all_files() {
        let mut app = app_with(vec![file("a.rs"), file("b.rs")]);
        app.open_finder();
        app.finder_recompute();
        assert_eq!(app.finder.as_ref().unwrap().matches.len(), 2);
    }

    #[test]
    fn finder_ranks_best_match_first() {
        let files = vec![
            file("src/main.rs"),
            file("src/diff/parser.rs"),
            file("README.md"),
        ];
        let mut app = app_with(files);
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
        let mut app = app_with(files);
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

    #[test]
    fn open_depth_collapses_deep_folders() {
        // open_depth = 1: root dirs open, their subdirs collapsed.
        let cfg = Config {
            open_depth: 1,
            ..Config::default()
        };
        let app = App::new(vec![file("src/diff/parser.rs")], false, &cfg);
        assert!(!app.collapsed.contains("src"));
        assert!(app.collapsed.contains("src/diff"));
    }
}

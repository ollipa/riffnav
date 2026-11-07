//! Rendering the files the reader is likely to open next, before they do.
//!
//! The next file is the one the last step from file to file would reach if it
//! were taken again, which depends on the key: `n` goes on in the tree, `v` to the
//! next file not yet viewed, `n` in a search to the next file with a match, and so
//! on. The file `m` would jump to from the line under the cursor comes after it.
//! See [`RenderCache::prefetch`].

use super::{App, Focus};
use crate::render::{Jump, Prefetch, RenderCache};
use crate::theme::DiffTheme;
use crate::tree::{Row, RowKind};

/// Which way the reader is working through the files, which is the best guess at
/// which file is next.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Heading {
    pub(super) forward: bool,
    pub(super) by: By,
}

/// The kind of step the reader is taking from file to file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum By {
    /// To the next file in the tree: `n`, `p`, and moving in the tree.
    File,
    /// To the next file with a change: `J` and `K`.
    Change,
    /// To the next file not marked viewed: `v` and `V`.
    Unviewed,
    /// To the next file with a match: `n` and `N` during a search.
    Match,
    /// To the next file with comments: `]` and `[`.
    Comment,
}

/// What decides which files to prefetch and how they're to look.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct PrefetchAsked {
    selected: usize,
    rows: usize,
    heading: Heading,
    width: u16,
    side_by_side: bool,
    theme: DiffTheme,
    viewed: usize,
    comment_rev: u64,
    /// What the search has found so far.
    found: Option<(String, usize, bool)>,
    /// Where `m` would go.
    jump: Option<usize>,
}

impl App {
    /// With the file on screen done, get the one the reader is heading for ready,
    /// and then the one `m` would jump to.
    pub(super) fn prefetch_next(&mut self, width: u16) {
        if !self.view_settled() || !self.held_keys.is_empty() || self.cache.busy() {
            return;
        }
        let asked = PrefetchAsked {
            selected: self.selected_index(),
            rows: self.rows.len(),
            heading: self.heading,
            width,
            side_by_side: self.side_by_side,
            theme: self.diff_theme,
            viewed: self.viewed_count(),
            comment_rev: self.comment_rev,
            found: self.search_found(),
            jump: self.jump_ahead(),
        };
        let files = match &self.prefetch_asked {
            Some((was, files)) if *was == asked => files.clone(),
            _ => {
                let files = self.likely_next(asked.jump);
                self.prefetch_asked = Some((asked, files.clone()));
                files
            }
        };
        for file in files {
            if self.ask_cache(file, width, RenderCache::prefetch) != Prefetch::Ready {
                return;
            }
        }
    }

    /// The file the next press in the direction of travel would open, then
    /// `jump`, where `m` would go.
    fn likely_next(&self, jump: Option<usize>) -> Vec<usize> {
        let mut files: Vec<usize> = self.next_file().into_iter().collect();
        files.extend(jump.filter(|jump| !files.contains(jump)));
        files
    }

    /// The file the next press in the direction of travel would open.
    fn next_file(&self) -> Option<usize> {
        let Heading { forward, by } = self.heading;
        let here = self.selected_file();
        let in_order = |wanted: &dyn Fn(usize) -> bool| {
            let order = self.tree_order();
            let ranks = self.ranks_onward(forward, order.len());
            ranks
                .map(|(rank, _)| order[rank])
                .find(|&file| Some(file) != here && wanted(file))
        };
        let file = |row: &Row| match row.kind {
            RowKind::File { diff_index } => Some(diff_index),
            RowKind::Dir { .. } => None,
        };
        let at = self.selected_index();
        match by {
            By::Change => in_order(&|file| crate::diff::has_changes(&self.files[file])),
            By::Comment => in_order(&|file| self.comment_count(file) > 0),
            By::Unviewed => {
                // `v` and `V` go round, but only ever forward.
                let n = self.rows.len();
                let mut rows = (1..=n).map(|step| &self.rows[(at + step) % n]);
                rows.find_map(|row| file(row).filter(|&f| Some(f) != here && !self.is_viewed(f)))
            }
            By::Match if self.searching() => self.next_file_with_matches(forward),
            By::File | By::Match if forward => self.rows.iter().skip(at + 1).find_map(file),
            By::File | By::Match => self.rows.iter().take(at).rev().find_map(file),
        }
    }

    /// The file `m` would go to from here, if it's another one.
    fn jump_ahead(&self) -> Option<usize> {
        if self.focus != Focus::Diff {
            return None;
        }
        let here = self.selected_file()?;
        let back = self
            .copy_return
            .filter(|(_, landed)| self.on_copy_landing(*landed));
        let file = match back {
            Some((from, _)) => from.file,
            None => match self.jump_target()?.jump {
                Jump::Moved(target) | Jump::Copied(target) => target.file,
                Jump::Elsewhere(_) => return None,
            },
        };
        (file != here).then_some(file)
    }

    /// Note the way the reader is going, for [`App::likely_next`].
    pub(super) fn head(&mut self, forward: bool, by: By) {
        self.heading = Heading { forward, by };
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode::{self, Char, Enter};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::comment::{Anchor, Comment, CommentStore, Side};
    use crate::config::Config;
    use crate::diff::parse;
    use crate::diff::testing::hunk;
    use crate::render::Target;

    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 16;

    /// Files named by `names`, each with one change, which `matching` also have
    /// a line `zzz` in.
    fn diff(names: &[&str], matching: &[&str]) -> String {
        let body = |name: &&str| match matching.contains(name) {
            true => " zzz\n-y\n+z\n",
            false => " x\n-y\n+z\n",
        };
        names.iter().map(|name| hunk(name, body(name))).collect()
    }

    /// An app that prefetches, as tests otherwise leave that off.
    fn app(raw: &str) -> App {
        let mut app = App::new(parse(raw), false, &Config::default());
        app.install_comments_for_test(CommentStore::disabled());
        app.focus = Focus::Diff;
        app.cache.prefetch_for_test();
        frame(&mut app);
        app
    }

    /// A pass of the event loop.
    fn frame(app: &mut App) {
        let diff_width = app.diff_pane_width(WIDTH);
        app.prepare_frame(diff_width);
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
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

    /// Passes of the event loop until there's nothing left to prefetch, and the
    /// files rendered since the last call, by name, in the order they started.
    fn started(app: &mut App) -> Vec<String> {
        for _ in 0..100 {
            app.cache.finish();
            frame(app);
            if !app.cache.busy() {
                let started = std::mem::take(&mut *app.cache.hooks().started.lock().unwrap());
                let name = |file: usize| app.files[file].path().to_string();
                return started.into_iter().map(name).collect();
            }
        }
        panic!("prefetching never stopped");
    }

    fn note(path: &str) -> Comment {
        Comment {
            id: path.to_string(),
            file: path.to_string(),
            side: Side::New,
            line: 2,
            body: "a note".to_string(),
            author: "tester".to_string(),
            created: crate::state::now_unix(),
            reply_to: None,
            diff_hash: None,
        }
    }

    #[test]
    fn the_next_file_is_prefetched_in_the_direction_of_travel() {
        let mut app = app(&diff(&["a", "b", "c", "d", "e", "f"], &[]));
        assert_eq!(started(&mut app), ["a", "b"]);

        press(&mut app, Char('n'));
        press(&mut app, Char('n'));
        press(&mut app, Char('n'));
        press(&mut app, Char('p'));
        assert_eq!(file_name(&app).as_deref(), Some("c"));
        started(&mut app);
        app.cache.clear();
        assert_eq!(started(&mut app), ["c", "b"], "going backwards");
    }

    #[test]
    fn a_change_jump_prefetches_the_files_with_a_change() {
        let raw = format!(
            "{}{}{}",
            hunk("a", " x\n-y\n z\n-w\n"),
            hunk("b", " x\n y\n"),
            hunk("c", " x\n-y\n"),
        );
        let mut app = app(&raw);
        started(&mut app);
        app.cache.clear();
        press(&mut app, Char('J'));
        assert_eq!(
            started(&mut app),
            ["a", "c"],
            "not b, which has nothing to stop at"
        );
    }

    #[test]
    fn marking_a_file_viewed_prefetches_the_next_one_not_viewed() {
        let mut app = app(&diff(&["a", "b", "c", "d", "e"], &[]));
        started(&mut app);
        app.cache.clear();
        app.review.toggle(app.file_hashes[2], "c");
        press(&mut app, Char('v'));
        assert_eq!(file_name(&app).as_deref(), Some("b"));
        // The next `v` skips c, and a has just been viewed.
        assert_eq!(started(&mut app), ["b", "d"]);
    }

    #[test]
    fn jumping_to_the_next_file_not_viewed_prefetches_the_one_after() {
        let mut app = app(&diff(&["a", "b", "c", "d"], &[]));
        started(&mut app);
        app.cache.clear();
        app.review.toggle(app.file_hashes[1], "b");
        app.review.toggle(app.file_hashes[3], "d");
        press(&mut app, Char('V'));
        assert_eq!(file_name(&app).as_deref(), Some("c"));
        assert_eq!(started(&mut app), ["c", "a"], "round to a");
    }

    #[test]
    fn stepping_through_a_search_prefetches_the_next_file_with_a_match() {
        let mut app = app(&diff(&["a", "b", "c", "d", "e"], &["a", "c", "e"]));
        press(&mut app, Char('/'));
        for c in "zzz".chars() {
            press(&mut app, Char(c));
        }
        press(&mut app, Enter);
        started(&mut app);
        app.cache.clear();
        assert_eq!(started(&mut app), ["a", "c"]);

        press(&mut app, Char('n'));
        assert_eq!(file_name(&app).as_deref(), Some("c"));
        started(&mut app);
        app.cache.clear();
        assert_eq!(started(&mut app), ["c", "e"]);

        press(&mut app, Char('N'));
        assert_eq!(file_name(&app).as_deref(), Some("a"));
        started(&mut app);
        app.cache.clear();
        assert_eq!(
            started(&mut app),
            ["a", "e"],
            "back the other way, round the end"
        );
    }

    #[test]
    fn a_comment_jump_prefetches_the_next_file_with_comments() {
        let mut app = app(&diff(&["a", "b", "c", "d"], &[]));
        app.comments.add(note("a"));
        app.comments.add(note("c"));
        app.comment_rev += 1;
        frame(&mut app);
        press(&mut app, Char(']'));
        assert_eq!(file_name(&app).as_deref(), Some("a"));
        started(&mut app);
        app.cache.clear();
        assert_eq!(started(&mut app), ["a", "c"]);
    }

    #[test]
    fn the_other_end_of_a_move_under_the_cursor_comes_after_the_next_file() {
        let mut app = app(&diff(&["a", "b", "c", "d", "e", "f", "g"], &[]));
        let anchor = Anchor {
            side: Side::New,
            line: 2,
        };
        let target = Target { file: 5, anchor };
        app.cache.set_jump_for_test(0, anchor, Jump::Moved(target));
        let row = app.current_render().unwrap().line_map.row_for(anchor);
        app.set_cursor(row.unwrap());
        started(&mut app);
        app.cache.clear();
        assert_eq!(started(&mut app), ["a", "b", "f"]);
    }

    #[test]
    fn once_the_next_file_is_ready_nothing_more_happens() {
        let mut app = app(&diff(&["a", "b", "c", "d"], &[]));
        assert_eq!(started(&mut app), ["a", "b"]);
        assert_eq!(started(&mut app), Vec::<String>::new());
        assert_eq!(app.idle_timeout(), None, "no timer to wake up for");
    }
}

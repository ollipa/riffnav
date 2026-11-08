//! Viewed marks, and syncing them to the pull request.

use super::prefetch::By;
use super::{App, Direction, SYNC_FLUSH_GRACE};
use crate::autodiff::{DiffSource, GitDiff};
use crate::forge::ReviewSync;
use crate::review::ReviewStore;
use crate::tree::RowKind;

impl App {
    /// Arm one-way "viewed" sync to the branch's GitHub PR (the `review_sync_github`
    /// config key). Only takes effect when a GitHub forge was detected; otherwise
    /// it's a no-op and marks stay purely local. Call after [`App::enable_forge`].
    pub fn enable_review_sync(&mut self, enabled: bool) {
        if enabled && self.forge.is_some() {
            self.review_sync = Some(ReviewSync::new());
        }
    }

    /// Whether a viewed mark for the selected file should be pushed to GitHub:
    /// sync is armed AND we're in the branch-vs-base view (the only view that
    /// mirrors the PR diff). Every other view stays local — including `all vs
    /// base`, whose extra files are uncommitted ones the PR has never seen, and
    /// which GitHub would reject a mark for.
    fn syncs_viewed_marks(&self) -> bool {
        self.review_sync.is_some()
            && matches!(
                self.git.as_ref().and_then(GitDiff::plain_source),
                Some(DiffSource::VsBase)
            )
    }

    /// Load persistent "viewed" review state for the current repo+branch (and
    /// garbage-collect stale state). A no-op outside a git repo, where the store
    /// stays session-only. Call after `files` are in place.
    pub fn enable_review(&mut self, retention_days: u64) {
        self.review = ReviewStore::load(retention_days);
        // Carry over marks made before the hash stopped counting context. The
        // diff is at git's default context here, as it was when they were made.
        for (file, hash) in self.files.iter().zip(&self.file_hashes) {
            self.review
                .rekey(crate::review::legacy_file_hash(&file.raw), *hash);
        }
        self.review.save();
        // Resume on the first file still needing review, not the top of the list.
        self.select_first_unviewed();
    }

    /// Move the selection to the first unviewed file. When every file is viewed
    /// the initial first-file selection stands.
    fn select_first_unviewed(&mut self) {
        if let Some(i) = self.rows.iter().position(
            |r| matches!(r.kind, RowKind::File { diff_index } if !self.is_viewed(diff_index)),
        ) {
            self.select(i);
        }
    }

    pub fn is_viewed(&self, diff_index: usize) -> bool {
        self.file_hashes
            .get(diff_index)
            .is_some_and(|h| self.review.is_viewed(*h))
    }

    pub fn viewed_count(&self) -> usize {
        self.review.count_viewed(&self.file_hashes)
    }

    pub(super) fn toggle_viewed(&mut self) {
        let Some(idx) = self.selected_file() else {
            self.set_status("No file selected to mark viewed");
            return;
        };
        let path = self.files[idx].path().to_string();
        let now_viewed = self.review.toggle(self.file_hashes[idx], &path);
        self.review.save();
        let progress = format!("{}/{}", self.viewed_count(), self.files.len());
        self.queue_sync(&path, now_viewed);
        self.set_status(if now_viewed {
            format!("✓ Viewed {path}  ({progress})")
        } else {
            format!("Unviewed {path}  ({progress})")
        });
        // Flow to the next file to review — but only on marking, not unmarking,
        // and keep the status above so progress stays visible.
        if now_viewed
            && self.review_auto_advance
            && let Some(i) = self.next_unviewed_after(self.selected_index())
        {
            self.head(Direction::Forward, By::Unviewed);
            self.select(i);
        }
    }

    /// Queue a GitHub sync of `path`'s viewed mark when armed and in the PR view
    /// (see [`App::syncs_viewed_marks`]). The `gh` round trip runs on a background
    /// thread, so the mark's optimistic status stands until
    /// [`App::drain_review_sync`] reports a failure. The local mark always stands.
    fn queue_sync(&mut self, path: &str, viewed: bool) {
        if !self.syncs_viewed_marks() {
            return;
        }
        self.review_sync
            .as_mut()
            .expect("sync armed when syncs_viewed_marks is true")
            .enqueue(path, viewed);
    }

    /// Surface any GitHub sync that failed since the last tick.
    pub(super) fn drain_review_sync(&mut self) {
        let errors = match self.review_sync.as_mut() {
            Some(sync) => sync.drain(),
            None => return,
        };
        // The status line shows one message; the most recent failure is the
        // useful one (e.g. the same auth/PR error repeated across a burst).
        if let Some(msg) = errors.into_iter().next_back() {
            self.set_status(format!("GitHub sync failed: {msg}"));
        }
    }

    /// On shutdown, give queued GitHub syncs up to [`SYNC_FLUSH_GRACE`] to finish.
    pub(super) fn flush_review_sync(&mut self) {
        if let Some(sync) = self.review_sync.as_mut() {
            sync.flush(SYNC_FLUSH_GRACE);
        }
    }

    /// The next unviewed file row after `from`, wrapping around, or `None` when
    /// every file is viewed.
    fn next_unviewed_after(&self, from: usize) -> Option<usize> {
        let n = self.rows.len();
        if n == 0 {
            return None;
        }
        (1..=n).map(|off| (from + off) % n).find(|&i| {
            matches!(self.rows[i].kind, RowKind::File { diff_index } if !self.is_viewed(diff_index))
        })
    }

    /// Select the next unviewed file after the cursor, reporting when everything
    /// has been reviewed.
    pub(super) fn jump_unviewed(&mut self) {
        self.head(Direction::Forward, By::Unviewed);
        match self.next_unviewed_after(self.selected_index()) {
            Some(i) => self.select(i),
            None => self.set_status("All files reviewed ✓"),
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::app::test_support::{app_no_advance, app_with, file, file_with_raw};
    use crate::autodiff::{DiffSource, GitDiff};
    use crate::forge::ReviewSync;

    #[test]
    fn toggle_viewed_marks_only_selected_and_counts() {
        let mut app = app_no_advance(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        assert_eq!(app.viewed_count(), 0);

        // The first file is selected on launch; mark it viewed.
        let first = app.selected_file().unwrap();
        app.toggle_viewed();
        assert_eq!(app.viewed_count(), 1);
        assert!(app.is_viewed(first));
        // The other file is untouched — content-keyed, not position-keyed.
        assert!(!app.is_viewed(if first == 0 { 1 } else { 0 }));

        // Toggling again clears it (selection didn't move: auto-advance off).
        app.toggle_viewed();
        assert_eq!(app.viewed_count(), 0);
        assert!(!app.is_viewed(first));
    }

    #[test]
    fn syncs_viewed_marks_only_in_the_branch_vs_base_view_when_armed() {
        let mut app = app_with(vec![file("a.rs")]);
        let view = |source| GitDiff::diff(source, Some("origin/main".into()), vec![]);

        // Nothing armed and no git command (e.g. a piped diff): purely local.
        assert!(!app.syncs_viewed_marks());

        // Arm sync, but a working-tree view doesn't mirror the PR: still local.
        app.review_sync = Some(ReviewSync::new());
        app.enable_git_diff(view(DiffSource::AllUncommitted));
        assert!(!app.syncs_viewed_marks());

        // Branch-vs-base view with sync armed: this is the PR view, so it syncs.
        app.enable_git_diff(view(DiffSource::VsBase));
        assert!(app.syncs_viewed_marks());

        // The same view but with pass-through arguments is no longer guaranteed
        // to be the PR diff (a pathspec would narrow it), so it stays local.
        app.enable_git_diff(GitDiff::diff(
            DiffSource::VsBase,
            Some("origin/main".into()),
            vec!["--".into(), "src/".into()],
        ));
        assert!(!app.syncs_viewed_marks());

        // Armed but showing a piped diff: nothing to sync against.
        app.git = None;
        assert!(!app.syncs_viewed_marks());
    }

    #[test]
    fn jump_unviewed_skips_viewed_files() {
        let mut app = app_no_advance(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        // Mark the selected (first) file viewed, then jump: lands on the other.
        app.toggle_viewed();
        app.jump_unviewed();
        assert!(!app.is_viewed(app.selected_file().unwrap()));

        // With everything viewed, the selection holds where it is.
        app.toggle_viewed();
        let before = app.selected_index();
        app.jump_unviewed();
        assert_eq!(app.selected_index(), before);
    }

    #[test]
    fn marking_viewed_auto_advances_to_next_unviewed() {
        // Default config has auto-advance on.
        let mut app = app_with(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        let first = app.selected_file().unwrap();
        app.toggle_viewed();
        // Selection moved off the just-viewed file to the remaining unviewed one.
        let now = app.selected_file().unwrap();
        assert_ne!(now, first);
        assert!(!app.is_viewed(now));

        // Marking the last file leaves the selection put (nothing left to go to).
        app.toggle_viewed();
        let before = app.selected_index();
        app.toggle_viewed(); // unmarking never advances either
        assert_eq!(app.selected_index(), before);
    }

    #[test]
    fn startup_opens_on_first_unviewed_file() {
        let mut app = app_no_advance(vec![
            file_with_raw("a.rs"),
            file_with_raw("b.rs"),
            file_with_raw("c.rs"),
        ]);
        assert_eq!(app.selected_file(), Some(0));

        // Mark a.rs viewed (auto-advance off keeps the cursor put), then re-run
        // the startup selection: it skips the viewed file and lands on b.rs.
        app.toggle_viewed();
        app.select_first_unviewed();
        assert_eq!(
            app.selected_file().map(|i| app.files[i].path()),
            Some("b.rs")
        );
    }

    #[test]
    fn startup_holds_on_first_file_when_all_viewed() {
        let mut app = app_no_advance(vec![file_with_raw("a.rs"), file_with_raw("b.rs")]);
        app.toggle_viewed();
        app.jump_unviewed();
        app.toggle_viewed();

        // Back to the top, then run the startup selection: nothing is unviewed,
        // so it holds on the first file rather than jumping.
        app.tree_state.select(Some(0));
        app.select_first_unviewed();
        assert_eq!(app.selected_file(), Some(0));
    }
}

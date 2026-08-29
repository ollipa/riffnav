//! Noticing comments written by someone else.
//!
//! An agent leaving notes runs `riffnav comment add` in another terminal, which
//! writes the same scope file this window reads. A `notify` watcher on that file's
//! directory turns the write into a redraw, so notes appear without a keypress —
//! no daemon, no port, nothing for an agent sandbox to block.
//!
//! The *directory* is watched rather than the file itself: saves go through a
//! temp-file-and-rename (see [`crate::state::write_atomic`]), which replaces the
//! inode and would silently break a watch registered on the file.
//!
//! And the whole comments tree is watched rather than this scope's own
//! directory, for the same reason one level up: a scope directory comes and goes
//! under a running window — [`crate::state::sweep`] prunes an empty one, the
//! next write recreates it — and a watch on a directory that was removed is dead
//! for good, without saying so. The tree's root outlives all of that.

use std::path::Path;
use std::sync::mpsc::{Receiver, channel};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

pub struct CommentWatch {
    rx: Receiver<()>,
    /// Kept alive for the lifetime of the watch; dropping it stops watching.
    _watcher: RecommendedWatcher,
}

impl CommentWatch {
    /// Watch the comments tree `path` lives in. Returns `None` if the watch can't
    /// be established — comments still work, they just won't refresh until
    /// something else redraws.
    pub fn new(path: &Path) -> Option<Self> {
        // `<comments>/<repo>/<branch>.json`, so two levels up is the tree root.
        // Neither directory need exist yet, and a watch on a missing path fails,
        // so create the root up front — but not the scope directory, which is
        // then free to appear and vanish inside a watch that outlives it.
        let root = path.parent()?.parent()?;
        std::fs::create_dir_all(root).ok()?;

        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(ev) = res
                && matches!(
                    ev.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
            {
                let _ = tx.send(());
            }
        })
        .ok()?;
        // Recursive, so scope directories created later are watched too. The
        // cost is a reload when another repo's scope changes: one small JSON
        // read, against never noticing a note written to our own.
        watcher.watch(root, RecursiveMode::Recursive).ok()?;
        Some(Self {
            rx,
            _watcher: watcher,
        })
    }

    /// Whether anything changed since the last check, draining the whole backlog.
    /// One save emits several events (the temp write, then the rename), so they
    /// coalesce into a single reload.
    pub fn changed(&self) -> bool {
        let mut any = false;
        while self.rx.try_recv().is_ok() {
            any = true;
        }
        any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Wait for the watch to report a change, or give up. Filesystem events are
    /// delivered on the watcher's own thread, so a poll loop is the only way to
    /// wait on one.
    fn woke(watch: &CommentWatch) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if watch.changed() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// The scope directory a window watches is not permanent: it holds no file
    /// until the first comment lands, and any `riffnav comment` run sweeps an
    /// empty one away. A watch registered on that directory dies with it — for
    /// good, and silently — so the first note an agent wrote would never appear,
    /// which is what watching the tree's root instead is for.
    #[test]
    fn a_note_written_after_the_scope_directory_was_swept_still_wakes_the_watch() {
        let root = std::env::temp_dir().join(format!("riffnav-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("repo").join("branch.json");

        let watch = CommentWatch::new(&path).expect("the watch is established");
        std::thread::sleep(Duration::from_millis(200));

        // A `riffnav comment` run in another terminal sweeps the empty scope
        // directory, then writes the first note into a freshly made one.
        let _ = std::fs::remove_dir(path.parent().unwrap());
        assert!(crate::state::write_atomic(&path, b"{}"));
        assert!(woke(&watch), "the note must wake the watch");

        // And it keeps working for the writes after it.
        watch.changed(); // drain the tail of the first write
        assert!(crate::state::write_atomic(&path, b"{\"n\":1}"));
        assert!(woke(&watch), "and so must the next one");

        let _ = std::fs::remove_dir_all(&root);
    }
}

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

use std::path::Path;
use std::sync::mpsc::{Receiver, channel};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

pub struct CommentWatch {
    rx: Receiver<()>,
    /// Kept alive for the lifetime of the watch; dropping it stops watching.
    _watcher: RecommendedWatcher,
}

impl CommentWatch {
    /// Watch the directory holding `path`. Returns `None` if the watch can't be
    /// established — comments still work, they just won't refresh until something
    /// else redraws.
    pub fn new(path: &Path) -> Option<Self> {
        let dir = path.parent()?;
        // The scope directory doesn't exist until the first comment is saved, and
        // a watch on a missing path fails — so create it up front.
        std::fs::create_dir_all(dir).ok()?;

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
        watcher.watch(dir, RecursiveMode::NonRecursive).ok()?;
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

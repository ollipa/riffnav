//! Watch mode: re-run a diff-producing command when the working tree changes.
//!
//! In watch mode the diff is sourced from a command (default `git diff`) instead
//! of stdin, since stdin can only be read once. A `notify` watcher gives prompt
//! reaction to filesystem changes; a periodic interval is a safety net that also
//! catches changes the watcher can't see (e.g. staging via the git index). Both
//! funnel into one "re-run, reload only if the diff text actually changed" path.

use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Quiet period after the last filesystem event before reloading, so a burst of
/// writes (an editor save, a `git` operation) coalesces into one refresh.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Cap on how long the input poll blocks, so filesystem events are noticed
/// promptly even when the configured interval is long and no key is pressed.
const MAX_POLL: Duration = Duration::from_millis(250);

/// Run `cmd` via `sh -c` and return its stdout. Used for the initial diff load
/// and every watch refresh, so quoting/pipes in the command behave as a shell.
pub fn run_once(cmd: &str) -> Result<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .with_context(|| format!("running `{cmd}`"))?;
    if !out.status.success() {
        bail!("`{cmd}` exited with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub struct Watch {
    cmd: String,
    interval: Duration,
    rx: Receiver<()>,
    /// Kept alive for the lifetime of watch mode; dropping it stops watching.
    _watcher: RecommendedWatcher,
    last_diff: String,
    /// A filesystem change is pending; reload once it has been quiet for `DEBOUNCE`.
    dirty: bool,
    dirty_since: Instant,
    last_run: Instant,
}

impl Watch {
    /// Start watching the current directory. `initial_diff` is the diff already
    /// loaded at startup, so the first refresh only rebuilds on a real change.
    pub fn new(cmd: String, interval: Duration, initial_diff: String) -> Result<Self> {
        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(ev) = res
                && matches!(
                    ev.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
                && !is_noise(&ev)
            {
                let _ = tx.send(());
            }
        })
        .context("creating filesystem watcher")?;
        watcher
            .watch(Path::new("."), RecursiveMode::Recursive)
            .context("watching the current directory")?;

        let now = Instant::now();
        Ok(Self {
            cmd,
            interval,
            rx,
            _watcher: watcher,
            last_diff: initial_diff,
            dirty: false,
            dirty_since: now,
            last_run: now,
        })
    }

    /// How long the input poll may block before reload triggers are re-checked.
    pub fn poll_timeout(&self) -> Duration {
        if self.dirty {
            DEBOUNCE
        } else {
            self.interval
                .saturating_sub(self.last_run.elapsed())
                .clamp(Duration::from_millis(16), MAX_POLL)
        }
    }

    /// Drain pending filesystem events and, if a reload is due (debounced change
    /// or the periodic interval), re-run the command. Returns:
    /// - `Some(Ok(text))` when the diff changed and should be reloaded,
    /// - `Some(Err(_))` when the command failed,
    /// - `None` when nothing is due or the diff is unchanged.
    pub fn poll_reload(&mut self) -> Option<Result<String>> {
        let mut fs_changed = false;
        while self.rx.try_recv().is_ok() {
            fs_changed = true;
        }
        if fs_changed {
            self.dirty = true;
            self.dirty_since = Instant::now();
        }

        let now = Instant::now();
        let debounced = self.dirty && now.duration_since(self.dirty_since) >= DEBOUNCE;
        let periodic = now.duration_since(self.last_run) >= self.interval;
        if !debounced && !periodic {
            return None;
        }
        self.dirty = false;
        self.last_run = now;

        match run_once(&self.cmd) {
            Ok(text) if text == self.last_diff => None,
            Ok(text) => {
                self.last_diff = text.clone();
                Some(Ok(text))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

/// True when every path in the event is build/dependency churn that can never
/// affect the diff — avoids hammering the command during a `cargo`/`npm` build.
fn is_noise(ev: &Event) -> bool {
    ev.paths.iter().all(|p| {
        p.components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some("target" | "node_modules" | ".jj")
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_once_captures_stdout() {
        assert_eq!(run_once("printf 'hi there'").unwrap(), "hi there");
    }

    #[test]
    fn run_once_errors_on_nonzero_exit() {
        assert!(run_once("exit 3").is_err());
    }

    #[test]
    fn noise_filter_ignores_build_dirs_but_keeps_sources() {
        use std::path::PathBuf;
        let ev = |p: &str| Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![PathBuf::from(p)],
            attrs: Default::default(),
        };
        assert!(is_noise(&ev("target/debug/foo")));
        assert!(is_noise(&ev("node_modules/x/index.js")));
        assert!(!is_noise(&ev("src/main.rs")));
        assert!(!is_noise(&ev(".git/index")));
    }
}

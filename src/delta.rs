use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::Write;
use std::process::{Command, Stdio};

use ansi_to_tui::IntoText;
use anyhow::{Context, Result, bail};
use ratatui::text::Text;

/// Verify `delta` is callable, with an actionable error if it isn't.
pub fn ensure_available() -> Result<()> {
    match Command::new("delta").arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => bail!("`delta` failed to run: {}", String::from_utf8_lossy(&out.stderr)),
        Err(e) => bail!(
            "`delta` was not found on PATH ({e}).\n\
             riffnav renders diffs with delta — install it from https://github.com/dandavison/delta"
        ),
    }
}

/// Render one file's diff through delta and return its raw ANSI bytes.
///
/// delta emits color even when its stdout is a pipe, so no force-color flag is
/// needed; we only force truecolor and fix the wrap width to the diff pane.
fn run(diff_text: &str, width: u16, side_by_side: Option<bool>) -> Result<Vec<u8>> {
    let mut cmd = Command::new("delta");
    cmd.arg("--paging=never")
        .arg("--true-color=always")
        .arg("--width")
        .arg(width.to_string());
    if side_by_side == Some(true) {
        cmd.arg("--side-by-side");
    }

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn delta")?;

    // Write on a worker thread so a large diff can't deadlock against delta
    // filling its stdout pipe while we're still writing its stdin.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let owned = diff_text.to_owned();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(owned.as_bytes());
    });
    let output = child.wait_with_output().context("waiting on delta")?;
    let _ = writer.join();

    if !output.status.success() {
        bail!("delta exited with {}", output.status);
    }
    Ok(output.stdout)
}

/// A delta-rendered file, ready to drop into the diff viewport.
pub struct Rendered {
    pub text: Text<'static>,
    pub lines: u16,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    file: usize,
    width: u16,
    side_by_side: Option<bool>,
}

/// Caches delta renders keyed by `(file, width, side_by_side)` so revisiting a
/// file — or redrawing at the same size — never re-runs delta.
#[derive(Default)]
pub struct RenderCache {
    entries: HashMap<Key, Rendered>,
}

impl RenderCache {
    /// Render `raw` for the given key if not already cached.
    pub fn ensure(
        &mut self,
        file: usize,
        raw: &str,
        width: u16,
        side_by_side: Option<bool>,
    ) -> Result<()> {
        if let Entry::Vacant(slot) = self.entries.entry(Key { file, width, side_by_side }) {
            let bytes = run(raw, width, side_by_side)?;
            let text = bytes
                .into_text()
                .context("converting delta output to ratatui text")?;
            let lines = text.lines.len().min(u16::MAX as usize) as u16;
            slot.insert(Rendered { text, lines });
        }
        Ok(())
    }

    pub fn get(&self, file: usize, width: u16, side_by_side: Option<bool>) -> Option<&Rendered> {
        self.entries.get(&Key { file, width, side_by_side })
    }

    /// Drop all cached renders (e.g. after a resize changes the wrap width).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

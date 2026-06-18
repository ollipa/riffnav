use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::Write;
use std::process::{Command, Stdio};

use ansi_to_tui::IntoText;
use anyhow::{Context, Result, bail};
use ratatui::text::Text;
use unicode_width::UnicodeWidthChar;

use crate::theme::DiffTheme;

/// Verify `delta` is callable, with an actionable error if it isn't.
pub fn ensure_available() -> Result<()> {
    match Command::new("delta").arg("--version").output() {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => bail!(
            "`delta` failed to run: {}",
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) => bail!(
            "`delta` was not found on PATH ({e}).\n\
             riffnav renders diffs with delta — install it from https://github.com/dandavison/delta"
        ),
    }
}

/// Whether the user's gitconfig turns on `delta.side-by-side` by default. delta
/// 0.19 has no per-invocation flag to force this *off*, so riffnav needs to know
/// the default to decide how to render unified mode (see `run`).
pub fn detect_side_by_side() -> bool {
    Command::new("git")
        .args(["config", "--get", "delta.side-by-side"])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Render one file's diff through delta and return its raw ANSI bytes.
///
/// delta emits color even when its stdout is a pipe, so no force-color flag is
/// needed; we only force truecolor and fix the wrap width to the diff pane.
///
/// `--wrap-max-lines unlimited` overrides delta's default of 2: without it a
/// long line wraps at most twice and the rest is truncated with a `→` marker
/// (very visible in side-by-side mode on prose-heavy files like markdown). We
/// want every column preserved, wrapped onto as many rows as it needs.
///
/// `config_sbs` is the user's `delta.side-by-side` default. To render unified
/// when that default is on, we must pass `--no-gitconfig` (delta exposes no
/// `--side-by-side=false`); this keeps syntax highlighting but drops the user's
/// custom theme. When the default is already unified we pass nothing and the
/// theme is preserved.
///
/// A non-`Delta` `theme` takes full control of the colors: it always passes
/// `--no-gitconfig` (so the user's gitconfig styles can't fight ours) plus the
/// theme's explicit style flags.
fn run(
    diff_text: &str,
    width: u16,
    side_by_side: bool,
    config_sbs: bool,
    theme: DiffTheme,
) -> Result<Vec<u8>> {
    let mut cmd = Command::new("delta");
    cmd.arg("--paging=never")
        .arg("--true-color=always")
        .arg("--wrap-max-lines")
        .arg("unlimited")
        .arg("--width")
        .arg(width.to_string());
    if side_by_side {
        cmd.arg("--side-by-side");
    }
    if theme == DiffTheme::Delta {
        // Baseline: only force-disable gitconfig when we need to override an
        // `delta.side-by-side = true` default to render unified.
        if !side_by_side && config_sbs {
            cmd.arg("--no-gitconfig");
        }
    } else {
        // Themed: ignore gitconfig entirely and apply our own styles on top.
        cmd.arg("--no-gitconfig");
        cmd.args(theme.delta_args());
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

/// Reproduce delta's background-color-erase as literal spaces.
///
/// In unified mode delta extends a diff line's background to the right edge by
/// setting the line's background and then emitting `ESC[K` (erase to end of
/// line); a terminal fills the rest of the row with the active background. But
/// `ansi_to_tui` doesn't honor `ESC[K`, so that fill is dropped and only the
/// glyphs carry the background — the bug where unified mode shows the +/- tint
/// only behind the text. (Side-by-side mode pads with real spaces instead and
/// never emits `ESC[K`, so it's unaffected.)
///
/// This rewrites each erase-to-end `ESC[K` (empty or `0` parameter) into the
/// spaces it stands for — enough to reach `width` columns — which inherit
/// whatever background delta left active, exactly reproducing the terminal fill.
/// Lines already at or past `width` (delta leaves long lines unwrapped) get no
/// padding, so the downstream wrap is unaffected.
fn expand_bce(bytes: &[u8], width: u16) -> Vec<u8> {
    let width = width as usize;
    let mut out = Vec::with_capacity(bytes.len());
    let mut col = 0usize; // visible columns emitted on the current line
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // CSI sequence: ESC [ params(0x20..=0x3f) final(0x40..=0x7e).
        if b == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                j += 1;
            }
            let Some(&final_byte) = bytes.get(j) else {
                // Unterminated escape at EOF: copy the rest verbatim.
                out.extend_from_slice(&bytes[i..]);
                break;
            };
            if final_byte == b'K' && matches!(&bytes[i + 2..j], b"" | b"0") {
                // Erase to end of line → pad to the right edge with spaces that
                // carry the background delta set just before this sequence.
                let pad = width.saturating_sub(col);
                out.resize(out.len() + pad, b' ');
                col += pad;
            } else {
                // Any other CSI (SGR colors, other erases): copy as-is, no width.
                out.extend_from_slice(&bytes[i..=j]);
            }
            i = j + 1;
            continue;
        }
        if b == b'\n' {
            out.push(b);
            col = 0;
            i += 1;
            continue;
        }
        // A printable run: decode one UTF-8 char to advance the column count.
        let len = utf8_len(b);
        let end = (i + len).min(bytes.len());
        if let Some(ch) = std::str::from_utf8(&bytes[i..end])
            .ok()
            .and_then(|s| s.chars().next())
        {
            col += ch.width().unwrap_or(0);
        }
        out.extend_from_slice(&bytes[i..end]);
        i = end;
    }
    out
}

/// Byte length of a UTF-8 sequence from its leading byte (1 for a stray
/// continuation byte, so the scanner always makes progress).
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
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
    side_by_side: bool,
    theme: DiffTheme,
}

/// Caches delta renders keyed by `(file, width, side_by_side, theme)` so
/// revisiting a file — or redrawing at the same size and theme — never re-runs
/// delta. Switching themes re-renders (and caches separately), so toggling back
/// is instant. `config_sbs` is a session constant (the user's gitconfig
/// default), so it isn't part of the key.
pub struct RenderCache {
    entries: HashMap<Key, Rendered>,
    config_sbs: bool,
}

impl RenderCache {
    pub fn new(config_sbs: bool) -> Self {
        Self {
            entries: HashMap::new(),
            config_sbs,
        }
    }

    /// Render `raw` for the given key if not already cached.
    pub fn ensure(
        &mut self,
        file: usize,
        raw: &str,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
    ) -> Result<()> {
        let config_sbs = self.config_sbs;
        if let Entry::Vacant(slot) = self.entries.entry(Key {
            file,
            width,
            side_by_side,
            theme,
        }) {
            let mut bytes = run(raw, width, side_by_side, config_sbs, theme)?;
            // Unified mode relies on terminal background-color-erase to fill each
            // line's tint to the edge; ansi_to_tui ignores it, so do it ourselves.
            // Side-by-side already pads with real spaces and needs no fixup.
            if !side_by_side {
                bytes = expand_bce(&bytes, width);
            }
            let text = bytes
                .into_text()
                .context("converting delta output to ratatui text")?;
            let lines = text.lines.len().min(u16::MAX as usize) as u16;
            slot.insert(Rendered { text, lines });
        }
        Ok(())
    }

    pub fn get(
        &self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
    ) -> Option<&Rendered> {
        self.entries.get(&Key {
            file,
            width,
            side_by_side,
            theme,
        })
    }

    /// Drop all cached renders (e.g. after a resize changes the wrap width).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Seed a render directly, bypassing delta. Tests run without delta on PATH,
    /// so this is the only way to exercise the rendering path.
    #[cfg(test)]
    pub(crate) fn insert_for_test(
        &mut self,
        file: usize,
        width: u16,
        side_by_side: bool,
        theme: DiffTheme,
        text: Text<'static>,
    ) {
        let lines = text.lines.len().min(u16::MAX as usize) as u16;
        self.entries.insert(
            Key {
                file,
                width,
                side_by_side,
                theme,
            },
            Rendered { text, lines },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bce_pads_to_width_with_active_background() {
        // delta's shape: set bg, text, reset, re-set bg, ESC[K, reset.
        let input = b"\x1b[42mhi\x1b[0m\x1b[42m\x1b[K\x1b[0m";
        let out = expand_bce(input, 5);
        // "hi" is 2 cols, so the erase becomes 3 spaces inside the trailing bg.
        assert_eq!(out, b"\x1b[42mhi\x1b[0m\x1b[42m   \x1b[0m");
    }

    #[test]
    fn bce_counts_wide_chars() {
        // Two 2-col CJK glyphs occupy 4 columns, so only 1 space reaches width 5.
        let input = "\u{4e16}\u{754c}\x1b[K".as_bytes();
        let out = expand_bce(input, 5);
        assert_eq!(out, "\u{4e16}\u{754c} ".as_bytes());
    }

    #[test]
    fn bce_resets_column_each_line() {
        let input = b"ab\x1b[K\ncd\x1b[K";
        let out = expand_bce(input, 4);
        assert_eq!(out, b"ab  \ncd  ");
    }

    #[test]
    fn bce_skips_overlong_lines() {
        // Lines already at/past the width get no padding (delta leaves long
        // unified lines unwrapped for the downstream pager to wrap).
        let input = b"abcdef\x1b[K";
        let out = expand_bce(input, 4);
        assert_eq!(out, b"abcdef");
    }

    #[test]
    fn bce_leaves_non_erase_sequences_untouched() {
        let input = b"\x1b[1mbold\x1b[0m";
        assert_eq!(expand_bce(input, 10), input);
    }
}

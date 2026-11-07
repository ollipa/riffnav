//! riffnav's own diff renderer: git's hunks in, styled ratatui text out.
//!
//! Each row is built knowing its line numbers, so the [`LineMap`] comments anchor
//! to comes straight from the rows rather than being read back out of output.
//!
//! Rendering happens on [`jobs`], since a big file takes far longer than a key
//! can wait. [`RenderCache`] holds the results and is only ever touched by the UI
//! thread: a job works from its own copy of what it needs, and a result is only
//! taken if the job is still wanted, so one that a resize or a reload has
//! overtaken can't land.

mod blobs;
mod cache;
mod copies;
mod highlight;
mod jobs;
mod layout;
mod marks;
mod moves;
mod rendered;
mod spec;
mod starts;
mod tokens;
mod words;
mod wrap;

#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[cfg(test)]
use ratatui::text::Text;

pub use self::cache::{Key, Prefetch, RenderCache, Request};
pub use self::copies::Jump;
pub use self::highlight::warm_up;
#[cfg(test)]
use self::highlight::{HunkStart, Parsing};
#[cfg(test)]
use self::layout::Content;
pub use self::marks::Marks;
pub use self::moves::Target;
pub use self::rendered::Rendered;
pub use self::spec::CommentLayer;
#[cfg(test)]
use crate::comment::LineMap;
#[cfg(test)]
use crate::diff::{self, FileDiff};
#[cfg(test)]
use crate::theme::DiffTheme;

/// How long a frame waits for a render before going ahead without it. Long enough
/// that an ordinary file is drawn once and correctly, and short enough not to be
/// felt.
#[cfg(not(test))]
pub const WAIT: Duration = Duration::from_millis(25);
/// Long enough to mean "until it's done", so a test reads a frame's render straight
/// after it. The tests of a render that's late say so with [`RenderCache::wait`].
#[cfg(test)]
pub const WAIT: Duration = Duration::from_secs(60);

/// Lay a file out with nothing else going on, for the tests of the layout.
#[cfg(test)]
fn laid_out(
    file: &FileDiff,
    moves: &moves::FileMoves,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
    starts: &[HunkStart],
    side_by_side: bool,
) -> (Text<'static>, LineMap) {
    let cancelled = AtomicBool::new(false);
    let rows = diff::rows(file);
    let parsing = Parsing {
        starts: starts.to_vec(),
        ..Parsing::of(file, &rows)
    };
    let syntax = highlight::highlight(&rows, theme, &parsing, &cancelled);
    let changes = words::changes(&rows, &syntax.tokens, moves, &cancelled);
    let content = Content {
        rows: &rows,
        syntax: &syntax.runs,
        changes: &changes,
        moves,
        copies: &Default::default(),
        cancelled: &cancelled,
    };
    let lay_out = if side_by_side {
        layout::side_by_side
    } else {
        layout::unified
    };
    let (text, map) = lay_out(file, &content, width, theme, tab_width);
    (text, map.with_changes(diff::change_ids(&rows)))
}

/// Render one file as a unified diff, `width` columns wide.
#[cfg(test)]
pub fn unified(
    file: &FileDiff,
    moves: &moves::FileMoves,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
    starts: &[HunkStart],
) -> (Text<'static>, LineMap) {
    laid_out(file, moves, width, theme, tab_width, starts, false)
}

/// Render one file side by side, `width` columns wide.
#[cfg(test)]
pub fn side_by_side(
    file: &FileDiff,
    moves: &moves::FileMoves,
    width: u16,
    theme: DiffTheme,
    tab_width: u8,
    starts: &[HunkStart],
) -> (Text<'static>, LineMap) {
    laid_out(file, moves, width, theme, tab_width, starts, true)
}

#[cfg(test)]
pub(crate) mod test_support;

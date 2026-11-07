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
//!
//! [`LineMap`]: crate::comment::LineMap

mod blobs;
mod cache;
mod copies;
mod highlight;
mod jobs;
mod jump;
mod layout;
mod marks;
mod moves;
mod rendered;
mod spec;
mod starts;
mod tokens;
mod words;
mod wrap;

use std::time::Duration;

pub use self::cache::{Key, Prefetch, RenderCache, Request};
pub use self::highlight::warm_up;
pub use self::jump::{Jump, Target};
pub use self::marks::Marks;
pub use self::rendered::Rendered;
pub use self::spec::CommentLayer;

/// How long a frame waits for a render before going ahead without it. Long enough
/// that an ordinary file is drawn once and correctly, and short enough not to be
/// felt.
#[cfg(not(test))]
pub const WAIT: Duration = Duration::from_millis(25);
/// Long enough to mean "until it's done", so a test reads a frame's render straight
/// after it. The tests of a render that's late say so with [`RenderCache::wait`].
#[cfg(test)]
pub const WAIT: Duration = Duration::from_secs(60);

#[cfg(test)]
pub(crate) mod test_support;

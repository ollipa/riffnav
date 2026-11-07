//! Where `m` goes from a moved or copied line.

use crate::comment::Anchor;

/// The line at the other end of a move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub file: usize,
    pub anchor: Anchor,
}

/// The other end of a moved or copied line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Jump {
    Moved(Target),
    /// A line of the diff, on the far side of a copy.
    Copied(Target),
    /// A copy's source that the diff doesn't show, which is what's said instead.
    Elsewhere(String),
}

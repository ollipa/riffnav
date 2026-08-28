//! Inline review comments: notes anchored to a line of a file's diff, written
//! either by a human in the TUI or by an agent through `riffnav comment add`.
//!
//! - [`store`] owns the data model and its on-disk form.
//! - [`anchor`] maps delta's rendered rows back to diff line numbers.
//! - [`compose`] is the text field a note is typed into.
//! - [`render`] turns a thread of comments into the rows drawn beside the code.
//! - [`watch`] notices notes written by an agent in another terminal.

pub mod anchor;
pub mod compose;
pub mod render;
pub mod store;
pub mod watch;

pub use anchor::LineMap;
pub use compose::{Composer, PendingComment};
pub use render::CommentBlock;
pub use store::{Anchor, Comment, CommentStore, Side};
pub use watch::CommentWatch;

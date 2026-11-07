//! Inline review comments: notes anchored to a line of a file's diff, written
//! either by a human in the TUI or by an agent through `riffnav comment add`.
//!
//! - [`store`] owns the data model and its on-disk form.
//! - [`anchor`] maps rendered rows to diff line numbers.
//! - [`compose`] is the text field a note is typed into.
//! - [`render`] turns a thread of comments into the rows drawn beside the code.
//! - [`watch`] notices notes written by an agent in another terminal.
//! - [`reanchor`] moves notes with their code when the file is edited, using
//!   [`snapshot`]s of the file and its [`content`] now.

pub mod anchor;
pub mod compose;
pub mod content;
pub mod reanchor;
pub mod render;
pub mod snapshot;
pub mod store;
#[cfg(test)]
pub mod testing;
pub mod watch;

pub use anchor::LineMap;
pub use compose::{Composer, PendingComment};
pub use render::CommentBlock;
pub use store::{Anchor, Comment, CommentStore, Side};
pub use watch::CommentWatch;

mod model;
mod parser;
pub mod path;
mod rows;
#[cfg(test)]
pub mod testing;

pub use model::{FileDiff, FileStatus, Hunk};
pub use parser::{change_identity, hunks, parse};
pub use rows::{Row, RowKind, RowWalk, change_ids, change_runs, has_changes, notes, rows};

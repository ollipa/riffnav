mod model;
mod parser;
pub mod path;
mod rows;
#[cfg(test)]
pub mod testing;

pub use model::{FileDiff, FileStatus, Hunk};
pub use parser::{hunks, parse};
pub use rows::{Row, RowKind, RowWalk, change_ids, has_changes, notes, rows};

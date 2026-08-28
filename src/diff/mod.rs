mod model;
mod parser;

pub use model::{FileDiff, FileStatus, Hunk};
pub use parser::{hunks, parse};

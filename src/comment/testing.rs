//! Comments for tests to start from.

use super::{Comment, Side};

/// A comment by `tester` on new line `line` of `file`, made at time 0.
pub fn note(file: &str, line: u32, body: &str) -> Comment {
    Comment {
        id: String::new(),
        file: file.to_string(),
        side: Side::New,
        line,
        body: body.to_string(),
        author: "tester".to_string(),
        created: 0,
        reply_to: None,
        diff_hash: None,
        snapshot: None,
        code: None,
        outdated: false,
    }
}

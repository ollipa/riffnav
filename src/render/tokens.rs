//! What a line's syntax says about its tokens.
//!
//! The word pass compares lines by character class, which treats every mark of
//! punctuation as a token. The parser knows better: `==`, `1.5` and `println!` are
//! each one thing, and changing part of one changes all of it.

use std::ops::Range;
use std::sync::OnceLock;

use two_face::re_exports::syntect::easy::ScopeRangeIterator;
use two_face::re_exports::syntect::parsing::{Scope, ScopeStack, ScopeStackOp};

/// Byte ranges of one line, in order. Empty for a line that wasn't parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Tokens that change as a whole, among those [`split`] would cut up.
    pub whole: Vec<Range<usize>>,
    /// Strings, comments and prose, where spacing is content and not formatting.
    pub literal: Vec<Range<usize>>,
}

/// What a stretch of text with one scope stack is.
#[derive(Clone, Copy, PartialEq)]
enum Class {
    /// Code with no scope saying more, such as brackets and separators.
    Code,
    Literal,
    /// A number, which the parser cuts into pieces at its decimal point.
    Number,
    /// A single token, such as a keyword, operator or name.
    Whole,
}

/// Scopes ordered into what they make of the text inside them.
struct Scopes {
    literal: [Scope; 4],
    /// Code embedded in a literal, such as the `{name}` of an f-string. `source`
    /// is also the root scope of most files.
    code: [Scope; 3],
    number: Scope,
    whole: [Scope; 7],
}

fn scopes() -> &'static Scopes {
    static SCOPES: OnceLock<Scopes> = OnceLock::new();
    let scope = |name| Scope::new(name).expect("scope names here are valid");
    SCOPES.get_or_init(|| Scopes {
        literal: ["string", "comment", "markup", "text"].map(scope),
        code: ["source", "meta.embedded", "meta.interpolation"].map(scope),
        number: scope("constant.numeric"),
        whole: [
            "constant",
            "entity",
            "keyword",
            "punctuation.accessor",
            "storage",
            "support",
            "variable",
        ]
        .map(scope),
    })
}

/// The innermost scope with an opinion decides, so code embedded in a string is
/// code and a string embedded in that code is a string again.
fn classify(stack: &[Scope]) -> Class {
    let known = scopes();
    let within = |list: &[Scope], scope| list.iter().any(|s| s.is_prefix_of(scope));
    for &scope in stack.iter().rev() {
        if known.number.is_prefix_of(scope) {
            return Class::Number;
        }
        if within(&known.whole, scope) {
            return Class::Whole;
        }
        if within(&known.literal, scope) {
            return Class::Literal;
        }
        if within(&known.code, scope) {
            return Class::Code;
        }
    }
    Class::Code
}

/// What the parser's output says about `text`: `ops` are the scope changes over
/// `line`, which is `text` with a newline, starting from the scopes `stack`.
pub fn parsed(
    text: &str,
    line: &str,
    mut stack: ScopeStack,
    ops: &[(usize, ScopeStackOp)],
) -> Tokens {
    let mut out = Tokens::default();
    let mut previous = Class::Code;
    for (range, op) in ScopeRangeIterator::new(ops, line) {
        if stack.apply(op).is_err() {
            return Tokens::default();
        }
        let range = range.start.min(text.len())..range.end.min(text.len());
        if range.is_empty() {
            continue;
        }
        let class = classify(stack.as_slice());
        match class {
            Class::Number if previous == Class::Number => extend(&mut out.whole, range),
            Class::Number => out.whole.push(range),
            Class::Whole if is_token(&text[range.clone()]) => out.whole.push(range),
            Class::Literal => extend(&mut out.literal, range),
            Class::Whole | Class::Code => {}
        }
        previous = class;
    }
    out
}

/// Add `range` to `ranges`, as part of the last one if it continues it.
fn extend(ranges: &mut Vec<Range<usize>>, range: Range<usize>) {
    match ranges.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => ranges.push(range),
    }
}

/// Whether text in a token's scope really is one token: an operator, or a name
/// with a sigil or hyphen. Some syntaxes give a whole path or argument one scope,
/// and marking all of that for a change to one part of it would mark too much.
fn is_token(text: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    if text.contains(char::is_whitespace) {
        false
    } else if text.contains(word) {
        text.chars().all(|c| word(c) || "-!?$@#'\\".contains(c))
    } else {
        true
    }
}

/// Split into words, whitespace runs, and single punctuation characters, as byte
/// ranges. `foo_bar` is one token, `foo.bar` three.
pub fn split(text: &str) -> Vec<Range<usize>> {
    #[derive(PartialEq)]
    enum Kind {
        Word,
        Blank,
        Mark,
    }
    let kind = |c: char| match c {
        c if c.is_alphanumeric() || c == '_' => Kind::Word,
        c if c.is_whitespace() => Kind::Blank,
        _ => Kind::Mark,
    };

    let mut out: Vec<Range<usize>> = Vec::new();
    let mut last = None;
    for (at, c) in text.char_indices() {
        let current = kind(c);
        match out.last_mut() {
            Some(token) if last.as_ref() == Some(&current) && current != Kind::Mark => {
                token.end = at + c.len_utf8();
            }
            _ => out.push(at..at + c.len_utf8()),
        }
        last = Some(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{parse, rows};
    use crate::render::highlight::highlight;
    use crate::theme::DiffTheme;

    /// What the parser finds in `line`, added to a file named `name`.
    fn tokens_of(name: &str, line: &str) -> (Vec<String>, Vec<String>) {
        let raw = format!(
            "diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n@@ -0,0 +1 @@\n+{line}\n"
        );
        let file = parse(&raw).remove(0);
        let rows = rows(&file);
        let tokens = highlight(&file, &rows, DiffTheme::GitHubDark, &[])
            .tokens
            .remove(1);
        let text =
            |ranges: Vec<Range<usize>>| ranges.into_iter().map(|r| line[r].to_string()).collect();
        (text(tokens.whole), text(tokens.literal))
    }

    #[test]
    fn operators_and_decimals_are_whole() {
        let (whole, _) = tokens_of("a.rs", "a == 1.5");
        assert!(whole.contains(&"==".to_string()));
        assert!(whole.contains(&"1.5".to_string()));
    }

    #[test]
    fn strings_and_comments_are_literal() {
        let (_, literal) = tokens_of("a.rs", "let a = \"x y\"; // note");
        assert_eq!(literal, ["\"x y\"", "// note"]);
    }

    #[test]
    fn code_inside_a_string_is_not_literal() {
        let (_, literal) = tokens_of("a.py", "x = f\"a {b} c\"");
        assert!(literal.iter().all(|piece| !piece.contains('b')));
    }

    #[test]
    fn a_line_not_in_a_language_says_nothing() {
        assert_eq!(tokens_of("notes.xyz", "a == 1.5"), (vec![], vec![]));
    }

    #[test]
    fn only_operators_and_sigiled_names_are_tokens() {
        assert!(is_token("=="));
        assert!(is_token("println!"));
        assert!(is_token("font-size"));
        assert!(is_token("--flag"));
        assert!(!is_token("tests/run.py"));
        assert!(!is_token("[--author"));
        assert!(!is_token("two words"));
    }

    #[test]
    fn split_keeps_words_and_blanks_together_and_marks_apart() {
        let text = "foo_bar  .(";
        let got: Vec<&str> = split(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(got, ["foo_bar", "  ", ".", "("]);
    }
}

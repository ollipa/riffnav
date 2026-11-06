//! Syntax highlighting of a file's diff rows.
//!
//! The two sides of a diff are different programs, so each gets its own parser
//! state: feeding removed and added lines through one parser would let a string
//! opened on a removed line swallow the added lines after it. Each hunk starts
//! afresh, since the lines between hunks aren't in the diff.

use std::path::Path;
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use two_face::re_exports::syntect::highlighting::{
    self, FontStyle, HighlightIterator, HighlightState, Highlighter,
};
use two_face::re_exports::syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

use super::tokens::{self, Tokens};
use crate::diff::{FileDiff, Row, RowKind};
use crate::theme::DiffTheme;

/// Lines longer than this skip highlighting. Rendering blocks the UI, and a
/// minified bundle is one enormous line that would stall it for no useful color.
pub const MAX_LINE_BYTES: usize = 2_000;
/// Diffs bigger than this skip highlighting altogether, for the same reason: a
/// generated file or lockfile would otherwise freeze the UI on selection. Word
/// emphasis still applies, and it's what matters most on a file that big.
const MAX_FILE_BYTES: usize = 500_000;

/// A stretch of a row's text in one style, up to byte offset `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub end: usize,
    pub style: Style,
}

struct Assets {
    syntaxes: SyntaxSet,
    themes: EmbeddedLazyThemeSet,
}

/// Decoding the bundled syntaxes takes real time, so it happens on first use and
/// only once.
fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| Assets {
        syntaxes: two_face::syntax::extra_newlines(),
        themes: two_face::theme::extra(),
    })
}

/// What parsing a file's rows found, index-aligned with them.
#[derive(Debug)]
pub struct Syntax {
    /// Colored runs. Empty for a row with no syntax color: not code, too long,
    /// or a file too big or of no known language.
    pub runs: Vec<Vec<Run>>,
    /// What the parser found in each changed row.
    pub tokens: Vec<Tokens>,
}

/// One side of the diff's parser state.
struct Side<'a> {
    parser: ParseState,
    state: HighlightState,
    highlighter: &'a Highlighter<'a>,
}

impl<'a> Side<'a> {
    fn new(syntax: &SyntaxReference, highlighter: &'a Highlighter<'a>) -> Self {
        Self {
            parser: ParseState::new(syntax),
            state: HighlightState::new(highlighter, ScopeStack::new()),
            highlighter,
        }
    }

    fn line(&mut self, set: &SyntaxSet, text: &str) -> Option<(Vec<Run>, Tokens)> {
        // The newline-aware syntaxes need each line terminated to match correctly.
        let line = format!("{text}\n");
        let ops = self.parser.parse_line(&line, set).ok()?;
        let tokens = tokens::parsed(text, &line, self.state.path.clone(), &ops);
        let regions = HighlightIterator::new(&mut self.state, &ops, &line, self.highlighter);
        let mut end = 0;
        let runs = regions
            .map(|(style, piece)| {
                end = (end + piece.len()).min(text.len());
                Run {
                    end,
                    style: to_style(style),
                }
            })
            .collect();
        Some((runs, tokens))
    }
}

/// Parse `rows`, each side of the diff on its own.
pub fn highlight(file: &FileDiff, rows: &[Row], theme: DiffTheme) -> Syntax {
    let mut out = Syntax {
        runs: vec![Vec::new(); rows.len()],
        tokens: vec![Tokens::default(); rows.len()],
    };
    if file.raw.len() > MAX_FILE_BYTES {
        return out;
    }
    let assets = assets();
    let Some(syntax) = find_syntax(&assets.syntaxes, file.path(), rows) else {
        return out;
    };
    let set = &assets.syntaxes;
    let highlighter = Highlighter::new(assets.themes.get(syntax_theme(theme)));

    let mut old = Side::new(syntax, &highlighter);
    let mut new = Side::new(syntax, &highlighter);
    for (at, row) in rows.iter().enumerate() {
        if row.text.len() > MAX_LINE_BYTES {
            continue;
        }
        let parsed = match row.kind {
            RowKind::HunkHeader { .. } => {
                old = Side::new(syntax, &highlighter);
                new = Side::new(syntax, &highlighter);
                None
            }
            RowKind::Removed => old.line(set, &row.text),
            RowKind::Added => new.line(set, &row.text),
            // Context is on both sides, so both parsers have to see it. The two
            // agree unless the change above altered how it parses, and then the
            // post-image is the one that's true of the code now.
            RowKind::Context => {
                old.line(set, &row.text);
                new.line(set, &row.text)
            }
            RowKind::NoNewline => None,
        };
        if let Some((runs, tokens)) = parsed {
            out.runs[at] = runs;
            if matches!(row.kind, RowKind::Removed | RowKind::Added) {
                out.tokens[at] = tokens;
            }
        }
    }
    out
}

/// The syntax theme painting a diff theme's code.
fn syntax_theme(theme: DiffTheme) -> EmbeddedThemeName {
    match theme {
        DiffTheme::GitHubLight => EmbeddedThemeName::Github,
        DiffTheme::GitHubDark => EmbeddedThemeName::OneHalfDark,
    }
}

/// The file's language from its name, then its extension, then a `#!` first
/// line. `None` for anything else, including plain text, which has nothing to
/// color and isn't worth parsing.
fn find_syntax<'a>(set: &'a SyntaxSet, path: &str, rows: &[Row]) -> Option<&'a SyntaxReference> {
    let path = Path::new(path);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    set.find_syntax_by_extension(name)
        .or_else(|| set.find_syntax_by_extension(extension))
        .or_else(|| {
            let first = rows.iter().find(|r| r.old == Some(1) || r.new == Some(1))?;
            set.find_syntax_by_first_line(&first.text)
        })
        .filter(|syntax| syntax.name != "Plain Text")
}

fn to_style(style: highlighting::Style) -> Style {
    let color = style.foreground;
    let mut out = Style::new().fg(Color::Rgb(color.r, color.g, color.b));
    for (flag, modifier) in [
        (FontStyle::BOLD, Modifier::BOLD),
        (FontStyle::ITALIC, Modifier::ITALIC),
        (FontStyle::UNDERLINE, Modifier::UNDERLINED),
    ] {
        if style.font_style.contains(flag) {
            out = out.add_modifier(modifier);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{parse, rows};

    fn file(path: &str, hunk: &str) -> FileDiff {
        let raw = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n{hunk}");
        parse(&raw).remove(0)
    }

    /// The file's rows and their highlighting.
    fn highlighted(path: &str, hunk: &str) -> (Vec<Row>, Vec<Vec<Run>>) {
        let file = file(path, hunk);
        let rows = rows(&file);
        let runs = highlight(&file, &rows, DiffTheme::GitHubDark).runs;
        (rows, runs)
    }

    fn colors(runs: &[Run]) -> usize {
        let mut seen: Vec<_> = runs.iter().map(|r| r.style.fg).collect();
        seen.dedup();
        seen.len()
    }

    #[test]
    fn runs_cover_the_text_without_the_newline() {
        let (rows, runs) = highlighted("a.rs", "@@ -1 +1 @@\n-fn a() {}\n+fn b() { 1 }\n");
        assert_eq!(runs[2].last().unwrap().end, rows[2].text.len());
        assert!(colors(&runs[2]) > 1, "code should be more than one color");
    }

    #[test]
    fn an_open_string_on_a_removed_line_leaves_the_added_side_alone() {
        let (_, runs) = highlighted("a.py", "@@ -1 +1 @@\n-x = \"\"\"open\n+def f(): pass\n");
        assert!(
            colors(&runs[2]) > 1,
            "the added line must not parse as a string"
        );
    }

    #[test]
    fn a_new_hunk_forgets_the_last_ones_state() {
        let (_, runs) = highlighted(
            "a.py",
            "@@ -0,0 +1 @@\n+x = \"\"\"open\n@@ -8,0 +9 @@\n+def f(): pass\n",
        );
        assert!(colors(&runs[3]) > 1);
    }

    #[test]
    fn a_shebang_names_the_language_of_a_file_with_no_extension() {
        let (_, runs) = highlighted("run", "@@ -0,0 +1,2 @@\n+#!/bin/sh\n+echo hi\n");
        assert!(!runs[2].is_empty());
    }

    #[test]
    fn plain_text_and_overlong_lines_get_no_runs() {
        let (_, runs) = highlighted("notes.txt", "@@ -1 +1 @@\n-a\n+b\n");
        assert!(runs.iter().all(Vec::is_empty));

        let long = "x".repeat(MAX_LINE_BYTES + 1);
        let (_, runs) = highlighted("a.rs", &format!("@@ -1 +1 @@\n-a\n+{long}\n"));
        assert!(runs[2].is_empty());
    }

    #[test]
    fn a_diff_past_the_size_limit_gets_no_runs() {
        let line = "+let x = 1;\n";
        let lines = MAX_FILE_BYTES / line.len() + 1;
        let hunk = format!("@@ -0,0 +1,{lines} @@\n{}", line.repeat(lines));
        let (_, runs) = highlighted("a.rs", &hunk);
        assert!(runs.iter().all(Vec::is_empty));
    }

    #[test]
    fn both_syntax_themes_are_bundled() {
        // `get` panics on a missing theme, so reaching the assert is the test.
        for theme in [DiffTheme::GitHubDark, DiffTheme::GitHubLight] {
            assert!(!assets().themes.get(syntax_theme(theme)).scopes.is_empty());
        }
    }
}

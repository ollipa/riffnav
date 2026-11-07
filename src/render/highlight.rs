//! Syntax highlighting of a file's diff rows.
//!
//! The two sides of a diff are different programs, so each gets its own parser
//! state: feeding removed and added lines through one parser would let a string
//! opened on a removed line swallow the added lines after it.
//!
//! The lines between hunks aren't in the diff, so a hunk starts afresh unless it
//! is handed a [`HunkStart`], which [`super::starts`] works out from the file
//! itself when it can get at it.

use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use ratatui::style::{Color, Modifier, Style};
use two_face::re_exports::syntect::highlighting::{
    self, FontStyle, HighlightIterator, HighlightState, Highlighter,
};
use two_face::re_exports::syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

use super::tokens::{self, Tokens};
use crate::diff::{FileDiff, Row, RowKind};
use crate::theme::DiffTheme;

/// Lines longer than this skip highlighting. A minified bundle is one enormous
/// line that would hold the render up for no useful color.
pub const MAX_LINE_BYTES: usize = 2_000;
/// Diffs bigger than this skip highlighting altogether, for the same reason: a
/// generated file or lockfile would otherwise take minutes to color. Word emphasis
/// still applies, and it's what matters most on a file that big.
pub const MAX_FILE_BYTES: usize = 500_000;

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

/// Start decoding the bundled syntaxes, so it's done by the time they're wanted.
pub fn warm_up() {
    thread::spawn(assets);
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

/// The parser as it stands partway through a file. It holds scopes rather than
/// styles, so a theme change can start from the same place.
#[derive(Debug, Clone)]
pub struct Seed {
    parser: ParseState,
    stack: ScopeStack,
}

/// How a file is parsed: its language, and where each hunk starts. `starts`
/// lines up with the file's hunks, and a hunk past the end of it starts afresh.
#[derive(Debug, Clone, Default)]
pub struct Parsing {
    /// `None` for a file that gets no color.
    pub language: Option<&'static SyntaxReference>,
    pub starts: Vec<HunkStart>,
}

impl Parsing {
    /// What the file's name and rows say, with every hunk starting afresh.
    pub fn of(file: &FileDiff, rows: &[Row]) -> Self {
        Self {
            language: language(file, rows),
            starts: Vec::new(),
        }
    }
}

/// Where one hunk's two sides start. `None` starts a side afresh.
#[derive(Debug, Clone, Default)]
pub struct HunkStart {
    pub old: Option<Seed>,
    pub new: Option<Seed>,
}

/// One side of the diff's parser state.
struct Side<'a> {
    parser: ParseState,
    state: HighlightState,
    highlighter: &'a Highlighter<'a>,
}

impl<'a> Side<'a> {
    fn new(
        syntax: &SyntaxReference,
        seed: Option<&Seed>,
        highlighter: &'a Highlighter<'a>,
    ) -> Self {
        let (parser, stack) = match seed {
            Some(seed) => (seed.parser.clone(), seed.stack.clone()),
            None => (ParseState::new(syntax), ScopeStack::new()),
        };
        Self {
            parser,
            state: HighlightState::new(highlighter, stack),
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

/// Parse `rows` as `parsing` says, each side of the diff on its own. Stops short
/// once `cancelled` is set.
pub fn highlight(
    rows: &[Row],
    theme: DiffTheme,
    parsing: &Parsing,
    cancelled: &AtomicBool,
) -> Syntax {
    let mut out = Syntax {
        runs: vec![Vec::new(); rows.len()],
        tokens: vec![Tokens::default(); rows.len()],
    };
    let Some(syntax) = parsing.language else {
        return out;
    };
    let assets = assets();
    let set = &assets.syntaxes;
    let highlighter = Highlighter::new(assets.themes.get(syntax_theme(theme)));

    let mut old = Side::new(syntax, None, &highlighter);
    let mut new = Side::new(syntax, None, &highlighter);
    let mut hunks = parsing.starts.iter();
    for (at, row) in rows.iter().enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        if let RowKind::HunkHeader { .. } = row.kind {
            let start = hunks.next();
            old = Side::new(syntax, start.and_then(|s| s.old.as_ref()), &highlighter);
            new = Side::new(syntax, start.and_then(|s| s.new.as_ref()), &highlighter);
            continue;
        }
        if row.text.len() > MAX_LINE_BYTES {
            continue;
        }
        let parsed = match row.kind {
            RowKind::Removed => old.line(set, &row.text),
            RowKind::Added => new.line(set, &row.text),
            // Context is on both sides, so both parsers have to see it. The two
            // agree unless the change above altered how it parses, and then the
            // post-image is the one that's true of the code now.
            RowKind::Context => {
                old.line(set, &row.text);
                new.line(set, &row.text)
            }
            RowKind::HunkHeader { .. } | RowKind::NoNewline => None,
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

/// The syntax `file` is parsed with, as far as its name and rows say, or `None`
/// for a file that gets no color: too big, or of no known language.
pub fn language(file: &FileDiff, rows: &[Row]) -> Option<&'static SyntaxReference> {
    if file.raw.len() > MAX_FILE_BYTES {
        return None;
    }
    find_syntax(&assets().syntaxes, file.path(), rows)
}

/// Whether the old side's first line could name `file`'s language where its
/// name and rows don't: it isn't too big, its name means nothing, and the diff
/// doesn't show that line already.
pub fn wants_first_line(file: &FileDiff, rows: &[Row]) -> bool {
    file.raw.len() <= MAX_FILE_BYTES
        && named(&assets().syntaxes, file.path()).is_none()
        && !rows.iter().any(|row| row.old == Some(1))
        && language(file, rows).is_none()
}

/// [`language`], falling back on `first`, the old side's first line, where
/// [`wants_first_line`] says it could help.
pub fn language_with(
    file: &FileDiff,
    rows: &[Row],
    first: Option<&str>,
) -> Option<&'static SyntaxReference> {
    language(file, rows).or_else(|| {
        let first = first.filter(|_| wants_first_line(file, rows))?;
        assets()
            .syntaxes
            .find_syntax_by_first_line(first)
            .filter(|syntax| syntax.name != "Plain Text")
    })
}

/// The parser's state after each count of `lines` in `stops`, which ascend and
/// stay within them, for handing to [`highlight`]. Nothing past the last stop is
/// parsed. A stop of 0 needs no state. Comes back short once `cancelled` is set.
///
/// Lines are treated as in [`highlight`], so a hunk continues from exactly where
/// parsing the whole file would have got to.
pub fn seeds(
    syntax: &SyntaxReference,
    lines: &[&str],
    stops: &[usize],
    cancelled: &AtomicBool,
) -> Vec<Option<Seed>> {
    let set = &assets().syntaxes;
    let mut parser = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut parsed = lines.iter();
    let mut at = 0;
    let mut out = Vec::with_capacity(stops.len());
    for &stop in stops {
        for text in parsed.by_ref().take(stop.saturating_sub(at)) {
            if cancelled.load(Ordering::Relaxed) {
                return out;
            }
            if text.len() > MAX_LINE_BYTES {
                continue;
            }
            let Ok(ops) = parser.parse_line(&format!("{text}\n"), set) else {
                continue;
            };
            for (_, op) in &ops {
                // The highlighter shrugs off an op it can't apply, too.
                let _ = stack.apply(op);
            }
        }
        at = at.max(stop);
        out.push((stop > 0).then(|| Seed {
            parser: parser.clone(),
            stack: stack.clone(),
        }));
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
/// line: the new side's, as the file is now, then the old side's, which still
/// names it when a line went in above. `None` for anything else, including plain
/// text, which has nothing to color and isn't worth parsing.
fn find_syntax<'a>(set: &'a SyntaxSet, path: &str, rows: &[Row]) -> Option<&'a SyntaxReference> {
    let first_line = |side: fn(&Row) -> Option<u32>| {
        let row = rows.iter().find(|row| side(row) == Some(1))?;
        set.find_syntax_by_first_line(&row.text)
    };
    named(set, path)
        .or_else(|| first_line(|row| row.new))
        .or_else(|| first_line(|row| row.old))
        .filter(|syntax| syntax.name != "Plain Text")
}

/// The file's language from its name, then its extension, plain text included.
fn named<'a>(set: &'a SyntaxSet, path: &str) -> Option<&'a SyntaxReference> {
    let path = Path::new(path);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    set.find_syntax_by_extension(name)
        .or_else(|| set.find_syntax_by_extension(extension))
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
    use crate::diff::rows;
    use crate::diff::testing::{header, one};

    fn file(path: &str, hunk: &str) -> FileDiff {
        one(&(header(path) + hunk))
    }

    /// The file's rows and their highlighting.
    fn highlighted(path: &str, hunk: &str) -> (Vec<Row>, Vec<Vec<Run>>) {
        let file = file(path, hunk);
        let rows = rows(&file);
        let parsing = Parsing::of(&file, &rows);
        let runs = highlight(
            &rows,
            DiffTheme::GitHubDark,
            &parsing,
            &AtomicBool::new(false),
        )
        .runs;
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

    fn language_of(path: &str, hunk: &str) -> Option<&'static str> {
        let file = file(path, hunk);
        language(&file, &rows(&file)).map(|syntax| syntax.name.as_str())
    }

    #[test]
    fn a_changed_shebang_names_the_file_as_it_is_now() {
        let hunk = "@@ -1,2 +1,2 @@\n-#!/bin/sh\n+#!/usr/bin/env ruby\n puts 1\n";
        assert_eq!(language_of("run", hunk), Some("Ruby"));
        // A new first line that names nothing leaves it to the old one.
        let hunk = "@@ -1,2 +1,2 @@\n-#!/usr/bin/env ruby\n+hello\n puts 1\n";
        assert_eq!(language_of("run", hunk), Some("Ruby"));
    }

    #[test]
    fn a_line_put_above_the_shebang_leaves_the_old_one_to_name_it() {
        let hunk = "@@ -1,2 +1,3 @@\n+# added\n #!/usr/bin/env python3\n x = 1\n";
        assert_eq!(language_of("run", hunk), Some("Python"));
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

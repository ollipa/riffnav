//! Diff color themes. riffnav renders diffs by shelling out to `delta`, so a
//! "theme" is really a set of `delta` style flags. `GitHubDark` is the default;
//! `Delta` is the baseline (inherit the user's gitconfig, riffnav's historical
//! behavior); the GitHub themes pass `--no-gitconfig` plus explicit styles so
//! the result is deterministic and matches GitHub's web diff colors.
//!
//! Why these colors read better than delta's defaults: delta's default +/-
//! backgrounds are pure, saturated, near-black hues (`#002800` green,
//! `#3f0001` red). GitHub uses softer, desaturated *tints* for whole lines and
//! reserves a stronger shade for the exact changed words (word-level emphasis).
//! The values below are GitHub's actual diff colors — the dark ones are
//! GitHub's translucent overlays flattened over its `#0d1117` canvas.
//!
//! Set a default with `diff_theme` in the config, or cycle at runtime with the
//! `T` key.

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

/// The colors riffnav's renderer paints a diff with.
#[derive(Debug, Clone, Copy)]
pub struct DiffPalette {
    plus_bg: Color,
    plus_emph_bg: Color,
    minus_bg: Color,
    minus_emph_bg: Color,
    /// Background of unchanged lines. `None` leaves the terminal's own.
    context_bg: Option<Color>,
    /// Text with no syntax color: plain text and overlong lines.
    plain: Color,
    pub plus_number: Color,
    pub minus_number: Color,
    pub number: Color,
    heading: Color,
    pub rule: Color,
}

impl DiffPalette {
    pub fn plus(&self) -> Style {
        Style::new().fg(self.plain).bg(self.plus_bg)
    }

    pub fn plus_emph(&self) -> Style {
        Style::new().fg(self.plain).bg(self.plus_emph_bg)
    }

    pub fn minus(&self) -> Style {
        Style::new().fg(self.plain).bg(self.minus_bg)
    }

    pub fn minus_emph(&self) -> Style {
        Style::new().fg(self.plain).bg(self.minus_emph_bg)
    }

    pub fn context(&self) -> Style {
        let style = Style::new().fg(self.plain);
        self.context_bg.map_or(style, |bg| style.bg(bg))
    }

    pub fn heading(&self) -> Style {
        Style::new().fg(self.heading).add_modifier(Modifier::BOLD)
    }

    /// git's `\\ No newline at end of file` marker.
    pub fn marker(&self) -> Style {
        Style::new().fg(self.number).add_modifier(Modifier::ITALIC)
    }

    pub fn whitespace_error(&self) -> Style {
        Style::new()
            .fg(Color::Magenta)
            .add_modifier(Modifier::REVERSED)
    }
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

// Explicit renames (not `rename_all = "kebab-case"`, which would split the enum
// names into `git-hub-dark`) keep the config strings aligned with `name()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum DiffTheme {
    /// Baseline: inherit the user's `delta`/gitconfig theme (historical default).
    #[serde(rename = "delta")]
    Delta,
    /// GitHub dark-mode diff colors.
    #[serde(rename = "github-dark")]
    GitHubDark,
    /// GitHub light-mode diff colors (needs a light terminal background).
    #[serde(rename = "github-light")]
    GitHubLight,
}

impl DiffTheme {
    pub fn next(self) -> Self {
        match self {
            DiffTheme::Delta => DiffTheme::GitHubDark,
            DiffTheme::GitHubDark => DiffTheme::GitHubLight,
            DiffTheme::GitHubLight => DiffTheme::Delta,
        }
    }

    /// Background for the diff pane's line cursor. Only the background changes,
    /// so it has to stay readable behind syntax-highlighted code and behind the
    /// +/- tints it replaces on a changed line.
    pub fn cursor_line_bg(self) -> ratatui::style::Color {
        use ratatui::style::Color;
        match self {
            // GitHub's selected-row blue-grey, flattened over each canvas.
            DiffTheme::GitHubDark => Color::Rgb(0x1c, 0x2c, 0x43),
            DiffTheme::GitHubLight => Color::Rgb(0xdd, 0xe7, 0xf5),
            // The baseline theme borrows the user's own scheme, so use a palette
            // color their terminal already defines rather than a fixed RGB.
            DiffTheme::Delta => Color::DarkGray,
        }
    }

    /// The colors this theme paints diffs with: the same GitHub values as
    /// [`Self::delta_args`], as colors rather than flags.
    pub fn palette(self) -> DiffPalette {
        match self {
            DiffTheme::GitHubLight => DiffPalette {
                plus_bg: rgb(0xe6ffec),
                plus_emph_bg: rgb(0xabf2bc),
                minus_bg: rgb(0xffebe9),
                minus_emph_bg: rgb(0xfdb8c0),
                context_bg: Some(rgb(0xffffff)),
                plain: rgb(0x1f2328),
                plus_number: rgb(0x1a7f37),
                minus_number: rgb(0xcf222e),
                number: rgb(0x6e7781),
                heading: rgb(0x0969da),
                rule: rgb(0xd0d7de),
            },
            DiffTheme::GitHubDark | DiffTheme::Delta => DiffPalette {
                plus_bg: rgb(0x12261d),
                plus_emph_bg: rgb(0x1a4a28),
                minus_bg: rgb(0x301a1e),
                minus_emph_bg: rgb(0x6b2a2b),
                context_bg: None,
                plain: rgb(0xe6edf3),
                plus_number: rgb(0x3fb950),
                minus_number: rgb(0xf85149),
                number: rgb(0x6e7681),
                heading: rgb(0x58a6ff),
                rule: rgb(0x30363d),
            },
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            DiffTheme::Delta => "delta",
            DiffTheme::GitHubDark => "github-dark",
            DiffTheme::GitHubLight => "github-light",
        }
    }

    /// Extra `delta` arguments that paint this theme. Empty for `Delta`, which
    /// keeps riffnav's original gitconfig-driven rendering. Each style flag and
    /// its value are separate argv entries, as `delta` expects.
    ///
    /// `syntax` in a style keeps delta's syntax-highlighted foreground and only
    /// sets the background — so code stays colorized on top of the diff tint,
    /// exactly like GitHub.
    ///
    /// Line numbers themselves aren't listed here: `delta::run` turns them on for
    /// every theme, since riffnav reads comment anchors back out of that gutter.
    pub fn delta_args(self) -> &'static [&'static str] {
        match self {
            DiffTheme::Delta => &[],
            // GitHub dark. Line tints are GitHub's green/red overlays flattened
            // over #0d1117; the emph shades are the same hues at higher opacity.
            DiffTheme::GitHubDark => &[
                "--syntax-theme",
                "Visual Studio Dark+",
                "--plus-style",
                "syntax #12261d",
                "--plus-emph-style",
                "syntax #1a4a28",
                "--minus-style",
                "syntax #301a1e",
                "--minus-emph-style",
                "syntax #6b2a2b",
                "--zero-style",
                "syntax",
                "--line-numbers-plus-style",
                "#3fb950",
                "--line-numbers-minus-style",
                "#f85149",
                "--line-numbers-zero-style",
                "#6e7681",
                "--hunk-header-style",
                "#58a6ff bold",
                "--hunk-header-decoration-style",
                "#30363d box",
                "--hunk-header-line-number-style",
                "#6e7681",
                "--file-style",
                "#58a6ff bold",
                "--file-decoration-style",
                "#30363d ul",
            ],
            // GitHub light. Context lines get a forced white background so the
            // whole pane reads as GitHub's light "card" even when the terminal
            // itself is dark.
            DiffTheme::GitHubLight => &[
                "--light",
                "--syntax-theme",
                "GitHub",
                "--plus-style",
                "syntax #e6ffec",
                "--plus-emph-style",
                "syntax #abf2bc",
                "--minus-style",
                "syntax #ffebe9",
                "--minus-emph-style",
                "syntax #fdb8c0",
                "--zero-style",
                "syntax #ffffff",
                "--line-numbers-plus-style",
                "#1a7f37",
                "--line-numbers-minus-style",
                "#cf222e",
                "--line-numbers-zero-style",
                "#6e7781",
                "--hunk-header-style",
                "#0969da bold",
                "--hunk-header-decoration-style",
                "#d0d7de box",
                "--hunk-header-line-number-style",
                "#6e7781",
                "--file-style",
                "#0969da bold",
                "--file-decoration-style",
                "#d0d7de ul",
            ],
        }
    }
}

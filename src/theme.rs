//! Diff color themes. `GitHubDark` is the default; both use GitHub's diff colors
//! and pair them with a syntax theme (see `render::highlight`).
//!
//! GitHub uses soft, desaturated *tints* for whole lines and reserves a stronger
//! shade for the exact changed words. The values below are GitHub's actual diff
//! colors — the dark ones are its translucent overlays flattened over its
//! `#0d1117` canvas.
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
    /// Background of lines that moved rather than changed.
    moved_bg: Color,
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

    pub fn moved(&self) -> Style {
        Style::new().fg(self.plain).bg(self.moved_bg)
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
    /// GitHub dark-mode diff colors. `delta` is what a config from before riffnav
    /// drew its own diffs might still ask for, and it shouldn't stop it starting.
    #[serde(rename = "github-dark", alias = "delta")]
    GitHubDark,
    /// GitHub light-mode diff colors (needs a light terminal background).
    #[serde(rename = "github-light")]
    GitHubLight,
}

impl DiffTheme {
    pub fn next(self) -> Self {
        match self {
            DiffTheme::GitHubDark => DiffTheme::GitHubLight,
            DiffTheme::GitHubLight => DiffTheme::GitHubDark,
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
        }
    }

    /// The colors this theme paints diffs with.
    pub fn palette(self) -> DiffPalette {
        match self {
            DiffTheme::GitHubLight => DiffPalette {
                plus_bg: rgb(0xe6ffec),
                plus_emph_bg: rgb(0xabf2bc),
                minus_bg: rgb(0xffebe9),
                minus_emph_bg: rgb(0xfdb8c0),
                moved_bg: rgb(0xf2edfc),
                context_bg: Some(rgb(0xffffff)),
                plain: rgb(0x1f2328),
                plus_number: rgb(0x1a7f37),
                minus_number: rgb(0xcf222e),
                number: rgb(0x6e7781),
                heading: rgb(0x0969da),
                rule: rgb(0xd0d7de),
            },
            DiffTheme::GitHubDark => DiffPalette {
                plus_bg: rgb(0x12261d),
                plus_emph_bg: rgb(0x1a4a28),
                minus_bg: rgb(0x301a1e),
                minus_emph_bg: rgb(0x6b2a2b),
                moved_bg: rgb(0x231f39),
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
            DiffTheme::GitHubDark => "github-dark",
            DiffTheme::GitHubLight => "github-light",
        }
    }
}

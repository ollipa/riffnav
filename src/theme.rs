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

use serde::Deserialize;

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
    pub fn delta_args(self) -> &'static [&'static str] {
        match self {
            DiffTheme::Delta => &[],
            // GitHub dark. Line tints are GitHub's green/red overlays flattened
            // over #0d1117; the emph shades are the same hues at higher opacity.
            DiffTheme::GitHubDark => &[
                "--line-numbers",
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
                "--line-numbers",
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

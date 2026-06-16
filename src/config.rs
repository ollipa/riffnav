//! TOML configuration. Effective settings are resolved as
//! defaults < config file < CLI flags — this module owns the first two layers;
//! `main` applies the CLI overrides on top.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::app::Focus;
use crate::icons::IconStyle;
use crate::theme::DiffTheme;

/// User configuration, loaded from `config.toml`. Every field is optional in the
/// file; anything omitted falls back to [`Config::default`] (via `serde(default)`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Initial layout. `None` follows the user's `delta.side-by-side` default;
    /// `Some(_)` forces it (CLI `-s`/`-u` still win over this).
    pub side_by_side: Option<bool>,
    /// File-tree glyphs: `nerd` | `unicode` | `ascii`.
    pub icon_style: IconStyle,
    /// Diff color theme: `delta` (inherit gitconfig) | `github-dark` |
    /// `github-light`. Cycle at runtime with the `T` key.
    pub diff_theme: DiffTheme,
    /// Columns reserved for the file-tree pane, including its right border.
    pub tree_width: u16,
    /// Show the file tree on launch.
    pub show_tree: bool,
    /// Where keyboard focus starts: `diff` reads the first file right away (a
    /// single-file view — move between files with n/p), `tree` starts in the
    /// file list. Ignored when the tree is hidden (focus is forced to the diff).
    pub start_focus: Focus,
    /// Show the top summary bar.
    pub show_header: bool,
    /// Show the bottom hint/status bar.
    pub show_footer: bool,
    /// Expand folders shallower than this depth on launch; deeper folders start
    /// collapsed. The large default means everything starts expanded.
    pub open_depth: usize,
    /// Days to keep "viewed" review marks before they're garbage-collected:
    /// whole branch files untouched for longer are swept on launch, and older
    /// entries within an active file are pruned on save.
    pub review_retention_days: u64,
    /// After marking a file viewed with `v`, jump to the next unviewed file so
    /// review flows file-to-file. Unmarking never moves.
    pub review_auto_advance: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            side_by_side: None,
            icon_style: IconStyle::Nerd,
            diff_theme: DiffTheme::GitHubDark,
            tree_width: 32,
            show_tree: true,
            start_focus: Focus::Diff,
            show_header: true,
            show_footer: true,
            open_depth: 64,
            review_retention_days: 90,
            review_auto_advance: true,
        }
    }
}

impl Config {
    /// Load config from `explicit` if given, otherwise the default XDG location.
    /// A missing *default* file is fine (use built-in defaults); a missing
    /// *explicit* file the user asked for is an error.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        if let Some(path) = explicit {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config file {}", path.display()))?;
            return parse(&text, path);
        }
        let Some(path) = default_path() else {
            return Ok(Self::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => parse(&text, &path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading config file {}", path.display())),
        }
    }
}

fn parse(text: &str, path: &Path) -> Result<Config> {
    toml::from_str(text).with_context(|| format!("parsing config file {}", path.display()))
}

/// `$XDG_CONFIG_HOME/riffnav/config.toml`, falling back to
/// `$HOME/.config/riffnav/config.toml`.
fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("riffnav").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_toml(s: &str) -> Result<Config> {
        toml::from_str(s).map_err(Into::into)
    }

    #[test]
    fn empty_file_yields_defaults() {
        let c = from_toml("").unwrap();
        assert_eq!(c.tree_width, 32);
        assert!(c.show_tree);
        assert!(c.show_header);
        assert_eq!(c.icon_style, IconStyle::Nerd);
        assert_eq!(c.side_by_side, None);
        assert_eq!(c.start_focus, Focus::Diff);
    }

    #[test]
    fn start_focus_parses_from_string() {
        assert_eq!(
            from_toml("start_focus = \"tree\"").unwrap().start_focus,
            Focus::Tree
        );
        assert_eq!(
            from_toml("start_focus = \"diff\"").unwrap().start_focus,
            Focus::Diff
        );
        assert!(from_toml("start_focus = \"sideways\"").is_err());
    }

    #[test]
    fn partial_file_overrides_only_named_keys() {
        let c = from_toml("icon_style = \"ascii\"\ntree_width = 20\n").unwrap();
        assert_eq!(c.icon_style, IconStyle::Ascii);
        assert_eq!(c.tree_width, 20);
        // Untouched keys keep their defaults.
        assert!(c.show_tree);
        assert_eq!(c.open_depth, 64);
    }

    #[test]
    fn side_by_side_can_be_forced() {
        assert_eq!(
            from_toml("side_by_side = true").unwrap().side_by_side,
            Some(true)
        );
        assert_eq!(
            from_toml("side_by_side = false").unwrap().side_by_side,
            Some(false)
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(from_toml("treewidth = 10").is_err());
    }

    #[test]
    fn diff_theme_parses_and_defaults_to_github_dark() {
        assert_eq!(from_toml("").unwrap().diff_theme, DiffTheme::GitHubDark);
        assert_eq!(
            from_toml("diff_theme = \"delta\"").unwrap().diff_theme,
            DiffTheme::Delta
        );
        assert_eq!(
            from_toml("diff_theme = \"github-light\"")
                .unwrap()
                .diff_theme,
            DiffTheme::GitHubLight
        );
        assert!(from_toml("diff_theme = \"solarized\"").is_err());
    }
}

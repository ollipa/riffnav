//! Optional integration with herdr: the terminal multiplexer and the desktop app.
//!
//! Inside either one the `z` key toggles "zoom" (maximize/restore) on riffnav's
//! own pane; elsewhere the feature is absent and the key does nothing.
//!
//! The multiplexer sets `HERDR_ENV=1`. We send it `pane.zoom` over its control
//! socket as newline-delimited JSON. See <https://herdr.dev/docs/socket-api/>.
//!
//! The app gives its terminals `HC_TERMINAL_ID`. That names a terminal, not a
//! pane, so each toggle first finds our pane with `herdr-app ls`, then runs
//! `herdr-app do pane.zoom`. That command only ever answers `ok`, and nothing it
//! offers reports zoom, so the state comes from the app's `view.json`.

mod desktop;
mod socket;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use self::desktop::AppPane;
use self::socket::TuiSocket;

/// How long a single request may block on connect/read/write, or a single
/// `herdr-app` run may take, before we give up, so a wedged herdr can never
/// freeze the UI on a keypress.
const TIMEOUT: Duration = Duration::from_millis(250);

/// A detected herdr session, in the multiplexer or the desktop app.
pub enum Herdr {
    Tui(TuiSocket),
    App(AppPane),
}

impl Herdr {
    /// Detect a herdr session. The multiplexer wins when both are present: it is
    /// the nearer of the two, as its panes inherit the app terminal's environment.
    pub fn detect() -> Option<Self> {
        TuiSocket::detect()
            .map(Self::Tui)
            .or_else(|| AppPane::detect().map(Self::App))
    }

    /// Toggle zoom on riffnav's pane. Returns the new zoom state, or `None` when
    /// herdr doesn't report one.
    pub fn toggle_zoom(&mut self) -> Result<Option<bool>> {
        match self {
            Self::Tui(tui) => tui.toggle_zoom(),
            Self::App(app) => app.toggle_zoom(),
        }
    }

    /// Undo our zoom on the way out.
    pub fn restore_zoom(&self) -> Result<()> {
        match self {
            Self::Tui(tui) => tui.toggle_zoom().map(|_| ()),
            Self::App(app) => app.restore_zoom(),
        }
    }
}

/// `$XDG_CONFIG_HOME`, falling back to `$HOME/.config` (matching
/// `config::default_path`), and to a relative `.config` if neither is set.
fn config_base() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

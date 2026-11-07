//! Optional integration with herdr: the terminal multiplexer and the desktop app.
//!
//! Inside either one the `z` key toggles "zoom" (maximize/restore) on riffnav's
//! own pane; elsewhere the feature is absent and the key does nothing.
//!
//! The multiplexer sets `HERDR_ENV=1`. We send it `pane.zoom` over its control
//! socket as newline-delimited JSON, omitting `pane_id` so it targets the focused
//! pane (us) and `mode` so it toggles. See <https://herdr.dev/docs/socket-api/>.
//!
//! The app gives its terminals `HC_TERMINAL_ID`. That names a terminal, not a
//! pane, so each toggle first finds our pane with `herdr-app ls`, then runs
//! `herdr-app do pane.zoom`. That command only ever answers `ok`, and nothing it
//! offers reports zoom, so the state comes from the app's `view.json`.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[cfg(unix)]
use std::io::{BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

/// How long a single request may block on connect/read/write, or a single
/// `herdr-app` run may take, before we give up, so a wedged herdr can never
/// freeze the UI on a keypress.
const TIMEOUT: Duration = Duration::from_millis(250);

/// How long to wait for `view.json` to show a toggle. The app saved it within
/// 8ms in testing; a lone pane never changes it, so this is also what `z` costs
/// there.
const VIEW_WAIT: Duration = Duration::from_millis(100);

/// A detected herdr session, in the multiplexer or the desktop app.
pub enum Herdr {
    Tui(TuiSocket),
    App(AppPane),
}

impl Herdr {
    /// Detect a herdr session. The multiplexer wins when both are present: it is
    /// the nearer of the two, as its panes inherit the app terminal's environment.
    /// Detection reads the environment once at startup; these variables don't
    /// change while riffnav runs.
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

/// The herdr multiplexer: the resolved control-socket path.
pub struct TuiSocket {
    socket: PathBuf,
}

impl TuiSocket {
    /// Returns `None` unless `HERDR_ENV=1` and a control socket path can be
    /// resolved.
    fn detect() -> Option<Self> {
        if std::env::var("HERDR_ENV").ok().as_deref() != Some("1") {
            return None;
        }
        Some(Self {
            socket: socket_path(),
        })
    }

    /// Toggle zoom on herdr's focused pane (riffnav's own pane). Returns the new
    /// zoom state, or `None` when herdr doesn't report one (e.g. a no-op toggle
    /// on a lone pane).
    fn toggle_zoom(&self) -> Result<Option<bool>> {
        // pane_id omitted → herdr targets the active focused pane, which is us;
        // mode omitted → toggle.
        let resp = self.request(r#"{"id":"riffnav-zoom","method":"pane.zoom","params":{}}"#)?;
        if let Some(err) = resp.error {
            bail!("{}", err.message);
        }
        Ok(resp.result.and_then(|r| r.zoom).and_then(|z| z.zoomed))
    }

    /// Send one newline-delimited JSON request and read the JSON response.
    #[cfg(unix)]
    fn request(&self, line: &str) -> Result<Response> {
        let stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connecting to {}", self.socket.display()))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;

        // `&UnixStream` implements Read/Write, so one stream serves both halves.
        let mut writer = &stream;
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;

        // Pull exactly one JSON value off the stream. A streaming deserializer
        // ends the value by structure, not by newline, so a pretty-printed
        // (multi-line) response — and any trailing bytes herdr writes on the same
        // connection — still parse cleanly.
        let mut values =
            serde_json::Deserializer::from_reader(BufReader::new(&stream)).into_iter::<Response>();
        match values.next() {
            Some(result) => result.context("parsing herdr response"),
            None => bail!("herdr closed the connection without responding"),
        }
    }

    /// On non-unix targets there is no socket to reach; `detect` returns `None`
    /// so this is never called, but it keeps the crate compiling everywhere.
    #[cfg(not(unix))]
    fn request(&self, _line: &str) -> Result<Response> {
        bail!("herdr integration is only supported on Unix")
    }
}

#[derive(Deserialize)]
struct Response {
    result: Option<ZoomResult>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct ZoomResult {
    /// herdr nests the zoom payload one level down under `zoom`; absent on
    /// responses that don't carry one.
    zoom: Option<ZoomState>,
}

#[derive(Deserialize)]
struct ZoomState {
    /// The pane's zoom state after the toggle. Absent or null for no-op toggles
    /// (e.g. a lone pane), so the new state can be unknown.
    zoomed: Option<bool>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}

/// riffnav's terminal in the herdr desktop app.
pub struct AppPane {
    /// `HC_TERMINAL_ID`.
    terminal: String,
    program: OsString,
    view_file: PathBuf,
    /// Whether the tab had a zoomed pane when `z` was first pressed. Anything
    /// but `Some(false)` means any zoom on exit isn't ours to undo.
    found_zoomed: Option<bool>,
}

/// Where our terminal is right now. Looked up on every use, because a terminal
/// can move to another pane or tab while riffnav runs.
#[derive(Debug, PartialEq)]
struct Place {
    tab: String,
    pane: String,
}

impl AppPane {
    /// Returns `None` unless `HC_TERMINAL_ID` is set and a running window shows
    /// that terminal. `hc` terminals can run headless, and `herdr-app` may not be
    /// installed where this one runs.
    fn detect() -> Option<Self> {
        let terminal = std::env::var("HC_TERMINAL_ID")
            .ok()
            .filter(|t| !t.is_empty())?;
        let app = Self::new(
            terminal,
            "herdr-app".into(),
            view_file(
                std::env::var_os("HERDR_APP_HOME").as_deref(),
                &config_base(),
            ),
        );
        app.place().ok()?;
        Some(app)
    }

    fn new(terminal: String, program: OsString, view_file: PathBuf) -> Self {
        Self {
            terminal,
            program,
            view_file,
            found_zoomed: None,
        }
    }

    /// Toggle zoom on our pane and read back the result. `None` means the
    /// toggle was sent but `view.json` can't tell us what it did.
    fn toggle_zoom(&mut self) -> Result<Option<bool>> {
        let place = self.place()?;
        let before = self.zoomed_pane(&place.tab);
        if let Some(zoomed) = &before {
            self.found_zoomed.get_or_insert(zoomed.is_some());
        }
        self.zoom(&place.pane)?;

        let deadline = Instant::now() + VIEW_WAIT;
        let after = loop {
            let after = self.zoomed_pane(&place.tab);
            if after != before || Instant::now() >= deadline {
                break after;
            }
            thread::sleep(Duration::from_millis(2));
        };
        match after {
            None => Ok(None),
            Some(_) if after == before => bail!("nothing to zoom: only pane in this tab"),
            Some(zoomed) => Ok(Some(zoomed.as_deref() == Some(place.pane.as_str()))),
        }
    }

    /// Undo our zoom, unless the tab was already zoomed when we first toggled it
    /// or our pane has since been unzoomed some other way.
    fn restore_zoom(&self) -> Result<()> {
        if self.found_zoomed != Some(false) {
            return Ok(());
        }
        let place = self.place()?;
        if self.zoomed_pane(&place.tab).flatten().as_ref() == Some(&place.pane) {
            self.zoom(&place.pane)?;
        }
        Ok(())
    }

    fn place(&self) -> Result<Place> {
        let out = run_bounded(&self.program, &["ls"])?;
        let listing: AppListing = serde_json::from_str(&out).context("parsing herdr-app ls")?;
        find_place(&listing, &self.terminal)
            .with_context(|| format!("terminal {} isn't in a pane", self.terminal))
    }

    fn zoom(&self, pane: &str) -> Result<()> {
        run_bounded(&self.program, &["do", "pane.zoom", "--pane", pane]).map(|_| ())
    }

    /// The tab's zoomed pane (itself `None` when nothing is zoomed), or `None`
    /// when `view.json` can't be read or doesn't list the tab.
    fn zoomed_pane(&self, tab: &str) -> Option<Option<String>> {
        let text = std::fs::read_to_string(&self.view_file).ok()?;
        let mut view: AppView = serde_json::from_str(&text).ok()?;
        Some(view.tabs.remove(tab)?.zoomed)
    }
}

fn find_place(listing: &AppListing, terminal: &str) -> Option<Place> {
    listing
        .spaces
        .iter()
        .flat_map(|space| &space.tabs)
        .find_map(|tab| {
            let found = tab.terminals.iter().find(|t| t.terminal == terminal)?;
            Some(Place {
                tab: tab.id.clone(),
                pane: found.pane.clone()?,
            })
        })
}

/// Run `program` and return its stdout, killing it after [`TIMEOUT`]. A thread
/// drains stdout so a long listing can't fill the pipe and stall the child.
fn run_bounded(program: &OsString, args: &[&str]) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running herdr-app")?;
    let mut stdout = child.stdout.take().context("herdr-app has no stdout")?;
    let (sender, output) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = sender.send(stdout.read_to_string(&mut text).map(|_| text));
    });

    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("herdr-app {} timed out", args[0]);
        }
        thread::sleep(Duration::from_millis(1));
    };
    let text = output
        .recv_timeout(TIMEOUT)
        .context("reading herdr-app output")??;
    if status.success() {
        return Ok(text);
    }
    match serde_json::from_str::<AppError>(&text) {
        Ok(reply) => bail!("{}", reply.error),
        Err(_) => bail!("herdr-app {} failed ({status})", args[0]),
    }
}

/// `herdr-app ls`, keeping only what locates a terminal.
#[derive(Deserialize)]
struct AppListing {
    spaces: Vec<AppSpace>,
}

#[derive(Deserialize)]
struct AppSpace {
    tabs: Vec<AppTab>,
}

#[derive(Deserialize)]
struct AppTab {
    id: String,
    terminals: Vec<AppTerminal>,
}

#[derive(Deserialize)]
struct AppTerminal {
    terminal: String,
    pane: Option<String>,
}

/// The app's `view.json`, keeping only each tab's zoomed pane.
#[derive(Deserialize)]
struct AppView {
    tabs: HashMap<String, AppTabView>,
}

#[derive(Deserialize)]
struct AppTabView {
    zoomed: Option<String>,
}

#[derive(Deserialize)]
struct AppError {
    error: String,
}

/// `view.json` in the app's home: `HERDR_APP_HOME`, else `herdr-app` under the
/// config dir, the same lookup `herdr-app` itself makes.
fn view_file(home_env: Option<&OsStr>, config_base: &Path) -> PathBuf {
    match home_env.filter(|home| !home.is_empty()) {
        Some(home) => Path::new(home).join("view.json"),
        None => config_base.join("herdr-app").join("view.json"),
    }
}

/// Resolve the control socket from the environment, mirroring herdr's own
/// lookup: an explicit `HERDR_SOCKET_PATH`, else a named-session socket under the
/// config dir, else the default session socket.
fn socket_path() -> PathBuf {
    resolve_socket(
        std::env::var("HERDR_SOCKET_PATH").ok().as_deref(),
        std::env::var("HERDR_SESSION").ok().as_deref(),
        &config_base(),
    )
}

/// Pure socket-path resolution, split out so it can be tested without touching
/// the process environment. `config_base` is the XDG config directory.
fn resolve_socket(
    socket_env: Option<&str>,
    session_env: Option<&str>,
    config_base: &Path,
) -> PathBuf {
    if let Some(path) = socket_env.filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    let herdr = config_base.join("herdr");
    match session_env.filter(|s| !s.is_empty()) {
        Some(name) => herdr.join("sessions").join(name).join("herdr.sock"),
        None => herdr.join("herdr.sock"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_socket_path_wins() {
        let p = resolve_socket(Some("/run/herdr.sock"), Some("work"), Path::new("/cfg"));
        assert_eq!(p, PathBuf::from("/run/herdr.sock"));
    }

    #[test]
    fn named_session_nests_under_sessions() {
        let p = resolve_socket(None, Some("work"), Path::new("/cfg"));
        assert_eq!(p, PathBuf::from("/cfg/herdr/sessions/work/herdr.sock"));
    }

    #[test]
    fn default_session_uses_top_level_socket() {
        let p = resolve_socket(None, None, Path::new("/cfg"));
        assert_eq!(p, PathBuf::from("/cfg/herdr/herdr.sock"));
    }

    #[test]
    fn empty_env_vars_fall_through_to_default() {
        let p = resolve_socket(Some(""), Some(""), Path::new("/cfg"));
        assert_eq!(p, PathBuf::from("/cfg/herdr/herdr.sock"));
    }

    #[test]
    fn view_file_follows_the_app_home() {
        let cfg = Path::new("/cfg");
        let home = OsStr::new("/home/app");
        assert_eq!(
            view_file(Some(home), cfg),
            PathBuf::from("/home/app/view.json")
        );
        let default = PathBuf::from("/cfg/herdr-app/view.json");
        assert_eq!(view_file(None, cfg), default);
        assert_eq!(view_file(Some(OsStr::new("")), cfg), default);
    }

    #[test]
    fn find_place_skips_terminals_without_a_pane() {
        let listing: AppListing = serde_json::from_str(
            r#"{"spaces":[{"id":"s","tabs":[
                {"id":"a","terminals":[{"terminal":"t1","pane":"p1","sub_agents":[]}]},
                {"id":"b","terminals":[{"terminal":"t2","pane":"p2"},{"terminal":"t3","pane":null}]}
            ]}],"recent":[]}"#,
        )
        .unwrap();
        let place = find_place(&listing, "t2").unwrap();
        assert_eq!(
            place,
            Place {
                tab: "b".into(),
                pane: "p2".into()
            }
        );
        assert_eq!(find_place(&listing, "t3"), None);
        assert_eq!(find_place(&listing, "t9"), None);
    }
}

#[cfg(all(test, unix))]
mod socket_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    /// A fresh, empty temp directory unique to this test run.
    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("riffnav-herdr-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Stand up a one-shot herdr stand-in: accept one connection, read the
    /// request line, reply with `response` (plus a newline), then exit. Returns a
    /// `TuiSocket` pointed at its socket.
    fn fake_herdr(dir: &Path, response: &'static str) -> TuiSocket {
        let socket = dir.join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let mut w = &stream;
            w.write_all(response.as_bytes()).unwrap();
            w.write_all(b"\n").unwrap();
        });
        TuiSocket { socket }
    }

    #[test]
    fn toggle_zoom_reads_new_state() {
        let dir = temp_dir("zoomed");
        let herdr = fake_herdr(
            &dir,
            r#"{"id":"riffnav-zoom","result":{"type":"pane_zoom","zoom":{"changed":true,"zoomed":true,"pane_id":"w1-1"}}}"#,
        );
        assert_eq!(herdr.toggle_zoom().unwrap(), Some(true));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_zoom_reports_unzoomed() {
        let dir = temp_dir("unzoomed");
        let herdr = fake_herdr(
            &dir,
            r#"{"id":"riffnav-zoom","result":{"type":"pane_zoom","zoom":{"changed":true,"zoomed":false,"pane_id":"w1-1"}}}"#,
        );
        assert_eq!(herdr.toggle_zoom().unwrap(), Some(false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_zoom_surfaces_error_message() {
        let dir = temp_dir("error");
        let herdr = fake_herdr(
            &dir,
            r#"{"id":"riffnav-zoom","error":{"code":"not_found","message":"pane not found"}}"#,
        );
        let err = herdr.toggle_zoom().unwrap_err().to_string();
        assert_eq!(err, "pane not found");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_zoom_parses_pretty_printed_response() {
        // herdr's real reply spans multiple lines (it carries a nested `layout`),
        // which a line-at-a-time read would mangle. Parsing by structure handles it.
        let dir = temp_dir("multiline");
        let herdr = fake_herdr(
            &dir,
            "{\n  \"id\": \"riffnav-zoom\",\n  \"result\": {\n    \"type\": \"pane_zoom\",\n    \"zoom\": {\n      \"zoomed\": true,\n      \"layout\": { \"panes\": [\"1-1\"] }\n    }\n  }\n}",
        );
        assert_eq!(herdr.toggle_zoom().unwrap(), Some(true));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn toggle_zoom_unknown_state_when_zoomed_absent() {
        let dir = temp_dir("noop");
        let herdr = fake_herdr(
            &dir,
            r#"{"id":"riffnav-zoom","result":{"type":"pane_zoom","zoom":{"changed":false,"reason":"single_pane"}}}"#,
        );
        assert_eq!(herdr.toggle_zoom().unwrap(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// `AppPane` against a fake `herdr-app`: a script that answers `ls` from a file
/// and, on `do`, logs its arguments and swaps `view.json` with `next-view.json`.
/// A test sets those two files to say what a toggle does.
#[cfg(all(test, unix))]
mod app_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const LS: &str = r#"{"spaces":[{"tabs":[{"id":"tab1","terminals":[
        {"terminal":"t1","pane":"p1"},{"terminal":"t2","pane":"p2"}]}]}]}"#;
    const NONE: &str = r#"{"tabs":{"tab1":{"zoomed":null,"focused":"p1"}}}"#;
    const OURS: &str = r#"{"tabs":{"tab1":{"zoomed":"p1"}}}"#;
    const THEIRS: &str = r#"{"tabs":{"tab1":{"zoomed":"p2"}}}"#;

    struct Fake {
        dir: PathBuf,
        app: AppPane,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fake {
        fn new(label: &str, script: &str, view: &str, next_view: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("riffnav-app-{}-{label}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let exe = dir.join("herdr-app");
            std::fs::write(
                &exe,
                format!(
                    "#!/bin/sh\n[ \"$1\" = --probe ] && exit\ncd \"$(dirname \"$0\")\"\n{script}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(dir.join("ls.json"), LS).unwrap();
            std::fs::write(dir.join("view.json"), view).unwrap();
            std::fs::write(dir.join("next-view.json"), next_view).unwrap();
            // A test thread forking while the script was open for writing makes
            // exec fail until that fork has exec'd too.
            while let Err(e) = Command::new(&exe).arg("--probe").output() {
                assert_eq!(e.kind(), std::io::ErrorKind::ExecutableFileBusy);
                thread::sleep(Duration::from_millis(1));
            }
            let view_file = dir.join("view.json");
            let app = AppPane::new("t1".into(), exe.into(), view_file);
            Self { dir, app }
        }

        fn toggling(label: &str, view: &str, next_view: &str) -> Self {
            const SCRIPT: &str = r#"case "$1" in
ls) cat ls.json ;;
do) echo "$*" >> calls
    mv view.json swap.json; mv next-view.json view.json; mv swap.json next-view.json
    echo '{"ok":true}' ;;
esac"#;
            Self::new(label, SCRIPT, view, next_view)
        }

        fn calls(&self) -> String {
            std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default()
        }
    }

    #[test]
    fn toggle_names_our_pane_and_reads_the_new_state() {
        let mut fake = Fake::toggling("toggle", NONE, OURS);
        assert_eq!(fake.app.toggle_zoom().unwrap(), Some(true));
        assert_eq!(fake.calls(), "do pane.zoom --pane p1\n");
        assert_eq!(fake.app.toggle_zoom().unwrap(), Some(false));
    }

    #[test]
    fn zoom_from_outside_is_seen_on_the_next_toggle() {
        let mut fake = Fake::toggling("outside", NONE, OURS);
        assert_eq!(fake.app.toggle_zoom().unwrap(), Some(true));
        // The person unzooms with their own key while riffnav runs.
        std::fs::write(fake.dir.join("view.json"), NONE).unwrap();
        std::fs::write(fake.dir.join("next-view.json"), OURS).unwrap();
        assert_eq!(fake.app.toggle_zoom().unwrap(), Some(true));
    }

    #[test]
    fn taking_zoom_from_another_pane_counts_as_zoomed() {
        let mut fake = Fake::toggling("sibling", THEIRS, OURS);
        assert_eq!(fake.app.toggle_zoom().unwrap(), Some(true));
    }

    #[test]
    fn lone_pane_says_there_is_nothing_to_zoom() {
        let mut fake = Fake::toggling("lone", NONE, NONE);
        let err = fake.app.toggle_zoom().unwrap_err().to_string();
        assert_eq!(err, "nothing to zoom: only pane in this tab");
    }

    #[test]
    fn unreadable_view_leaves_the_state_unknown() {
        for (label, view) in [("garbage", "not json"), ("no-tab", r#"{"tabs":{}}"#)] {
            let mut fake = Fake::toggling(label, view, view);
            assert_eq!(fake.app.toggle_zoom().unwrap(), None);
            fake.app.restore_zoom().unwrap();
            assert_eq!(fake.calls().lines().count(), 1, "{label}: no restore");
        }
    }

    #[test]
    fn restore_undoes_our_zoom() {
        let mut fake = Fake::toggling("restore", NONE, OURS);
        fake.app.toggle_zoom().unwrap();
        fake.app.restore_zoom().unwrap();
        assert_eq!(fake.calls().lines().count(), 2);
    }

    #[test]
    fn restore_leaves_a_zoom_the_person_already_undid() {
        let mut fake = Fake::toggling("undone", NONE, OURS);
        fake.app.toggle_zoom().unwrap();
        std::fs::write(fake.dir.join("view.json"), NONE).unwrap();
        fake.app.restore_zoom().unwrap();
        assert_eq!(fake.calls().lines().count(), 1);
    }

    #[test]
    fn restore_leaves_a_zoom_that_was_there_first() {
        let mut fake = Fake::toggling("theirs", OURS, NONE);
        assert_eq!(fake.app.toggle_zoom().unwrap(), Some(false));
        assert_eq!(fake.app.toggle_zoom().unwrap(), Some(true));
        fake.app.restore_zoom().unwrap();
        assert_eq!(fake.calls().lines().count(), 2);
    }

    #[test]
    fn restore_is_a_no_op_before_any_toggle() {
        let fake = Fake::toggling("untouched", OURS, NONE);
        fake.app.restore_zoom().unwrap();
        assert_eq!(fake.calls(), "");
    }

    #[test]
    fn pane_is_looked_up_on_every_toggle() {
        let mut fake = Fake::toggling("moved", NONE, OURS);
        fake.app.toggle_zoom().unwrap();
        let moved = LS.replace(r#""pane":"p1""#, r#""pane":"p9""#);
        std::fs::write(fake.dir.join("ls.json"), moved).unwrap();
        fake.app.toggle_zoom().unwrap();
        assert!(fake.calls().ends_with("do pane.zoom --pane p9\n"));
    }

    #[test]
    fn terminal_without_a_pane_is_an_error() {
        let mut fake = Fake::toggling("parked", NONE, OURS);
        std::fs::write(fake.dir.join("ls.json"), r#"{"spaces":[]}"#).unwrap();
        let err = fake.app.toggle_zoom().unwrap_err().to_string();
        assert_eq!(err, "terminal t1 isn't in a pane");
        assert_eq!(fake.calls(), "");
    }

    #[test]
    fn error_reply_is_surfaced() {
        let script = r#"[ "$1" = ls ] && { cat ls.json; exit; }
echo '{ "error": "pane gone" }'; exit 1"#;
        let mut fake = Fake::new("error", script, NONE, OURS);
        let err = fake.app.toggle_zoom().unwrap_err().to_string();
        assert_eq!(err, "pane gone");
    }

    #[test]
    fn missing_window_fails_without_a_reply() {
        let fake = Fake::new("no-window", "exit 2", NONE, OURS);
        let err = fake.app.place().unwrap_err().to_string();
        assert_eq!(err, "herdr-app ls failed (exit status: 2)");
    }

    #[test]
    fn missing_program_is_an_error() {
        let program = OsString::from("/nonexistent/herdr-app");
        assert!(run_bounded(&program, &["ls"]).is_err());
    }

    #[test]
    fn stuck_program_is_killed_and_reaped() {
        let fake = Fake::new("stuck", "echo $$ > pid; exec sleep 30", NONE, OURS);
        let started = Instant::now();
        let err = fake.app.place().unwrap_err().to_string();
        assert_eq!(err, "herdr-app ls timed out");
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid = std::fs::read_to_string(fake.dir.join("pid")).unwrap();
        // A zombie would still be listed: gone means killed and waited for.
        assert!(!Path::new("/proc").join(pid.trim()).exists());
    }

    #[test]
    fn output_larger_than_a_pipe_buffer_is_read_whole() {
        let tabs = format!(
            r#"{{"spaces":[{{"tabs":[{}{{"id":"last","terminals":[{{"terminal":"t1","pane":"p1"}}]}}]}}]}}"#,
            r#"{"id":"x","terminals":[]},"#.repeat(10_000)
        );
        let fake = Fake::new("big", "cat ls.json", NONE, OURS);
        std::fs::write(fake.dir.join("ls.json"), tabs).unwrap();
        assert_eq!(fake.app.place().unwrap().tab, "last");
    }
}

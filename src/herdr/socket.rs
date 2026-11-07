//! The multiplexer, asked over its control socket.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[cfg(unix)]
use std::io::{BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;

use super::{TIMEOUT, config_base};

/// The herdr multiplexer: the resolved control-socket path.
pub struct TuiSocket {
    socket: PathBuf,
}

impl TuiSocket {
    /// Returns `None` unless `HERDR_ENV=1` and a control socket path can be
    /// resolved.
    pub(super) fn detect() -> Option<Self> {
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
    pub(super) fn toggle_zoom(&self) -> Result<Option<bool>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

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
}

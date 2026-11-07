//! Telling an agent in the desktop app that the person left it review comments.
//!
//! A notice is typed into the agent's terminal with `hc send`, as if the person
//! had typed it. An agent mid-turn, or waiting on a question the text would
//! answer, isn't typed at: the notice waits in a background thread until the
//! agent is free, and reports how it's getting on over a channel.

use std::ffi::{OsStr, OsString};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::Duration;

use anyhow::{Result, bail};
use serde::Deserialize;

use super::desktop::{AppPane, run_for};

/// How often a waiting notice looks at the agent again.
const POLL: Duration = Duration::from_secs(1);
/// How long `hc send` may take. It waits for the person to stop typing in that
/// terminal before it pastes, so this is generous.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// `hc send`'s exit code for a program blocked on a dialog.
const BLOCKED: i32 = 9;
/// Listing failures in a row that give up on the notice. One slow `herdr-app ls`
/// shouldn't drop a notice that has been waiting for minutes.
const LISTING_TRIES: u32 = 3;

/// Text for one agent, and the comments it tells it about.
#[derive(Debug, Clone)]
pub struct Notice {
    pub terminal: String,
    pub label: String,
    pub text: String,
    pub ids: Vec<String>,
}

/// How a notice is getting on.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// The agent is busy, with this status; the notice waits.
    Waiting(String),
    Sent,
    /// The agent's terminal is gone.
    Gone,
    Failed(String),
}

impl Progress {
    fn is_final(&self) -> bool {
        !matches!(self, Self::Waiting(_))
    }
}

/// A notice on its way, in a thread of its own. Dropping it stops the thread
/// before it next looks or types, so a notice left behind by quitting never goes.
pub struct Delivery {
    pub notice: Notice,
    progress: Receiver<Progress>,
    stop: Arc<AtomicBool>,
}

impl Drop for Delivery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Delivery {
    pub(super) fn start(pane: AppPane, hc: OsString, notice: Notice) -> Self {
        let (sender, progress) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (sent, stopped) = (notice.clone(), stop.clone());
        thread::spawn(move || {
            deliver(&pane, &hc, &sent, POLL, &stopped, |p| {
                sender.send(p).is_ok()
            });
        });
        Self {
            notice,
            progress,
            stop,
        }
    }

    #[cfg(test)]
    pub fn for_test(notice: Notice, progress: Receiver<Progress>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        Self {
            notice,
            progress,
            stop,
        }
    }

    /// The latest news, if any, without blocking. A thread that died without a
    /// last word reads as a failure, so the notice is never stuck as queued.
    pub fn poll(&self) -> Option<Progress> {
        let mut latest = None;
        loop {
            match self.progress.try_recv() {
                Ok(p) => {
                    let done = p.is_final();
                    latest = Some(p);
                    if done {
                        return latest;
                    }
                }
                Err(TryRecvError::Empty) => return latest,
                Err(TryRecvError::Disconnected) => {
                    return latest.or(Some(Progress::Failed("delivery stopped".into())));
                }
            }
        }
    }
}

/// Wait for the agent to be free, then type the notice. Every look is reported.
/// `stopped`, or a `false` from `report` (nobody listening), ends it unsent.
fn deliver(
    pane: &AppPane,
    hc: &OsStr,
    notice: &Notice,
    poll: Duration,
    stopped: &AtomicBool,
    mut report: impl FnMut(Progress) -> bool,
) {
    let mut misses = 0;
    loop {
        if stopped.load(Ordering::Relaxed) {
            return;
        }
        let waiting = match pane.agent(&notice.terminal) {
            Err(e) => {
                misses += 1;
                if misses >= LISTING_TRIES {
                    report(Progress::Failed(format!("{e:#}")));
                    return;
                }
                None
            }
            Ok(None) => {
                report(Progress::Gone);
                return;
            }
            Ok(Some(agent)) if agent.busy() => {
                misses = 0;
                agent.status
            }
            Ok(Some(_)) if stopped.load(Ordering::Relaxed) => return,
            Ok(Some(_)) => {
                misses = 0;
                match send(hc, &notice.terminal, &notice.text) {
                    Ok(Sent::Yes) => {
                        report(Progress::Sent);
                        return;
                    }
                    Ok(Sent::Blocked) => Some("blocked".into()),
                    Err(e) => {
                        report(Progress::Failed(format!("{e:#}")));
                        return;
                    }
                }
            }
        };
        if let Some(status) = waiting
            && !report(Progress::Waiting(status))
        {
            return;
        }
        thread::sleep(poll);
    }
}

enum Sent {
    Yes,
    /// The program put up a dialog first; typing now would answer it.
    Blocked,
}

fn send(hc: &OsStr, terminal: &str, text: &str) -> Result<Sent> {
    let done = run_for(hc, &["send", terminal, text], SEND_TIMEOUT)?;
    if done.status.success() {
        return Ok(Sent::Yes);
    }
    if done.status.code() == Some(BLOCKED) {
        return Ok(Sent::Blocked);
    }
    match serde_json::from_str::<HcError>(&done.stderr) {
        Ok(reply) => bail!("{}", reply.error.message),
        Err(_) => bail!("hc send failed ({})", done.status),
    }
}

/// `hc`'s error reply on stderr.
#[derive(Deserialize)]
struct HcError {
    error: HcErrorBody,
}

#[derive(Deserialize)]
struct HcErrorBody {
    message: String,
}

/// Delivery against a fake `herdr-app` that answers `ls` from `ls.json`, and a
/// fake `hc` that logs what it's asked to send and exits with the code in
/// `code` (0 when there's none).
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    fn ls(status: &str) -> String {
        format!(
            r#"{{"spaces":[{{"tabs":[{{"id":"a","terminals":[
            {{"terminal":"t1","pane":"p1"}},
            {{"terminal":"t2","pane":"p2","app":"claude-code","name":"review","status":{status}}}]}}]}}]}}"#
        )
    }

    struct Fake {
        dir: TempDir,
        pane: AppPane,
        hc: OsString,
    }

    fn script(dir: &Path, name: &str, body: &str) -> OsString {
        let exe = dir.join(name);
        std::fs::write(
            &exe,
            format!("#!/bin/sh\ncd \"$(dirname \"$0\")\"\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A test thread forking while the script was open for writing makes
        // exec fail until that fork has exec'd too.
        while let Err(e) = Command::new(&exe).arg("--probe").output() {
            assert_eq!(e.kind(), std::io::ErrorKind::ExecutableFileBusy);
            thread::sleep(Duration::from_millis(1));
        }
        exe.into_os_string()
    }

    impl Fake {
        fn new(label: &str, status: &str) -> Self {
            let dir = TempDir::new(label);
            std::fs::write(dir.join("ls.json"), ls(status)).unwrap();
            let app = script(&dir, "herdr-app", "[ \"$1\" = ls ] && cat ls.json");
            let hc = script(
                &dir,
                "hc",
                r#"[ "$1" = --probe ] && exit
printf '%s|%s|%s\n' "$1" "$2" "$3" >> sent
[ -f stderr ] && cat stderr >&2
exit "$(cat code 2>/dev/null || echo 0)""#,
            );
            let pane = AppPane::new("t1".into(), app, dir.join("view.json"));
            Self { dir, pane, hc }
        }

        fn notice() -> Notice {
            Notice {
                terminal: "t2".into(),
                label: "review".into(),
                text: "[riffnav] hello".into(),
                ids: vec!["a1".into()],
            }
        }

        /// Run a delivery to the end, letting `step` change the world after each
        /// report. Returns every report.
        fn run(&self, mut step: impl FnMut(&Self, usize)) -> Vec<Progress> {
            let mut seen = Vec::new();
            deliver(
                &self.pane,
                &self.hc,
                &Self::notice(),
                Duration::ZERO,
                &AtomicBool::new(false),
                |p| {
                    seen.push(p);
                    step(self, seen.len());
                    seen.len() < 20
                },
            );
            seen
        }

        fn sent(&self) -> String {
            std::fs::read_to_string(self.dir.join("sent")).unwrap_or_default()
        }

        fn set(&self, file: &str, text: &str) {
            std::fs::write(self.dir.join(file), text).unwrap();
        }
    }

    #[test]
    fn a_free_agent_is_sent_to_at_once() {
        for status in ["null", r#""idle""#, r#""done""#] {
            let fake = Fake::new("free", status);
            assert_eq!(fake.run(|_, _| {}), [Progress::Sent], "{status}");
            assert_eq!(fake.sent(), "send|t2|[riffnav] hello\n");
        }
    }

    #[test]
    fn a_busy_agent_is_waited_for() {
        let fake = Fake::new("busy", r#""working""#);
        let seen = fake.run(|fake, n| match n {
            2 => fake.set("ls.json", &ls(r#""blocked""#)),
            3 => fake.set("ls.json", &ls(r#""idle""#)),
            _ => {}
        });
        let waiting = |s: &str| Progress::Waiting(s.into());
        assert_eq!(
            seen,
            [
                waiting("working"),
                waiting("working"),
                waiting("blocked"),
                Progress::Sent
            ]
        );
        assert_eq!(
            fake.sent().lines().count(),
            1,
            "typed only once it was free"
        );
    }

    #[test]
    fn a_send_refused_as_blocked_keeps_waiting() {
        let fake = Fake::new("refused", r#""idle""#);
        fake.set("code", "9");
        let seen = fake.run(|fake, n| {
            if n == 2 {
                fake.set("code", "0");
            }
        });
        let blocked = Progress::Waiting("blocked".into());
        assert_eq!(seen, [blocked.clone(), blocked, Progress::Sent]);
    }

    #[test]
    fn a_failed_send_says_why() {
        let fake = Fake::new("failed", r#""idle""#);
        fake.set("code", "3");
        fake.set(
            "stderr",
            r#"{"error":{"kind":"not_found","message":"no terminal t2","hint":"hc ls"}}"#,
        );
        assert_eq!(
            fake.run(|_, _| {}),
            [Progress::Failed("no terminal t2".into())]
        );
        std::fs::remove_file(fake.dir.join("stderr")).unwrap();
        assert_eq!(
            fake.run(|_, _| {}),
            [Progress::Failed("hc send failed (exit status: 3)".into())]
        );
    }

    #[test]
    fn an_agent_that_leaves_is_gone() {
        let fake = Fake::new("gone", r#""working""#);
        let seen = fake.run(|fake, _| fake.set("ls.json", r#"{"spaces":[]}"#));
        assert_eq!(seen, [Progress::Waiting("working".into()), Progress::Gone]);
        assert_eq!(fake.sent(), "");
    }

    #[test]
    fn listing_failures_give_up_after_a_few_tries() {
        let fake = Fake::new("ls-fails", r#""idle""#);
        fake.set("ls.json", "not json");
        let seen = fake.run(|_, _| {});
        assert_eq!(seen.len(), 1);
        assert!(matches!(&seen[0], Progress::Failed(e) if e.contains("parsing")));
    }

    #[test]
    fn nobody_listening_stops_the_wait() {
        let fake = Fake::new("dropped", r#""working""#);
        let mut looks = 0;
        deliver(
            &fake.pane,
            &fake.hc,
            &Fake::notice(),
            Duration::ZERO,
            &AtomicBool::new(false),
            |_| {
                looks += 1;
                false
            },
        );
        assert_eq!(looks, 1);
        assert_eq!(fake.sent(), "");
    }

    /// Quitting riffnav drops the delivery: a notice still waiting on a busy
    /// agent must not be typed once that agent comes free.
    #[test]
    fn a_stopped_delivery_never_types() {
        let fake = Fake::new("stopped", r#""working""#);
        let stopped = AtomicBool::new(false);
        let mut seen = Vec::new();
        deliver(
            &fake.pane,
            &fake.hc,
            &Fake::notice(),
            Duration::ZERO,
            &stopped,
            |p| {
                seen.push(p);
                stopped.store(true, Ordering::Relaxed);
                fake.set("ls.json", &ls(r#""idle""#));
                seen.len() < 5
            },
        );
        assert_eq!(seen, [Progress::Waiting("working".into())]);
        assert_eq!(fake.sent(), "");
    }

    #[test]
    fn a_delivery_reports_through_its_channel() {
        let fake = Fake::new("thread", r#""idle""#);
        let delivery = Delivery::start(fake.pane.clone(), fake.hc.clone(), Fake::notice());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let last = loop {
            if let Some(p) = delivery.poll() {
                break p;
            }
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(last, Progress::Sent);
    }
}

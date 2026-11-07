//! The `S` key: tell the agent in this herdr tab about the person's new comments.
//!
//! Comments written here collect quietly. `S` sends one notice for all of them,
//! to the one agent in the tab or the one picked from several, and remembers the
//! pick. A busy agent's notice waits in the background until it's free; the ids
//! only count as sent once it has gone.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent};

use super::App;
use crate::herdr::{Agent, Delivery, Herdr, Notice, Progress};

/// The person's comments on their way to an agent.
#[derive(Default)]
pub(super) struct Outbox {
    /// Ids of the comments and replies written in this window, oldest first.
    /// Whatever was on disk before, or came from an agent, is never news.
    written: Vec<String>,
    sent: HashSet<String>,
    /// The agent's terminal picked for this session.
    chosen: Option<String>,
    picker: Option<AgentPicker>,
    delivery: Option<Delivery>,
    /// The busy agent's status, while the notice waits on it.
    waiting_on: Option<String>,
}

/// Choosing among several agents in the tab.
pub struct AgentPicker {
    pub agents: Vec<Agent>,
    pub selected: usize,
}

impl App {
    /// Whether `S` can work here: inside the herdr desktop app.
    pub fn can_send_comments(&self) -> bool {
        self.herdr.as_ref().is_some_and(Herdr::can_send)
    }

    pub fn agent_picker(&self) -> Option<&AgentPicker> {
        self.outbox.picker.as_ref()
    }

    pub(super) fn picking_agent(&self) -> bool {
        self.outbox.picker.is_some()
    }

    pub(super) fn sending(&self) -> bool {
        self.outbox.delivery.is_some()
    }

    /// Drop a notice still waiting, unsent: riffnav is quitting.
    pub(super) fn stop_sending(&mut self) {
        self.outbox.delivery = None;
    }

    /// Note a comment written in this window, as news for the agent.
    pub(super) fn record_written(&mut self, id: String) {
        self.outbox.written.push(id);
    }

    /// The person's comments the agent hasn't been told about, and that aren't
    /// already on their way. A comment deleted before it went is left out.
    fn unsent_ids(&self) -> Vec<String> {
        let queued = self.outbox.delivery.as_ref().map(|d| &d.notice.ids);
        self.outbox
            .written
            .iter()
            .filter(|id| !self.outbox.sent.contains(*id))
            .filter(|id| !queued.is_some_and(|q| q.contains(id)))
            .filter(|id| self.comments.get(id).is_some())
            .cloned()
            .collect()
    }

    pub fn unsent_count(&self) -> usize {
        if !self.can_send_comments() {
            return 0;
        }
        self.unsent_ids().len()
    }

    /// What the header says about a notice on its way.
    pub fn queued_label(&self) -> Option<String> {
        let notice = &self.outbox.delivery.as_ref()?.notice;
        let n = notice.ids.len();
        let label = &notice.label;
        Some(match &self.outbox.waiting_on {
            Some(status) => format!("✉ {n} queued for {label} ({status})"),
            None => format!("✉ {n} sending to {label}"),
        })
    }

    /// `S`.
    pub(super) fn send_comments(&mut self) {
        if !self.can_send_comments() {
            return self.set_status("Sending comments needs the herdr app");
        }
        if let Some(delivery) = &self.outbox.delivery {
            let label = &delivery.notice.label;
            let msg = match self.outbox.waiting_on {
                Some(_) => format!("Already queued for {label}: it goes once {label} is free"),
                None => format!("Already sending to {label}"),
            };
            return self.set_status(msg);
        }
        if self.unsent_ids().is_empty() {
            return self.set_status("No new comments to send");
        }
        if self.comments.path().is_none() {
            return self.set_status(
                "Comments outside a git repo aren't saved, so an agent can't read them",
            );
        }
        let Some(herdr) = &self.herdr else { return };
        let agents = match herdr.tab_agents() {
            Ok(agents) => agents,
            Err(e) => return self.set_status(format!("herdr: {e:#}")),
        };
        if let Some(chosen) = &self.outbox.chosen {
            match agents.iter().find(|a| &a.terminal == chosen) {
                Some(agent) => return self.start_delivery(agent.clone()),
                None => self.outbox.chosen = None,
            }
        }
        match agents.len() {
            0 => self.set_status("No agent in this tab"),
            1 => self.start_delivery(agents[0].clone()),
            _ => {
                self.outbox.picker = Some(AgentPicker {
                    agents,
                    selected: 0,
                })
            }
        }
    }

    /// A key while the picker is open.
    pub(super) fn picker_key(&mut self, key: KeyEvent) {
        let Some(picker) = &mut self.outbox.picker else {
            return;
        };
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                picker.selected = (picker.selected + 1).min(picker.agents.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Enter => {
                let agent = picker.agents.get(picker.selected).cloned();
                self.outbox.picker = None;
                if let Some(agent) = agent {
                    self.start_delivery(agent);
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.outbox.picker = None;
                self.set_status("Not sent");
            }
            _ => {}
        }
    }

    fn start_delivery(&mut self, agent: Agent) {
        let ids = self.unsent_ids();
        let notice = Notice {
            text: notice_text(ids.len(), self.comments.repo(), self.comments.branch()),
            terminal: agent.terminal.clone(),
            label: agent.label.clone(),
            ids,
        };
        let Some(herdr) = &self.herdr else { return };
        match herdr.deliver(notice) {
            Ok(delivery) => self.queue(delivery, &agent),
            Err(e) => self.set_status(format!("herdr: {e:#}")),
        }
    }

    fn queue(&mut self, delivery: Delivery, agent: &Agent) {
        self.outbox.chosen = Some(agent.terminal.clone());
        self.outbox.delivery = Some(delivery);
        self.outbox.waiting_on = agent.busy().then(|| agent.status.clone()).flatten();
        let label = &agent.label;
        let msg = match &self.outbox.waiting_on {
            Some(status) => {
                format!("✉ Queued for {label} ({status}): it goes once {label} is free")
            }
            None => format!("✉ Sending to {label}…"),
        };
        self.set_status(msg);
    }

    /// Take in how the notice on its way is getting on.
    pub(super) fn drain_delivery(&mut self) {
        let Some(delivery) = &self.outbox.delivery else {
            return;
        };
        let Some(progress) = delivery.poll() else {
            return;
        };
        let label = delivery.notice.label.clone();
        if let Progress::Waiting(status) = progress {
            self.outbox.waiting_on = Some(status);
            return;
        }
        let Some(delivery) = self.outbox.delivery.take() else {
            return;
        };
        self.outbox.waiting_on = None;
        let n = delivery.notice.ids.len();
        let msg = match progress {
            Progress::Waiting(_) => unreachable!("handled above"),
            Progress::Sent => {
                self.outbox.sent.extend(delivery.notice.ids.iter().cloned());
                format!("✉ Sent {n} {} to {label}", plural(n, "comment", "comments"))
            }
            Progress::Gone => {
                if self.outbox.chosen.as_ref() == Some(&delivery.notice.terminal) {
                    self.outbox.chosen = None;
                }
                format!("{label} has gone; comments not sent")
            }
            Progress::Failed(e) => format!("Couldn't send comments to {label}: {e}"),
        };
        self.set_status(msg);
    }
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// The one line typed into the agent's terminal. It reads as if the person
/// typed it, so it says who it's from and what it's about, and where: the agent
/// may be in another worktree or on another branch.
fn notice_text(n: usize, repo: &str, branch: &str) -> String {
    let what = plural(n, "comment", "comments");
    let them = plural(n, "it", "them");
    let branch = match branch {
        "(detached)" => "detached HEAD".to_string(),
        branch => format!("branch {branch}"),
    };
    format!(
        "[riffnav] The user left {n} new review {what} for you in {repo} ({branch}). \
         Read {them} with `riffnav comment list` from that folder. \
         If riffnav's comment commands aren't in your context, `riffnav skill` explains them."
    )
}

#[cfg(test)]
impl App {
    pub(crate) fn open_picker_for_test(&mut self, agents: Vec<Agent>) {
        self.outbox.picker = Some(AgentPicker {
            agents,
            selected: 1,
        });
    }
}

#[cfg(test)]
impl Delivery {
    /// A delivery whose progress the test reports by hand.
    fn fake(notice: Notice) -> (Self, std::sync::mpsc::Sender<Progress>) {
        let (sender, progress) = std::sync::mpsc::channel();
        (Self::for_test(notice, progress), sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{app_with, file};
    use crate::comment::testing::note;
    use crate::comment::{Comment, CommentStore};

    #[test]
    fn notice_says_who_what_and_where() {
        assert_eq!(
            notice_text(2, "/src/app", "main"),
            "[riffnav] The user left 2 new review comments for you in /src/app (branch main). \
             Read them with `riffnav comment list` from that folder. \
             If riffnav's comment commands aren't in your context, `riffnav skill` explains them."
        );
        let one = notice_text(1, "/r", "(detached)");
        assert!(one.contains("1 new review comment for you in /r (detached HEAD). Read it "));
        assert!(!one.contains('\n'), "one line, so it goes as one message");
    }

    fn agent(terminal: &str, status: Option<&str>) -> Agent {
        Agent {
            terminal: terminal.into(),
            label: format!("agent-{terminal}"),
            status: status.map(str::to_string),
        }
    }

    /// An app with two comments written here and one from an agent.
    fn app_with_comments() -> (App, Vec<String>) {
        let mut app = app_with(vec![file("a.rs")]);
        let mut store = CommentStore::disabled();
        let mut ids = Vec::new();
        for line in [1, 2] {
            ids.push(store.add(Comment {
                author: "tester".into(),
                ..note("a.rs", line, "mine")
            }));
        }
        store.add(Comment {
            author: "claude".into(),
            ..note("a.rs", 3, "theirs")
        });
        app.install_comments_for_test(store);
        for id in &ids {
            app.record_written(id.clone());
        }
        (app, ids)
    }

    fn notice(ids: &[String]) -> Notice {
        Notice {
            terminal: "t2".into(),
            label: "review".into(),
            text: String::new(),
            ids: ids.to_vec(),
        }
    }

    #[test]
    fn only_comments_written_here_and_still_there_are_unsent() {
        let (mut app, ids) = app_with_comments();
        assert_eq!(app.unsent_ids(), ids);
        app.comments.remove(&ids[0]);
        assert_eq!(app.unsent_ids(), [ids[1].clone()]);
    }

    #[test]
    fn a_notice_marks_its_comments_sent_only_once_it_goes() {
        let (mut app, ids) = app_with_comments();
        let (delivery, progress) = Delivery::fake(notice(&ids[..1]));
        app.queue(delivery, &agent("t2", Some("working")));
        assert_eq!(app.unsent_ids(), [ids[1].clone()], "queued isn't unsent");
        assert_eq!(
            app.queued_label().as_deref(),
            Some("✉ 1 queued for review (working)")
        );

        progress.send(Progress::Waiting("blocked".into())).unwrap();
        app.drain_delivery();
        assert_eq!(
            app.queued_label().as_deref(),
            Some("✉ 1 queued for review (blocked)")
        );

        progress.send(Progress::Sent).unwrap();
        app.drain_delivery();
        assert!(!app.sending());
        assert_eq!(app.status.as_deref(), Some("✉ Sent 1 comment to review"));
        assert_eq!(app.unsent_ids(), [ids[1].clone()], "the sent one is done");
        assert_eq!(app.outbox.chosen.as_deref(), Some("t2"), "and remembered");
    }

    #[test]
    fn a_failed_notice_leaves_its_comments_unsent() {
        let (mut app, ids) = app_with_comments();
        let (delivery, progress) = Delivery::fake(notice(&ids));
        app.queue(delivery, &agent("t2", None));
        progress.send(Progress::Failed("boom".into())).unwrap();
        app.drain_delivery();
        assert_eq!(
            app.status.as_deref(),
            Some("Couldn't send comments to review: boom")
        );
        assert_eq!(app.unsent_ids(), ids);
        assert_eq!(app.outbox.chosen.as_deref(), Some("t2"));
    }

    #[test]
    fn a_gone_agent_is_forgotten() {
        let (mut app, ids) = app_with_comments();
        let (delivery, progress) = Delivery::fake(notice(&ids));
        app.queue(delivery, &agent("t2", None));
        progress.send(Progress::Gone).unwrap();
        app.drain_delivery();
        assert_eq!(app.outbox.chosen, None);
        assert_eq!(app.unsent_ids(), ids);
    }

    #[test]
    fn a_dead_delivery_thread_reads_as_failed() {
        let (mut app, ids) = app_with_comments();
        let (delivery, progress) = Delivery::fake(notice(&ids));
        app.queue(delivery, &agent("t2", None));
        drop(progress);
        app.drain_delivery();
        assert!(!app.sending());
        assert_eq!(app.unsent_ids(), ids);
    }

    #[test]
    fn outside_the_app_s_says_what_it_needs() {
        let (mut app, _) = app_with_comments();
        app.press_for_test(KeyCode::Char('S'), false);
        assert_eq!(
            app.status.as_deref(),
            Some("Sending comments needs the herdr app")
        );
        assert_eq!(app.unsent_count(), 0, "no count where `S` can't send");
    }

    #[test]
    fn the_picker_walks_the_agents_and_backs_out() {
        let (mut app, _) = app_with_comments();
        app.outbox.picker = Some(AgentPicker {
            agents: vec![agent("t2", None), agent("t3", None)],
            selected: 0,
        });
        assert!(app.keyboard_taken());
        for (key, want) in [
            (KeyCode::Char('j'), 1),
            (KeyCode::Down, 1),
            (KeyCode::Char('k'), 0),
            (KeyCode::Up, 0),
        ] {
            app.press_for_test(key, false);
            assert_eq!(app.agent_picker().unwrap().selected, want);
        }
        app.press_for_test(KeyCode::Esc, false);
        assert!(app.agent_picker().is_none());
        assert!(!app.quit, "Esc closed the picker, not riffnav");
        assert_eq!(app.status.as_deref(), Some("Not sent"));
    }
}

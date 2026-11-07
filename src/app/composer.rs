//! Writing and deleting comments: the `c` and `x` keys.

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use crate::comment::{Comment, Composer, PendingComment};

impl App {
    /// The rows the composer must open clear of, inside the diff pane —
    /// `(top, bottom)`, bottom exclusive, counted from the pane's top edge — or
    /// `None` when they're off-screen or nothing is rendered yet.
    ///
    /// Normally that's the line the note will hang on. Inside a thread it's the
    /// whole box, or the field would open through the note being replied to.
    pub fn composer_rows(&self) -> Option<(u16, u16)> {
        let render = self.current_render()?;
        let (first, past) = match self.block_at_cursor() {
            Some(block) => (block.start, block.start + block.len),
            None => (self.diff_cursor, self.diff_cursor + 1),
        };
        let top = render.row_of(first);
        let bottom = render.row_of(past).max(top + 1);
        let top = top.checked_sub(self.diff_scroll)?;
        let bottom = bottom.saturating_sub(self.diff_scroll);
        let height = u32::from(self.diff_height);
        (top < height).then_some((top as u16, bottom.min(height) as u16))
    }

    /// Start composing a note on whatever the cursor is over: a comment on the
    /// diff line, or a reply when the cursor is inside a thread (where `]` parks
    /// it).
    pub(super) fn start_comment(&mut self) {
        let Some(idx) = self.selected_file() else {
            self.set_status("No file selected to comment on");
            return;
        };
        // A reply inherits the thread's anchor, so it lands beside the same code.
        let in_thread = self.block_at_cursor().map(|block| {
            (
                block.anchor,
                block.comment_at(self.diff_cursor).map(str::to_string),
            )
        });
        let (anchor, reply_to) = match in_thread {
            Some(threaded) => threaded,
            None => match self
                .current_render()
                .and_then(|r| r.line_map.get(self.diff_cursor).anchor())
            {
                Some(anchor) => (anchor, None),
                None => {
                    let on_code = self.current_render().is_some_and(|render| {
                        render.line_map.sources(self.diff_cursor)[0].is_some()
                    });
                    self.set_status(if on_code {
                        "Only another parent of this merge had this line, so it has no number to comment on"
                    } else {
                        "Put the cursor on a diff line to comment on it"
                    });
                    return;
                }
            },
        };
        self.composer = Some(Composer::new(PendingComment {
            file: self.files[idx].path().to_string(),
            anchor,
            reply_to,
            diff_hash: self.file_hashes.get(idx).copied().unwrap_or(0),
            context: self.cursor_context(),
            draft: String::new(),
        }));
    }

    pub fn composer(&self) -> Option<&Composer> {
        self.composer.as_ref()
    }

    /// Store what's in the composer and close it. An empty body aborts.
    fn finish_comment(&mut self) {
        let Some(composer) = self.composer.take() else {
            return;
        };
        let body = composer.body();
        self.save_composed(composer.pending, body);
    }

    fn cancel_comment(&mut self) {
        if self.composer.take().is_some() {
            self.set_status("Comment discarded");
        }
    }

    /// `Ctrl-O` from the composer: reopen the same note in `$EDITOR`, carrying
    /// whatever has been typed so far. The editor runs once the event loop
    /// regains the terminal.
    fn comment_to_editor(&mut self) {
        if let Some(composer) = self.composer.take() {
            self.pending_comment = Some(composer.into_draft());
        }
    }

    /// The composer owns all input while it's up, so every key either edits the
    /// body or ends the note.
    pub(super) fn composer_key(&mut self, key: crossterm::event::KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            // A bare Enter posts the note; any modifier on it breaks the line
            // instead (Shift-Enter where the terminal reports it, Alt-Enter
            // everywhere else).
            KeyCode::Enter if !(shift || alt || ctrl) => self.finish_comment(),
            KeyCode::Char('s') if ctrl => self.finish_comment(),
            KeyCode::Esc => self.cancel_comment(),
            KeyCode::Char('c') if ctrl => self.cancel_comment(),
            KeyCode::Char('o') if ctrl => self.comment_to_editor(),
            _ => {
                let Some(c) = self.composer.as_mut() else {
                    return;
                };
                match key.code {
                    // Only a modified Enter reaches here; the bare one saved.
                    KeyCode::Enter => c.newline(),
                    KeyCode::Backspace => c.backspace(),
                    KeyCode::Delete => c.delete(),
                    KeyCode::Left => c.left(),
                    KeyCode::Right => c.right(),
                    KeyCode::Up => c.up(),
                    KeyCode::Down => c.down(),
                    KeyCode::Home => c.home(),
                    KeyCode::End => c.end(),
                    KeyCode::Char('a') if ctrl => c.home(),
                    KeyCode::Char('e') if ctrl => c.end(),
                    KeyCode::Char('u') if ctrl => c.delete_to_start(),
                    KeyCode::Char('w') if ctrl => c.delete_word(),
                    KeyCode::Char(ch) if !ctrl => c.insert(ch),
                    _ => {}
                }
            }
        }
    }

    /// The code on the rendered lines around the cursor, as plain text, to quote
    /// back in the editor so the author can see what they're commenting on.
    /// Headers, notes and comments in among them aren't code, so they're left out.
    fn cursor_context(&self) -> Vec<String> {
        let Some(render) = self.current_render() else {
            return Vec::new();
        };
        let first = self.diff_cursor.saturating_sub(2);
        let last = (self.diff_cursor + 3).min(render.text.lines.len());
        let code = |&at: &usize| render.line_map.sources(at).iter().any(Option::is_some);
        (first..last)
            .filter(code)
            .map(|at| {
                let spans = &render.text.lines[at].spans;
                let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
                text.trim_end().to_string()
            })
            .collect()
    }

    /// `x`: ask before deleting the comment the cursor is on, as the store is
    /// rewritten on the spot and nothing can bring it back. The next keypress
    /// answers. The CLI's `comment rm` doesn't ask: a script naming an id has
    /// already decided.
    pub(super) fn delete_comment(&mut self) {
        let Some(id) = self
            .block_at_cursor()
            .and_then(|b| b.comment_at(self.diff_cursor))
            .map(str::to_string)
        else {
            self.set_status("Put the cursor on a comment to delete it");
            return;
        };
        let doomed = self.comments.thread_ids(&id).len();
        // Deliberately not `set_status`: the question must stay up until it's
        // answered rather than time out under the cursor.
        self.status = Some(match doomed {
            0 | 1 => format!("Delete comment #{id}?  y/N"),
            n => format!("Delete comment #{id} and {} repl(ies)?  y/N", n - 1),
        });
        self.status_deadline = None;
        self.confirm_delete = Some(id);
    }

    /// The answer to that prompt: `y` deletes, anything else backs out.
    pub(super) fn answer_delete(&mut self, key: KeyCode) {
        let Some(id) = self.confirm_delete.take() else {
            return;
        };
        if !matches!(key, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.set_status("Delete cancelled");
            return;
        }
        let removed = self.comments.update(|store| store.remove(&id));
        self.bump_comments();
        self.set_status(match removed {
            Ok(0) => "Nothing to delete".to_string(),
            Ok(1) => format!("Deleted comment #{id}"),
            Ok(n) => format!("Deleted comment #{id} and {} repl(ies)", n - 1),
            Err(e) => format!("Couldn't delete comment #{id}: {e}"),
        });
    }

    /// Turn the text an author typed into a stored comment. Empty means abort.
    /// If the store refuses it, the composer reopens holding the text, so a
    /// failed save never costs what was typed.
    pub(super) fn save_composed(&mut self, pending: PendingComment, body: String) {
        let body = body.trim();
        if body.is_empty() {
            self.set_status("Empty comment — nothing saved");
            return;
        }
        let comment = Comment {
            id: String::new(),
            file: pending.file.clone(),
            side: pending.anchor.side,
            line: pending.anchor.line,
            body: body.to_string(),
            author: self.comment_author.clone(),
            created: crate::state::now_unix(),
            reply_to: pending.reply_to.clone(),
            diff_hash: Some(format!("{:032x}", pending.diff_hash)),
        };
        let saved = self.comments.update(|store| store.add(comment));
        self.bump_comments();
        match saved {
            Ok(id) => self.set_status(format!(
                "💬 Commented on {}:{}  (#{id})",
                pending.file, pending.anchor.line
            )),
            Err(e) => {
                self.composer = Some(Composer::new(PendingComment {
                    draft: body.to_string(),
                    ..pending
                }));
                // Not `set_status`: the reason stays up while the note does.
                self.status = Some(format!("Couldn't save the comment: {e}"));
                self.status_deadline = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::Direction;

    use crossterm::event::{KeyCode, KeyModifiers};

    use crate::app::test_support::{app_with, file_with_raw, seed_text};
    use crate::app::{App, Focus};
    use crate::comment::CommentStore;
    use crate::testing::TempDir;

    /// An app with comments on, that diff rendered, and the cursor on a
    /// commentable line — the state `c` is pressed from.
    fn app_ready_to_comment() -> App {
        let mut app = app_with(vec![file_with_raw("a.rs")]);
        app.install_comments_for_test(CommentStore::disabled());
        app.focus = Focus::Diff;
        app.diff_height = 20;
        app.seed_render_for_test(60, seed_text());
        app.resync_cursor();
        app
    }

    /// The `$EDITOR` buffer quotes the code around the cursor, and not the hunk
    /// header boxes or file header lines next to it.
    #[test]
    fn the_editor_quotes_only_code_around_the_cursor() {
        let raw = format!(
            "{}@@ -1,2 +1,2 @@\n one\n-two\n+2\n@@ -40,2 +40,2 @@ fn f()\n forty\n-x\n+y\n",
            crate::diff::testing::header("a.txt")
        );
        let mut app = App::new(
            crate::diff::parse(&raw),
            false,
            &crate::config::Config::default(),
        );
        app.install_comments_for_test(CommentStore::disabled());
        app.focus = Focus::Diff;
        crate::app::test_support::frame(&mut app);
        for first in [true, false] {
            let render = app.current_render().unwrap();
            let line = if first { 1 } else { 40 };
            let at = (0..render.lines())
                .find(|&at| render.line_map.get(at).new == Some(line))
                .unwrap();
            app.set_cursor(at);
            let context = app.cursor_context();
            assert!(!context.is_empty());
            for quoted in &context {
                assert!(quoted.contains('⋮'), "{quoted:?} in {context:?}");
            }
            let expect = if first { "one" } else { "forty" };
            assert!(context.iter().any(|q| q.ends_with(expect)), "{context:?}");
        }
    }

    fn type_into(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.press_for_test(KeyCode::Char(ch), false);
        }
    }

    /// A note is typed and saved without the screen being handed to `$EDITOR`.
    #[test]
    fn a_comment_is_typed_and_saved_in_place() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        assert!(app.composer.is_some(), "`c` opens the composer");

        type_into(&mut app, "no backoff");
        app.press_mods_for_test(KeyCode::Enter, KeyModifiers::SHIFT); // a second line
        type_into(&mut app, "here");
        assert!(app.composer.is_some(), "Shift-Enter must not end the note");

        app.press_for_test(KeyCode::Enter, false);
        assert!(app.composer.is_none());
        assert!(app.pending_comment.is_none(), "no editor was involved");
        let saved = app.comments.all();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].body, "no backoff\nhere");
        assert_eq!(saved[0].line, 1, "anchored to the cursor's line");
    }

    /// A save the store refuses hands the note back: the composer reopens with
    /// every line typed, the status says why, and a save that works later
    /// stores it. Covers both refusals: a store that won't parse, and a lock
    /// that can't be taken.
    #[test]
    fn a_failed_save_reopens_the_composer_with_the_text() {
        for broken in ["unparseable", "unlockable"] {
            let dir = TempDir::new("composer-fail");
            let path = dir.join("scope.json");
            match broken {
                "unparseable" => std::fs::write(&path, "{ half a store").unwrap(),
                // A directory where the lock file goes can't be opened to lock.
                _ => std::fs::create_dir(dir.join(".lock")).unwrap(),
            }
            let mut app = app_ready_to_comment();
            app.install_comments_for_test(CommentStore::with_path(path.clone()));

            app.press_for_test(KeyCode::Char('c'), false);
            type_into(&mut app, "first line");
            app.press_mods_for_test(KeyCode::Enter, KeyModifiers::SHIFT);
            type_into(&mut app, "second");
            app.press_for_test(KeyCode::Enter, false);

            let composer = app.composer.as_ref().expect("the note is handed back");
            assert_eq!(composer.body(), "first line\nsecond", "{broken}");
            assert_eq!(composer.pending.anchor.line, 1, "on the same line");
            let status = app.status.clone().unwrap_or_default();
            assert!(status.starts_with("Couldn't save the comment"), "{status}");
            assert!(app.comments.all().is_empty());

            // The cursor sits at the end, so typing carries on the note.
            type_into(&mut app, ", more");
            // Once the store is fixed, the same Enter stores it.
            match broken {
                "unparseable" => std::fs::remove_file(&path).unwrap(),
                _ => std::fs::remove_dir(dir.join(".lock")).unwrap(),
            }
            app.press_for_test(KeyCode::Enter, false);
            assert!(app.composer.is_none(), "{broken}");
            assert_eq!(app.comments.all()[0].body, "first line\nsecond, more");
        }
    }

    /// The same for a note written in `$EDITOR`: it comes back in the composer.
    #[test]
    fn a_failed_save_from_the_editor_keeps_the_text_too() {
        let dir = TempDir::new("composer-fail-editor");
        let path = dir.join("scope.json");
        std::fs::write(&path, "{ half a store").unwrap();
        let mut app = app_ready_to_comment();
        app.install_comments_for_test(CommentStore::with_path(path.clone()));
        app.press_for_test(KeyCode::Char('c'), false);
        app.press_for_test(KeyCode::Char('o'), true);
        let pending = app.pending_comment.take().expect("handed to the editor");

        app.save_composed(pending, "written in the editor".to_string());
        let composer = app.composer.as_ref().expect("the note is handed back");
        assert_eq!(composer.body(), "written in the editor");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ half a store");
    }

    /// A delete the store refuses deletes nothing, and says so.
    #[test]
    fn a_failed_delete_keeps_the_comment() {
        let dir = TempDir::new("composer-fail-delete");
        let path = dir.join("scope.json");
        let mut app = app_ready_to_comment();
        app.install_comments_for_test(CommentStore::with_path(path.clone()));
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "keep me");
        app.press_for_test(KeyCode::Enter, false);
        let id = app.comments.all()[0].id.clone();

        // The save left a lock file; a directory in its place can't be locked.
        std::fs::remove_file(dir.join(".lock")).unwrap();
        std::fs::create_dir(dir.join(".lock")).unwrap();
        app.confirm_delete = Some(id.clone());
        app.answer_delete(KeyCode::Char('y'));
        let status = app.status.clone().unwrap_or_default();
        assert!(status.starts_with("Couldn't delete"), "{status}");
        assert_eq!(app.comments.all().len(), 1, "still in memory");
        let mut fresh = CommentStore::with_path(path);
        fresh.reload();
        assert_eq!(fresh.get(&id).map(|c| c.body.as_str()), Some("keep me"));
    }

    /// Terminals that don't report Shift on Enter still need a way to break a
    /// line, so Alt-Enter — which survives the legacy encoding — does it too.
    #[test]
    fn alt_enter_breaks_the_line_where_shift_enter_cant_be_seen() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "one");
        app.press_mods_for_test(KeyCode::Enter, KeyModifiers::ALT);
        type_into(&mut app, "two");
        assert!(app.composer.is_some(), "Alt-Enter must not end the note");

        app.press_for_test(KeyCode::Enter, false);
        assert_eq!(app.comments.all()[0].body, "one\ntwo");
    }

    /// While a note is being typed the composer owns every key, so the ones that
    /// normally act on the panes type instead.
    #[test]
    fn the_composer_swallows_the_pane_keys() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "quit v?");

        assert!(!app.quit, "`q` types rather than quitting");
        assert!(!app.show_help, "`?` types rather than opening help");
        assert_eq!(
            app.viewed_count(),
            0,
            "`v` types rather than marking viewed"
        );
        app.press_for_test(KeyCode::Char('s'), true);
        assert_eq!(app.comments.all()[0].body, "quit v?");
    }

    #[test]
    fn escape_discards_the_note_and_saves_nothing() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "never mind");
        app.press_for_test(KeyCode::Esc, false);
        assert!(app.composer.is_none());
        assert!(app.comments.all().is_empty());
        assert!(!app.quit, "Esc closed the composer, not riffnav");

        // An empty note is an abort too.
        app.press_for_test(KeyCode::Char('c'), false);
        app.press_for_test(KeyCode::Char('s'), true);
        assert!(app.comments.all().is_empty());
    }

    /// On a diff line `c` starts a note, and inside a thread it replies to the
    /// comment it's sitting on.
    #[test]
    fn c_inside_a_thread_replies_to_the_comment_under_the_cursor() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "first");
        app.press_for_test(KeyCode::Char('s'), true);

        // Re-splice so the stored note occupies rows, then jump onto it.
        app.seed_render_for_test(60, seed_text());
        app.resync_cursor();
        app.jump_comment(Direction::Forward);
        assert!(
            app.current_render()
                .unwrap()
                .line_map
                .get(app.diff_cursor)
                .anchor()
                .is_none(),
            "the cursor is on a comment row, not a diff line"
        );

        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "second");
        app.press_for_test(KeyCode::Char('s'), true);
        let saved = app.comments.all();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[1].body, "second");
        assert_eq!(
            saved[1].line, saved[0].line,
            "a reply hangs on the same line as the thread"
        );
        assert_eq!(
            saved[1].reply_to.as_deref(),
            Some(saved[0].id.as_str()),
            "it threads under the comment the cursor was on"
        );

        // Back on a diff line, the same key starts a root note again.
        app.seed_render_for_test(60, seed_text());
        app.resync_cursor();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "third");
        app.press_for_test(KeyCode::Char('s'), true);
        assert!(app.comments.all()[2].reply_to.is_none());
    }

    /// An app with one saved note and the cursor parked on it — the state `x` is
    /// pressed from.
    fn app_on_a_saved_comment() -> App {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "why the retry here?");
        app.press_for_test(KeyCode::Enter, false);
        app.seed_render_for_test(60, seed_text()); // splice the note into rows
        app.resync_cursor();
        app.jump_comment(Direction::Forward);
        app
    }

    /// `x` asks first, and the answer is the next key.
    #[test]
    fn deleting_a_comment_asks_before_it_goes() {
        let mut app = app_on_a_saved_comment();

        app.press_for_test(KeyCode::Char('x'), false);
        assert_eq!(app.comments.all().len(), 1, "nothing is deleted yet");
        let prompt = app.status.clone().expect("the question is on screen");
        assert!(prompt.contains("Delete comment #"), "{prompt}");
        assert!(prompt.ends_with("y/N"), "{prompt}");
        assert!(
            app.status_deadline.is_none(),
            "the question must not time out under the cursor"
        );

        // Any key but `y` backs out — and `q` answers the question rather than
        // quitting riffnav.
        app.press_for_test(KeyCode::Char('q'), false);
        assert_eq!(app.comments.all().len(), 1, "the note survived");
        assert!(!app.quit, "`q` answered the prompt, it didn't quit");
        assert_eq!(app.status.as_deref(), Some("Delete cancelled"));

        app.press_for_test(KeyCode::Char('x'), false);
        app.press_for_test(KeyCode::Char('y'), false);
        assert!(app.comments.all().is_empty(), "confirmed, so it went");
        assert!(app.status.unwrap().starts_with("Deleted comment #"));
    }

    /// While the prompt is up the mouse leaves the panes alone, as the keys do.
    #[test]
    fn the_delete_prompt_holds_the_mouse_off_too() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = app_on_a_saved_comment();
        app.diff_area = Some(ratatui::layout::Rect::new(0, 0, 60, 10));
        let on = app.diff_cursor;
        app.press_for_test(KeyCode::Char('x'), false);
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.diff_cursor, on, "the click went nowhere");
        app.press_for_test(KeyCode::Char('y'), false);
        assert!(
            app.comments.all().is_empty(),
            "and `y` deleted the note it asked about"
        );
    }

    /// The prompt counts the replies that would go with the note, since deleting
    /// a thread's root takes the whole thread.
    #[test]
    fn the_delete_prompt_counts_the_replies_that_would_go_too() {
        let mut app = app_on_a_saved_comment();
        // `c` on a comment row replies to it.
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "because the API flakes");
        app.press_for_test(KeyCode::Enter, false);
        app.seed_render_for_test(60, seed_text());
        app.resync_cursor();
        app.jump_comment(Direction::Forward); // onto the thread's first row: its root

        app.press_for_test(KeyCode::Char('x'), false);
        let prompt = app.status.clone().expect("the question is on screen");
        assert!(prompt.contains("and 1 repl(ies)?"), "{prompt}");

        app.press_for_test(KeyCode::Char('y'), false);
        assert!(
            app.comments.all().is_empty(),
            "the reply went with its root"
        );
    }

    /// `Ctrl-O` is the escape hatch for a note that outgrows the field: it hands
    /// the same anchor — and the text so far — to the editor path.
    #[test]
    fn ctrl_o_hands_the_half_written_note_to_the_editor() {
        let mut app = app_ready_to_comment();
        app.press_for_test(KeyCode::Char('c'), false);
        type_into(&mut app, "half a thought");
        app.press_for_test(KeyCode::Char('o'), true);

        assert!(app.composer.is_none());
        let pending = app.pending_comment.as_ref().expect("handed to the editor");
        assert_eq!(pending.draft, "half a thought");
        assert_eq!(pending.anchor.line, 1);
        assert!(
            app.comments.all().is_empty(),
            "nothing is stored until it's saved"
        );
    }
}

//! Handing a file or a comment to `$EDITOR`.

use std::io::IsTerminal;
use std::process::{Command, Stdio};

use anyhow::Result;
use ratatui::DefaultTerminal;

use crate::comment::PendingComment;
use crate::diff::path;

use super::App;
use super::terminal::{
    disable_enhanced_keys, disable_mouse, enable_enhanced_keys, enable_mouse, init_terminal,
};

/// Separator between a comment body and the explanatory footer, borrowed from
/// git's `--verbose` scissors. Everything from this line down is discarded, which
/// leaves `#` usable inside the body itself.
const SCISSORS: &str = "# ------------------------ >8 ------------------------";

/// The buffer `$EDITOR` opens on: whatever was already typed in the composer (or
/// a blank line to type into), then the scissors and the context being
/// commented on.
fn comment_template(pending: &PendingComment) -> String {
    let mut out = if pending.draft.is_empty() {
        String::from("\n")
    } else {
        format!("{}\n", pending.draft)
    };
    out.push_str(SCISSORS);
    out.push('\n');
    out.push_str(&format!(
        "# Comment on {}:{} ({} side). Everything below the line above is\n\
         # ignored; save an empty comment to abort.\n#\n",
        pending.file,
        pending.anchor.line,
        pending.anchor.side.as_str(),
    ));
    for line in &pending.context {
        out.push_str("#   ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Everything above the scissors line, which is the comment the author typed.
fn strip_scissors(text: &str) -> String {
    text.split(SCISSORS).next().unwrap_or("").trim().to_string()
}

impl App {
    /// Suspend the TUI, run `$VISUAL`/`$EDITOR` on `path`, then resume.
    ///
    /// Returns the editor's name and how it exited, so callers can decide what to
    /// report — opening a file just says so, while composing a comment only reads
    /// the buffer back on success.
    fn run_editor(
        &mut self,
        terminal: &mut DefaultTerminal,
        path: &str,
    ) -> (String, std::io::Result<std::process::ExitStatus>) {
        disable_enhanced_keys();
        disable_mouse();
        ratatui::restore();
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_string());
        let mut cmd = Command::new(&editor);
        cmd.arg(path::to_os(path));
        // A piped diff (`git diff | riffnav`) leaves our stdin a spent pipe; the
        // editor would inherit it and complain that input isn't a terminal. The
        // TUI already talks to /dev/tty, so hand the editor the same.
        if !std::io::stdin().is_terminal()
            && let Ok(tty) = std::fs::File::open("/dev/tty")
        {
            cmd.stdin(Stdio::from(tty));
        }
        let status = cmd.status();

        *terminal = init_terminal();
        enable_mouse();
        enable_enhanced_keys();
        let _ = terminal.clear();
        self.last_width = 0; // force a re-render into the fresh screen
        (editor, status)
    }

    /// The `o` key: edit the selected file in place.
    pub(super) fn open_editor(&mut self, terminal: &mut DefaultTerminal, path: &str) -> Result<()> {
        let (editor, status) = self.run_editor(terminal, path);
        self.status = Some(match status {
            Ok(s) if s.success() => format!("Edited {path}"),
            Ok(s) => format!("{editor} exited: {s}"),
            Err(e) => format!("Couldn't launch {editor}: {e}"),
        });
        Ok(())
    }

    /// Compose a comment body in `$EDITOR`, git-commit style.
    ///
    /// The buffer opens with an empty first line and everything explanatory below
    /// a scissors marker, so a body containing `#` — a markdown heading, a shell
    /// snippet — survives verbatim. That's why this doesn't use git's plain `#`
    /// comment convention.
    pub(super) fn compose_comment(
        &mut self,
        terminal: &mut DefaultTerminal,
        pending: PendingComment,
    ) -> Result<()> {
        let path = std::env::temp_dir().join(format!("riffnav-comment-{}.md", std::process::id()));
        let template = comment_template(&pending);
        if let Err(e) = std::fs::write(&path, template) {
            self.set_status(format!("Couldn't open a comment buffer: {e}"));
            return Ok(());
        }

        let (editor, status) = self.run_editor(terminal, &path.to_string_lossy());
        match status {
            Ok(s) if s.success() => {
                let typed = std::fs::read_to_string(&path).unwrap_or_default();
                self.save_composed(pending, strip_scissors(&typed));
            }
            Ok(s) => self.set_status(format!("{editor} exited: {s} — comment discarded")),
            Err(e) => self.set_status(format!("Couldn't launch {editor}: {e}")),
        }
        let _ = std::fs::remove_file(&path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::{Anchor, PendingComment};

    /// The editor buffer must round-trip a body containing `#`, which is why it
    /// uses git's scissors convention rather than git's `#`-comment convention:
    /// review notes routinely contain markdown headings and shell snippets.
    #[test]
    fn the_editor_template_round_trips_a_body_containing_hashes() {
        let pending = PendingComment {
            file: "src/app.rs".to_string(),
            anchor: Anchor {
                side: crate::comment::Side::New,
                line: 103,
            },
            reply_to: None,
            diff_hash: 0,
            context: vec!["  103 │ let x = 1;".to_string()],
            draft: String::new(),
        };
        let template = comment_template(&pending);
        // The footer orients the author without becoming part of the note.
        assert!(template.contains("src/app.rs:103 (new side)"));
        assert!(template.contains("let x = 1;"));
        assert_eq!(strip_scissors(&template), "", "an untouched buffer aborts");

        let typed = format!("# A heading\n\nAnd a `#!/bin/sh` line.\n{template}");
        assert_eq!(
            strip_scissors(&typed),
            "# A heading\n\nAnd a `#!/bin/sh` line."
        );

        // Handing a half-typed note to the editor pre-fills the buffer with it,
        // so `Ctrl-O` continues the note rather than starting it over.
        let carried = comment_template(&PendingComment {
            draft: "half a thought".to_string(),
            ..pending
        });
        assert_eq!(strip_scissors(&carried), "half a thought");
    }
}

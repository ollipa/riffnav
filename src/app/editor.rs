//! Handing a file or a comment to `$EDITOR`.

use std::ffi::OsStr;
use std::hash::{BuildHasher, RandomState};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;
use ratatui::DefaultTerminal;

use super::App;
use super::terminal::{
    disable_enhanced_keys, disable_mouse, enable_enhanced_keys, enable_mouse, init_terminal,
};
use crate::comment::PendingComment;
use crate::diff::path;
use crate::git;

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

/// A directory of our own under the temp dir (`$TMPDIR` if set) for the comment
/// buffer, removed with everything in it when dropped: after the editor exits,
/// fails or can't start, and when a panic unwinds past it. The editor's swap
/// and backup files land in it too, so they go with it.
///
/// The temp dir is shared with every local user, so nothing in it is trusted.
/// The directory's name is random, so it can't be guessed ahead of time, and
/// creating it fails if anything, a symlink included, already has that name.
/// It's `0700`, so nobody else can plant or swap a file inside it.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        Self::new_in(&std::env::temp_dir())
    }

    fn new_in(parent: &Path) -> io::Result<Self> {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        // A clash with a random name means someone is squatting on names, or
        // the dice came up twice; either way a fresh name is the answer.
        let mut last = None;
        for _ in 0..16 {
            let dir = parent.join(format!("riffnav-{}", random_hex()));
            match builder.create(&dir) {
                Ok(()) => return Ok(Self(dir)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("no free scratch name")))
    }

    /// Create `name` inside, readable and writable only by us, holding `text`.
    fn write(&self, name: &str, text: &str) -> io::Result<PathBuf> {
        let path = self.0.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&path)?.write_all(text.as_bytes())?;
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 64 bits no other user can predict: `RandomState` hashes with keys the OS's
/// random source seeded, so its output is unguessable without them.
fn random_hex() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = RandomState::new().hash_one((std::process::id(), nanos));
    format!("{n:016x}")
}

/// Where the editor finds the diff's `path`. git names files from the top of the
/// repository, wherever riffnav was started, so a name is found there first. A
/// relative name is given a `./`, so one starting with `-` or `+` doesn't read as
/// an option or a line number.
fn editor_path(path: &str) -> PathBuf {
    let name = Path::new(path);
    if name.is_absolute() {
        return path::to_os(path).into();
    }
    if let Some(top) = git::toplevel(Path::new(".")) {
        let at_top = top.join(path::to_os(path));
        if at_top.exists() || !name.exists() {
            return at_top;
        }
    }
    Path::new(".").join(path::to_os(path))
}

/// `$VISUAL`, else `$EDITOR`, else `vi`. One that's set but empty is skipped.
fn chosen_editor() -> String {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|editor| !editor.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string())
}

/// The command that runs `editor` on `args`, the way git runs one: an editor
/// with spaces or shell characters in it, like `code --wait`, is a shell command,
/// run as `sh -c '<editor> "$@"'` so the arguments reach it untouched, and
/// anything else is the program itself. A path to a program that has spaces in
/// it is run as it is, so it needn't be quoted.
fn editor_command<'a>(editor: &str, args: impl IntoIterator<Item = &'a OsStr>) -> Command {
    const SHELL: &str = "|&;<>()$`\\\"' \t\n*?[#~=%";
    let plain = !editor.contains(|c| SHELL.contains(c)) || Path::new(editor).is_file();
    if plain {
        let mut cmd = Command::new(editor);
        cmd.args(args);
        return cmd;
    }
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!("{editor} \"$@\""))
        .arg(editor)
        .args(args);
    cmd
}

impl App {
    /// Suspend the TUI, run `$VISUAL`/`$EDITOR` on `path`, then resume. Returns the
    /// editor's name and how it exited, for the caller to report.
    fn run_editor(
        &mut self,
        terminal: &mut DefaultTerminal,
        path: &OsStr,
    ) -> (String, std::io::Result<std::process::ExitStatus>) {
        disable_enhanced_keys();
        disable_mouse();
        ratatui::restore();
        let editor = chosen_editor();
        let mut cmd = editor_command(&editor, [path]);
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
        let (editor, status) = self.run_editor(terminal, editor_path(path).as_os_str());
        self.set_status(match status {
            Ok(s) if s.success() => format!("Edited {path}"),
            Ok(s) => format!("{editor} exited: {s}"),
            Err(e) => format!("Couldn't launch {editor}: {e}"),
        });
        Ok(())
    }

    /// Compose a comment body in `$EDITOR`, with everything explanatory below
    /// [`SCISSORS`] so a body containing `#` survives verbatim.
    pub(super) fn compose_comment(
        &mut self,
        terminal: &mut DefaultTerminal,
        pending: PendingComment,
    ) -> Result<()> {
        let template = comment_template(&pending);
        // Dropped on the way out, whichever way that is, taking the buffer with it.
        let buffer = Scratch::new().and_then(|s| Ok((s.write("comment.md", &template)?, s)));
        let (path, _scratch) = match buffer {
            Ok(made) => made,
            Err(e) => {
                self.set_status(format!("Couldn't open a comment buffer: {e}"));
                return Ok(());
            }
        };

        let (editor, status) = self.run_editor(terminal, path.as_os_str());
        match status {
            Ok(s) if s.success() => {
                let typed = std::fs::read_to_string(&path).unwrap_or_default();
                self.save_composed(pending, strip_scissors(&typed));
            }
            Ok(s) => self.set_status(format!("{editor} exited: {s} — comment discarded")),
            Err(e) => self.set_status(format!("Couldn't launch {editor}: {e}")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the editor command hands over, for an editor `$EDITOR` names.
    fn run_with(editor: &str, args: &[&str]) -> String {
        let out = editor_command(editor, args.iter().map(OsStr::new))
            .output()
            .unwrap();
        assert!(out.status.success(), "{editor}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// An editor with arguments or shell syntax goes through the shell, and every
    /// argument, `+line` included, reaches it as one word.
    #[test]
    fn an_editor_with_arguments_gets_ours_untouched() {
        let args = ["+12", "a b", "$HOME", "-n"];
        assert_eq!(run_with("printf \"<%s>\"", &args), "<+12><a b><$HOME><-n>");
        assert_eq!(run_with("printf '[%s]'", &args), "[+12][a b][$HOME][-n]");
        assert_eq!(run_with("echo", &["+3", "x"]), "+3 x\n");

        // A program whose own path has a space in it, quoted or not.
        let dir = crate::testing::TempDir::new("editor dir");
        let program = dir.join("my ed");
        std::fs::write(&program, "#!/bin/sh\nprintf '{%s}' \"$@\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = program.to_str().unwrap();
        assert_eq!(run_with(path, &["+1", "f g"]), "{+1}{f g}");
        assert_eq!(run_with(&format!("'{path}' -x"), &["f"]), "{-x}{f}");
    }

    /// A name git gives from the top of the repository is opened there, and none
    /// reaches the editor looking like an option or a `+line`.
    #[test]
    fn the_editor_gets_a_path_it_cannot_misread() {
        let top = git::toplevel(Path::new(".")).expect("tests run in the repository");
        assert_eq!(editor_path("src/main.rs"), top.join("src/main.rs"));
        for name in ["-n", "+12", "--", "no/such/-file"] {
            let given = editor_path(name);
            assert!(given.is_absolute(), "{given:?}");
            assert!(given.ends_with(name), "{given:?}");
        }
        assert_eq!(editor_path("/etc/hosts"), Path::new("/etc/hosts"));
    }
    use crate::comment::{Anchor, PendingComment};

    /// The buffer goes in a fresh private directory under a name nobody can
    /// guess, and everything in that directory goes when it's dropped.
    #[cfg(unix)]
    #[test]
    fn the_scratch_buffer_is_private_and_cleaned_up() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = crate::testing::TempDir::new("scratch");
        let a = Scratch::new_in(&tmp).unwrap();
        let b = Scratch::new_in(&tmp).unwrap();
        assert_ne!(a.0, b.0, "every buffer gets a name of its own");

        let path = a.write("comment.md", "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&a.0), 0o700);
        assert_eq!(mode(&path), 0o600);
        // A second buffer of the same name isn't written through.
        assert!(a.write("comment.md", "again").is_err());

        // An editor's swap file goes with it.
        std::fs::write(a.0.join(".comment.md.swp"), "").unwrap();
        let dir = a.0.clone();
        drop(a);
        assert!(!dir.exists());

        // And on a panic, too.
        let dir = b.0.clone();
        let unwound = std::panic::catch_unwind(move || {
            let _held = b;
            panic!("the editor round trip blew up");
        });
        assert!(unwound.is_err());
        assert!(!dir.exists());
    }

    /// The editor buffer must round-trip a body containing `#`: review notes
    /// routinely contain markdown headings and shell snippets.
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

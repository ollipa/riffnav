use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::autodiff::DiffSource;
use crate::diff::FileDiff;

#[derive(Parser, Debug)]
#[command(
    name = "riffnav",
    version,
    about = "A git diff pager with a file tree, powered by delta"
)]
pub struct Cli {
    /// What to review, and the CLI tools alongside it. Absent when a diff is
    /// piped in, which keeps `riffnav` usable as git's pager exactly as before.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Start in side-by-side view (default follows your delta config).
    #[arg(short = 's', long, global = true, conflicts_with = "unified")]
    pub side_by_side: bool,

    /// Start in unified view (default follows your delta config).
    #[arg(short = 'u', long, global = true)]
    pub unified: bool,

    /// Use a specific config file instead of the default XDG location.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Print the parsed file list and exit (debug; no TUI).
    #[arg(long, global = true, hide = true)]
    pub list: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Review changes from git, like `git diff`
    ///
    /// Takes the same arguments as `git diff`, so `riffnav diff` can stand in
    /// for it. With no flags it shows unstaged work, as `git diff` does, plus
    /// the untracked files git leaves out.
    Diff(DiffArgs),

    /// Review a commit, like `git show`
    ///
    /// Takes the same arguments as `git show` (HEAD by default), so
    /// `riffnav show` can stand in for it.
    Show {
        /// Arguments passed straight through to `git show`
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "GIT_ARGS"
        )]
        args: Vec<String>,
    },
    /// Read and write inline review comments on the current repo and branch.
    #[command(subcommand)]
    Comment(CommentCmd),
    /// Print the agent skill describing these commands, for a coding agent to
    /// load. `--path` prints where it was written instead of its contents.
    Skill {
        /// Write the skill to a file and print its path.
        #[arg(long)]
        path: bool,
    },
}

/// `riffnav diff`: a built-in view, chosen by flag, plus anything else the user
/// wants to hand to `git diff`.
#[derive(Args, Debug)]
pub struct DiffArgs {
    /// Everything since the branch forked: its commits plus all uncommitted work
    ///
    /// Committed, staged, unstaged and untracked, measured against the fork point
    /// — the whole of what the branch has done, whether or not you've committed
    /// it yet.
    #[arg(long, group = "view")]
    pub all: bool,

    /// All uncommitted work: staged and unstaged vs HEAD, plus untracked files
    #[arg(long, group = "view")]
    pub all_uncommitted: bool,

    /// What the branch adds over its base (`<base>...HEAD`) — the PR view
    #[arg(long, group = "view")]
    pub vs_base: bool,

    /// Staged changes only
    #[arg(long, group = "view")]
    pub staged: bool,

    /// Unstaged working-tree changes
    ///
    /// The default, so this is only worth spelling out to override a
    /// `diff_source` set in the config file.
    #[arg(long, group = "view")]
    pub unstaged: bool,

    /// Base branch for --all and --vs-base [default: detected]
    ///
    /// Detection picks origin/HEAD or a local main/master, whichever forks off
    /// the current branch later.
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// The pre-1.1 name for `--vs-base`. Declared only so it can be turned into
    /// an error that names its replacement: without it the flag would fall
    /// through to git as a pass-through argument, and the user would get
    /// `error: invalid option: --committed` from a program they didn't call.
    #[arg(long, hide = true)]
    pub committed: bool,

    /// Arguments passed straight through to `git diff`
    ///
    /// A revision, a pathspec after `--`, `-w`, and so on. Any of these turn off
    /// the parts of riffnav that assume a plain view: cycling views with `d`,
    /// folding in untracked files, and the `diff_source` config default.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "GIT_ARGS"
    )]
    pub args: Vec<String>,
}

impl DiffArgs {
    /// The view the flags select, or `None` to fall back to the `diff_source`
    /// config key (and then to unstaged, matching `git diff`).
    pub fn view(&self) -> Option<DiffSource> {
        [
            (self.all, DiffSource::All),
            (self.all_uncommitted, DiffSource::AllUncommitted),
            (self.staged, DiffSource::Staged),
            (self.unstaged, DiffSource::Unstaged),
            (self.vs_base, DiffSource::VsBase),
        ]
        .into_iter()
        .find_map(|(given, source)| given.then_some(source))
    }

    /// The renamed flag the user typed, if any, and what to type instead. `--all`
    /// isn't here: it still parses, having been reused for the wider view.
    pub fn renamed_flag(&self) -> Option<(&'static str, &'static str)> {
        self.committed.then_some(("--committed", "--vs-base"))
    }
}

/// Give back a `--` that clap ate, so a pathspec stays a pathspec.
///
/// clap reads a `--` sitting directly after the subcommand as its own value
/// escape and drops it, which would turn `riffnav diff -- main` into
/// `git diff main` — a *revision*, not the file called `main`. A `--` anywhere
/// later survives, so comparing how many the user typed against how many
/// arrived says whether one needs putting back.
pub fn git_args(args: Vec<String>) -> Vec<String> {
    restore_escape(args, std::env::args())
}

fn restore_escape(mut args: Vec<String>, raw: impl IntoIterator<Item = String>) -> Vec<String> {
    let typed = raw.into_iter().filter(|a| a == "--").count();
    let kept = args.iter().filter(|a| *a == "--").count();
    if typed > kept {
        args.insert(0, "--".to_string());
    }
    args
}

#[derive(Subcommand, Debug)]
pub enum CommentCmd {
    /// Leave one comment on a line of a file's diff.
    Add(AddArgs),
    /// Apply a JSON batch of comments read from stdin. The whole batch is
    /// validated before any of it is written, so a typo can't half-apply.
    Apply {
        /// Read the batch from stdin (required, and the only input source).
        #[arg(long)]
        stdin: bool,
    },
    /// List existing comments.
    List {
        /// Only comments on this file.
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// Only comments by this author.
        #[arg(long, value_name = "NAME")]
        author: Option<String>,
        /// Emit JSON instead of a human-readable listing.
        #[arg(long)]
        json: bool,
    },
    /// Delete one comment and any replies beneath it.
    Rm {
        /// The short id shown by `comment list` (and beside each note in the UI).
        id: String,
    },
    /// Delete many comments at once.
    Clear {
        /// Limit the deletion to one file.
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
        /// Required: deleting comments can't be undone.
        #[arg(long)]
        yes: bool,
    },
    /// Print the files and hunks a comment can be anchored to — start here, it's
    /// far smaller than the diff itself.
    Context {
        /// Emit JSON instead of a human-readable summary.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
pub struct AddArgs {
    /// File to comment on, as it appears in the diff. Omitted for a reply.
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,
    /// Line number on the post-image (added/context) side.
    #[arg(long, value_name = "N", conflicts_with = "old_line")]
    pub new_line: Option<u32>,
    /// Line number on the pre-image (removed/context) side.
    #[arg(long, value_name = "N")]
    pub old_line: Option<u32>,
    /// The comment text. Pass `-` to read it from stdin.
    #[arg(long, value_name = "TEXT")]
    pub body: String,
    /// Name to record as the author [default: $USER].
    #[arg(long, value_name = "NAME")]
    pub author: Option<String>,
    /// Thread this comment under an existing one, by id.
    ///
    /// A reply takes no anchor of its own: it inherits the file and line of the
    /// comment it answers, so `--file` and the line flags must be left off.
    #[arg(long, value_name = "ID")]
    pub reply_to: Option<String>,
}

/// `--list` debug output: the parsed files with status and ± counts.
pub fn print_list(files: &[FileDiff]) {
    println!("{} file(s):", files.len());
    for f in files {
        println!(
            "  {} {:<48} +{} -{}",
            f.status.sigil(),
            f.path(),
            f.additions,
            f.deletions
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn parsed(line: &str) -> Vec<String> {
        match Cli::parse_from(line.split_whitespace()).command {
            Some(Command::Diff(args)) => restore_escape(args.args, argv(line)),
            Some(Command::Show { args }) => restore_escape(args, argv(line)),
            other => panic!("expected a diff/show command, got {other:?}"),
        }
    }

    #[test]
    fn the_cli_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn a_leading_pathspec_escape_survives_clap() {
        // clap eats the `--` here, so it has to be put back: without it git reads
        // `feat` as a revision, not as the file of that name.
        assert_eq!(parsed("riffnav diff -- feat"), ["--", "feat"]);
        assert_eq!(parsed("riffnav show -- feat"), ["--", "feat"]);
        assert_eq!(parsed("riffnav diff --staged -- a b"), ["--", "a", "b"]);
    }

    #[test]
    fn a_later_pathspec_escape_is_left_alone() {
        // clap only eats the first `--`, and only when nothing precedes it — one
        // after a revision arrives intact and must not be doubled.
        assert_eq!(parsed("riffnav diff HEAD -- a"), ["HEAD", "--", "a"]);
        assert_eq!(parsed("riffnav diff -w -- a"), ["-w", "--", "a"]);
        assert_eq!(parsed("riffnav diff HEAD~3"), ["HEAD~3"]);
        assert_eq!(parsed("riffnav diff"), Vec::<String>::new());
    }

    #[test]
    fn view_flags_map_to_diff_sources() {
        let view = |line: &str| match Cli::parse_from(line.split_whitespace()).command {
            Some(Command::Diff(args)) => args.view(),
            _ => panic!("expected a diff command"),
        };
        assert_eq!(view("riffnav diff --all"), Some(DiffSource::All));
        assert_eq!(
            view("riffnav diff --all-uncommitted"),
            Some(DiffSource::AllUncommitted)
        );
        assert_eq!(view("riffnav diff --vs-base"), Some(DiffSource::VsBase));
        assert_eq!(view("riffnav diff --staged"), Some(DiffSource::Staged));
        assert_eq!(view("riffnav diff --unstaged"), Some(DiffSource::Unstaged));
        // No flag defers to the config file, and then to unstaged.
        assert_eq!(view("riffnav diff"), None);
    }

    #[test]
    fn view_flags_are_mutually_exclusive() {
        assert!(Cli::try_parse_from(["riffnav", "diff", "--all", "--staged"]).is_err());
    }

    #[test]
    fn riffnavs_own_flags_work_on_either_side_of_the_subcommand() {
        assert!(Cli::parse_from(["riffnav", "-s", "diff"]).side_by_side);
        assert!(Cli::parse_from(["riffnav", "diff", "-s"]).side_by_side);
    }
}

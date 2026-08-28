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
    /// Subcommands for reading and writing inline review comments, meant for AI
    /// agents. Absent for a normal launch, which keeps `riffnav` usable as
    /// git's pager exactly as before.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Start in side-by-side view (default follows your delta config).
    #[arg(short = 's', long, conflicts_with = "unified")]
    pub side_by_side: bool,

    /// Start in unified view (default follows your delta config).
    #[arg(short = 'u', long)]
    pub unified: bool,

    /// Use a specific config file instead of the default XDG location.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Watch for changes and refresh automatically. The diff is produced by
    /// --watch-cmd (stdin is ignored in this mode).
    #[arg(short = 'w', long)]
    pub watch: bool,

    /// Command that produces the diff in watch mode [default: "git diff"].
    #[arg(long, value_name = "CMD")]
    pub watch_cmd: Option<String>,

    /// Seconds between periodic watch refreshes [default: 2].
    #[arg(long, value_name = "SECS")]
    pub watch_interval: Option<f64>,

    /// On a bare launch (no piped diff), which diff to show: all (uncommitted) |
    /// committed (branch vs base) | staged | unstaged. Omit for the adaptive
    /// default — uncommitted changes, or branch-vs-base when the tree is clean.
    #[arg(long, value_name = "SOURCE")]
    pub diff: Option<DiffSource>,

    /// Base branch for the branch-vs-base view on a bare launch. Omit to detect
    /// it (origin/HEAD or a local main/master, whichever forks off later).
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Print the parsed file list and exit (debug; no TUI).
    #[arg(long, hide = true)]
    pub list: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
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
    /// File to comment on, as it appears in the diff.
    #[arg(long, value_name = "PATH")]
    pub file: String,
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

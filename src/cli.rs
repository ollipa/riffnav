use std::path::PathBuf;

use clap::Parser;

use crate::autodiff::DiffSource;
use crate::diff::FileDiff;

#[derive(Parser, Debug)]
#[command(
    name = "riffnav",
    version,
    about = "A git diff pager with a file tree, powered by delta"
)]
pub struct Cli {
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
    /// it (origin/HEAD, else a local main/master).
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,

    /// Print the parsed file list and exit (debug; no TUI).
    #[arg(long, hide = true)]
    pub list: bool,
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

# riffnav

A git diff pager with a GitHub-style file tree, powered by [delta][delta].

`riffnav` reads a unified diff on stdin, renders each file with `delta`, and wraps
it in a terminal UI: a navigable file tree on the left, the rendered diff on the
right. It's a Rust take on [diffnav][diffnav].

![riffnav demo](demo.gif)

> **Built with AI.** riffnav — its code, tests, and docs — was written with AI
> assistance (Claude Code).

```text
 riffnav   3 files   +55  -5    unified
v src/                         │ src/diff/parser.rs
  v diff/                      │  @@ -0,0 +1,40 @@
    A parser.rs          +40 -0│  +use crate::diff::FileDiff;
  M main.rs              +12 -4│  +
M README.md               +3 -1│  +pub fn parse(input: &str) -> Vec<FileDiff> {
                               │   …
 j/k · n/p file · t: find · Tab focus · ?: help · q: quit
```

## Requirements

- **[delta][delta]** on your `PATH` — riffnav renders diffs with it.
- A **[Nerd Font][nerdfonts]** for filetype icons (optional). No Nerd Font? Press
  `i` to cycle to `unicode` or `ascii` icons, or set `icon_style` in the config.

## Install

With the Rust toolchain (`cargo`):

```sh
# From a local checkout:
cargo install --path .

# Or straight from the repository:
cargo install --git https://github.com/ollipa/riffnav
```

This puts the `riffnav` binary in `~/.cargo/bin` (make sure that's on your `PATH`).

## Usage

Pipe any unified diff into it:

```sh
git diff | riffnav
git diff HEAD~3 | riffnav
git show <commit> | riffnav
```

### Use it as git's pager

```sh
git config --global pager.diff riffnav
git config --global pager.show riffnav
```

Now `git diff` and `git show` open in riffnav. (Setting `core.pager` also works,
but scoping to `diff`/`show` avoids sending `git log` through it.)

By default riffnav follows your `delta.side-by-side` git setting; force a layout
for one run with `-s` (side-by-side) or `-u` (unified).

## Keybindings

| Key | Action |
|-----|--------|
| `j` / `k` (or `↑` / `↓`) | Move selection (tree) / scroll (diff), per focus |
| `n` / `p` (or `N`) | Next / previous file |
| `Ctrl-d` / `Ctrl-u` | Scroll diff half a page |
| `g` / `G` | Top / bottom of the diff |
| `Enter` / `Space` | Expand / collapse the selected folder |
| `Tab` | Switch focus between tree and diff |
| `t` / `/` | Fuzzy-find a file |
| `s` | Toggle side-by-side / unified |
| `e` | Toggle the file tree |
| `i` | Cycle icon style (nerd → unicode → ascii) |
| `y` | Copy the selected file's path |
| `o` | Open the selected file in `$EDITOR` |
| `?` | Toggle the help overlay |
| `q` / `Esc` / `Ctrl-c` | Quit |

## Configuration

riffnav reads `$XDG_CONFIG_HOME/riffnav/config.toml` (or
`~/.config/riffnav/config.toml`); override with `--config <FILE>`. Every key is
optional. Settings resolve as **defaults < config file < CLI flags**.

```toml
# ~/.config/riffnav/config.toml
# side_by_side = false   # omit to follow your delta.side-by-side default
icon_style   = "nerd"    # nerd | unicode | ascii
tree_width   = 32        # columns for the file-tree pane
show_tree    = true
show_header  = true
show_footer  = true
open_depth   = 64        # expand folders shallower than this on launch
```

See [`config.example.toml`](config.example.toml) for the annotated version.

## Watch mode

`-w` / `--watch` keeps riffnav open and refreshes when your working tree changes —
handy on a second monitor while you edit.

```sh
riffnav --watch                       # re-runs `git diff` on change
riffnav --watch --watch-cmd "git diff --staged"
riffnav --watch --watch-interval 1    # also poll every second
```

In watch mode the diff is produced by `--watch-cmd` (default `git diff`), not
stdin. Changes are detected by a filesystem watcher (debounced) plus the polling
interval as a safety net; the view only rebuilds when the diff actually changes,
and your selected file is preserved across refreshes.

## How it works

stdin → split per file (`diff --git`) → build the tree → on selection, run the
file's hunk through `delta` (cached per file/width/layout) and convert its ANSI
output to styled text with [ansi-to-tui][ansi-to-tui], drawn with
[ratatui][ratatui]. Because stdin is the diff, key input is read from `/dev/tty`.

## License

MIT

[delta]: https://github.com/dandavison/delta
[diffnav]: https://github.com/dlvhdr/diffnav
[nerdfonts]: https://www.nerdfonts.com/
[ratatui]: https://ratatui.rs/
[ansi-to-tui]: https://github.com/ratatui/ansi-to-tui

# riffnav

A git diff pager with a GitHub-style file tree, powered by [delta][delta].

> 🤖 **Built with AI.** riffnav — its code, tests, and docs — was written with AI assistance

`riffnav` reads a unified diff on stdin, renders each file with `delta`, and wraps
it in a terminal UI: a navigable file tree on the left, the rendered diff on the
right. It's a Rust take on [diffnav][diffnav].

![riffnav demo](demo.gif)

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

Use `riffnav diff` and `riffnav show` wherever you'd type `git diff` and
`git show` — same arguments, rendered in the TUI:

```sh
riffnav diff                  # unstaged changes, like `git diff`
riffnav diff --staged         # staged changes
riffnav diff HEAD~3           # any revision(s) git accepts
riffnav diff -- src/          # scoped to a pathspec
riffnav show                  # the last commit, like `git show`
riffnav show <commit>
```

See [Diff views](#diff-views) for the flags that pick a view, and how the
argument pass-through works.

Or pipe any unified diff in, which is what makes riffnav usable as git's pager:

```sh
git diff | riffnav
git show <commit> | riffnav
```

### Use it as git's pager

```sh
git config --global pager.diff riffnav
git config --global pager.show riffnav
```

Now `git diff` and `git show` open in riffnav. (Setting `core.pager` also works,
but scoping to `diff`/`show` avoids sending `git log` through it.) This is
independent of `riffnav diff` — riffnav disables the pager for its own git calls,
so the two can't loop.

By default riffnav follows your `delta.side-by-side` git setting; force a layout
for one run with `-s` (side-by-side) or `-u` (unified).

## Keybindings

| Key | Action |
|-----|--------|
| `j` / `k` (or `↑` / `↓`) | Move selection (tree) / move the line cursor (diff), per focus |
| `n` / `p` (or `N`) | Next / previous file |
| `Ctrl-d` / `Ctrl-u` | Scroll diff half a page |
| `PgDn` / `PgUp` | Page down / up (scroll diff or move tree, per focus) |
| `g` / `G` | Top / bottom of the diff |
| `Enter` / `Space` | Expand / collapse the selected folder |
| `Tab` | Switch focus between tree and diff |
| `t` / `/` | Fuzzy-find a file |
| `s` | Toggle side-by-side / unified |
| `e` | Toggle the file tree |
| `i` | Cycle icon style (nerd → unicode → ascii) |
| `T` | Cycle diff theme (delta → github-dark → github-light) |
| `y` | Copy the selected file's path |
| `v` / `V` | Mark the file viewed / jump to the next unviewed file |
| `d` | Cycle the diff view — uncommitted → staged → unstaged → branch-vs-base (only for a plain [`riffnav diff`](#diff-views)) |
| `r` | Re-read the diff, picking up changes made since (not for a diff piped in) |
| `o` | Open the selected file in `$EDITOR` |
| `c` | [Comment](#review-comments) on the cursor's line, or reply when it's inside a thread |
| `x` | Delete the comment under the cursor, and its replies |
| `]` / `[` | Jump to the next / previous comment |
| `z` | Toggle zoom on riffnav's pane (only inside [herdr](#herdr-integration)) |
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
diff_theme   = "github-dark" # github-dark | github-light | delta (inherit gitconfig)
tree_width   = 32        # columns for the file-tree pane
show_tree    = true
start_focus  = "diff"    # "diff": open in the first file (n/p between files) | "tree"
show_header  = true
show_footer  = true
open_depth   = 64        # expand folders shallower than this on launch
review_retention_days = 90 # days to keep "viewed" marks before GC
review_auto_advance = true # jump to next unviewed file after marking viewed
review_sync_github = false # push "viewed" marks to the matching GitHub PR (needs `gh`)
# base_branch = "main"     # base for "branch vs base"; omit to auto-detect
# diff_source = "all"      # default view for `riffnav diff`: all|committed|staged|unstaged
```

See [`config.example.toml`](config.example.toml) for the annotated version.

## Reviewing changes

Press `v` to mark the selected file **viewed** — it gets a green `✓` and dims in
the tree — and `V` to jump to the next unviewed file. Marking viewed also
advances to the next unviewed file by default (`review_auto_advance`), so review
flows file-to-file. The header shows your progress (`✓ 3/8 viewed`).

Viewed marks persist across runs, scoped per repository **and** branch (like
GitHub's per-PR "Viewed" checkbox), and are keyed on the *content* of each
change: edit a file you'd marked viewed and it reverts to unviewed automatically,
just as GitHub un-ticks a file the author pushes to. State lives under
`$XDG_STATE_HOME/riffnav/viewed/` and is garbage-collected by age
(`review_retention_days`, default 90). Outside a git repo (e.g. an arbitrary diff
piped in) marking still works for the session but isn't persisted.

## Review comments

Leave notes on a line of the diff, and read notes an AI agent left for you.

Press `c` to comment on the line under the cursor. A field opens right where the
note will live — type into it, `Enter` to save, `Esc` to discard. It takes the
usual editing keys (`Shift-Enter` for a new line, `Ctrl-W` / `Ctrl-U` to rub out
a word or the line, `Ctrl-A` / `Ctrl-E` for its ends), and `Ctrl-O` moves what
you've typed into `$EDITOR` when a note outgrows the field — a git-commit-style
buffer, where you type above the scissors line and save empty to abort.

`Shift-Enter` needs a terminal that speaks the kitty keyboard protocol (kitty,
ghostty, WezTerm, foot, recent Alacritty); elsewhere use `Alt-Enter` for a new
line. `Ctrl-S` still saves in either case.

Press `c` inside an existing thread — where `]` / `[` leave the cursor — and it
replies to the comment you're on instead; there's no separate reply key. `x`
deletes the comment under the cursor along with the replies beneath it. Saved
notes are drawn as a box under the line they annotate, with each reply on a
divider inside it. Files carrying comments show a `💬` count in the tree.

Your own name — `comment_author`, or `$USER` — is drawn in the box's own color;
every other author gets one picked from a palette by hashing the name, so an
agent (or a second reviewer) keeps the same color everywhere and a thread can be
read by who is speaking.

### Letting an agent comment

The `riffnav comment` subcommands are the agent-facing half. They're ordinary
non-interactive commands that read and write the same store the running window
watches — so a note written in another terminal appears on screen within a
moment, and works just as well with no window open.

```sh
riffnav comment context                # files and the line ranges you can anchor to
riffnav comment add --file src/app.rs --new-line 103 --body "No backoff here."
riffnav comment list --json            # read back what the user wrote
```

Point your agent at the bundled skill, which teaches it the above:

```sh
riffnav skill            # print it
riffnav skill --path     # write it out and print the path
```

Anchors are validated before anything is written: naming a file that isn't in
the diff, or a line outside every hunk, fails with the ranges that *would* have
worked. Several notes at once go through one batch, which is validated whole so
a typo can't half-apply:

```sh
printf '%s' '{"comments":[
  {"file":"src/app.rs","newLine":103,"body":"No backoff here."},
  {"file":"README.md","newLine":12,"body":"Stale: the flag is now --diff."}
]}' | riffnav comment apply --stdin
```

There's no daemon and no port — just a JSON file under
`$XDG_STATE_HOME/riffnav/comments/`, scoped per repository and branch like
viewed marks, and garbage-collected by age (`comment_retention_days`). A comment
whose code has changed since it was written is flagged rather than silently
sliding onto a different line.

## Diff views

`riffnav diff` shows unstaged work by default, like `git diff`. Flags pick one
of the other views:

```sh
riffnav diff               # unstaged — `git diff`, plus untracked files
riffnav diff --staged      # staged — `git diff --staged`
riffnav diff --all         # all uncommitted: staged + unstaged + untracked
riffnav diff --committed   # branch vs base — `git diff <base>...HEAD`, the PR view
riffnav diff --base develop --committed   # against a specific base branch
```

The two working-tree views (`riffnav diff` and `--all`) also list untracked,
non-ignored files, rendered as fully added — `git diff` omits them by design,
which would leave a brand-new file invisible until you staged it. That's the one
way they differ from the git command they shadow.

Press `d` to cycle between those four views without restarting, and `r` to
re-read the diff — commits, stages, and edits made since you opened riffnav show
up without losing your place in the file you're reading.

The base branch for `--committed` is auto-detected from `origin/HEAD` and a local
`main`/`master`, picking whichever branched off your current branch more
recently — so commits you already merged into a local `main` aren't counted as
your branch's work. Set it explicitly with `--base <ref>` or the `base_branch`
config key, and change the default view with the `diff_source` config key.

### Passing arguments through to git

Anything else you write after `riffnav diff` or `riffnav show` goes to git
verbatim, so the commands stand in for their git counterparts:

```sh
riffnav diff main...HEAD
riffnav diff -w -- src/
riffnav show HEAD~2 -- src/
```

Three things switch off once you pass your own arguments, because riffnav can no
longer tell a revision from a pathspec: the `d` view cycle (there is nothing to
cycle through), folding untracked files in, and a `diff_source` set in the config
file (your revision is the one that counts). `r` still refreshes.

riffnav needs a real unified diff to build its tree from, so arguments that
suppress one — `--stat`, `--name-only`, `--summary` and friends — leave it with
nothing to show.

## herdr integration

When riffnav runs inside [herdr](https://herdr.dev) (detected via `HERDR_ENV=1`),
the `z` key toggles **zoom** on riffnav's pane — maximizing it to fill the window,
or restoring it. riffnav talks to herdr's [socket API][herdr-socket] over its Unix
control socket (found via `HERDR_SOCKET_PATH` / `HERDR_SESSION`, or the default
session socket). Outside herdr the key does nothing and isn't shown in the footer
or help.

## How it works

A unified diff — from `git diff`/`git show` run by riffnav, or piped in on stdin
— is split per file (`diff --git`) into a tree; on selection the file's hunks go
through `delta` (cached per file/width/layout) and its ANSI output is converted
to styled text with [ansi-to-tui][ansi-to-tui], drawn with [ratatui][ratatui].
Because stdin may itself be the diff, key input is read from `/dev/tty`.

Comments anchor to a *diff line*, never a screen row, so they stay put across a
resize, a theme switch, or a unified/side-by-side toggle. riffnav recovers those
line numbers by pinning delta's line-number gutter to a fixed-width format and
reading it back out of the rendered output — which is why line numbers are always
on.

## License

MIT

[delta]: https://github.com/dandavison/delta
[diffnav]: https://github.com/dlvhdr/diffnav
[nerdfonts]: https://www.nerdfonts.com/
[ratatui]: https://ratatui.rs/
[ansi-to-tui]: https://github.com/ratatui/ansi-to-tui
[herdr-socket]: https://herdr.dev/docs/socket-api/

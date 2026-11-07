---
name: riffnav-review
description: Leave and read inline review comments on a git diff through the riffnav CLI. Comments are anchored to a file and line, persist on disk, and appear live in the user's riffnav window. Use when reviewing a branch or changeset the user is reading in riffnav, or when the user asks you to comment on code.
---

# riffnav review comments

riffnav is a terminal diff viewer the user drives. Never run `riffnav diff` or
`riffnav show`: they take over the terminal. Use `riffnav comment ...`, which
are plain non-interactive commands.

Comments are kept per repo and branch under `$XDG_STATE_HOME/riffnav/comments/`,
so they work whether or not a window is open. An open window shows new ones
within a moment.

## Workflow

```bash
riffnav comment context            # 1. files and line ranges you can comment on
riffnav comment list               # 2. what's already been said
riffnav comment apply --stdin      # 3. your notes, in one batch
```

Read the code with your usual tools (`git diff`, Read). The CLI handles
comments, not the diff.

## Commands

```bash
riffnav comment context [--json]
riffnav comment list [--file <path>] [--author <name>] [--json]
riffnav comment add --file <path> (--new-line <n> | --old-line <n>) --body <text> --author <name>
riffnav comment add --reply-to <id> --body <text> --author <name>
riffnav comment apply --stdin
riffnav comment rm <id>
riffnav comment clear [--file <path>] --yes
```

`--body -` reads the text from stdin, for long or heavily quoted text.

**Always sign with `--author`** (`"author"` in a batch), e.g. `claude`.
Without it the note is filed under `$USER` and looks like the user wrote it.

## Anchors

Each comment names one file and one line:

- `--new-line <n>`: post-image side (added or context lines). The usual choice.
- `--old-line <n>`: pre-image side (removed or context lines), for deleted code.

Numbers are git's 1-based hunk line numbers, the same ones in riffnav's gutter.
On a merge the old side is the first parent.

Only lines inside the ranges `context` prints can take a comment.

**Editing files moves the diff.** After you change code, earlier line numbers
are stale, and the diff riffnav shows can switch on its own (a clean tree shows
the branch against its base; the first edit widens it to everything since the
fork). Re-run `riffnav comment context` before commenting again.

**If an anchor is rejected, stop and report it.** Never move the comment to a
nearby line the CLI accepts: it would be filed against code it isn't about.

## Batches

Prefer one `apply` over many `add`s. The whole batch is checked before anything
is written, so one bad line fails it cleanly.

```bash
printf '%s' '{"comments":[
  {"file":"src/app.rs","newLine":103,"author":"claude","body":"This retry loop has no backoff."},
  {"file":"src/app.rs","oldLine":88,"author":"claude","body":"Why was the guard here dropped?"},
  {"replyTo":"a3f1c2","author":"claude","body":"Fixed: added exponential backoff."}
]}' | riffnav comment apply --stdin
```

Each item has `body`, `author`, and either an anchor (`file` plus one of
`newLine`/`oldLine`) or `replyTo`, never both. Fields are camelCase. `apply`
takes no flag but `--stdin`.

## Replies

`comment list` shows each comment's short id. `--reply-to <id>` threads under it.
A reply takes no anchor: it inherits the parent's file and line, and passing
`--file` or a line flag with it is an error. So you can reply after a fix even
when the parent's line has left the diff.

## Reading the user's comments

The user writes comments in the window with `c`. Read them with
`riffnav comment list --json` when they say they've left notes, and before
revising code they reviewed.

In the herdr desktop app, the user can send you a notice like this, typed into
your terminal by riffnav, not by the user:

```
[riffnav] The user left 2 new review comments for you in /path/to/repo (branch main). Read them with `riffnav comment list` from that folder. ...
```

Run `riffnav comment list` in that folder (it may differ from your working
directory or branch) and act on the comments.

## Good review comments

- Flag what the user wouldn't spot: intent, risk, subtle interactions, follow-ups.
  Not every hunk.
- One idea per comment, on the exact line it's about.
- Say what and why: "No backoff, so a flapping upstream becomes a tight loop"
  beats "add backoff".

## Errors

- **no file `x` in this diff**: the path isn't in the changeset; the error lists
  what is.
- **line N is not in ...'s diff**: the line isn't in a hunk. The error lists the
  ranges and the diff in force. Re-run `context`; don't pick a nearby line.
- **pass one of --new-line or --old-line**: exactly one, never both.
- **a reply carries no anchor of its own**: drop `--file` and the line flags.
- **no riffnav session and not inside a git repository**: run it from the repo.
- **no comment with id X to reply to**: check `riffnav comment list`.

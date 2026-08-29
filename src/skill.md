---
name: riffnav-review
description: Leave and read inline review comments on a git diff through the riffnav CLI. Comments are anchored to a file and line, persist on disk, and appear live in the user's riffnav window. Use when reviewing a branch or changeset the user is reading in riffnav, or when the user asks you to comment on code.
---

# riffnav review comments

riffnav is a terminal diff viewer the *user* drives. Never run `riffnav diff` or
`riffnav show` — they take over the terminal. Use the `riffnav comment ...`
subcommands, which are plain non-interactive processes.

Comments are stored per repo and branch under `$XDG_STATE_HOME/riffnav/comments/`.
They persist, so you can leave notes whether or not a window is open. If one is
open, it picks them up within a moment — no daemon, no port, nothing to unblock.

## Workflow

```bash
riffnav comment context            # 1. what can be commented on
riffnav comment list               # 2. what's already been said
riffnav comment apply --stdin      # 3. leave your notes in one batch
```

Run `context` first. It prints each file with the line ranges that are actually
in the diff, and it is far smaller than the diff itself. You cannot anchor a
comment to a line outside those ranges.

Read the code with your normal tools (`git diff`, Read); riffnav's CLI is for
writing and reading *comments*, not for reading the diff.

## Sign every comment

Always pass `--author` (or `"author"` in a batch), naming yourself — `claude`,
or whatever the user calls you. Without it the note is recorded under `$USER`,
so your review arrives looking like something the user wrote themselves: the
window colors each author differently and draws the user's own name in its own
color, and a thread is only readable when it says who is speaking.

## Commands

```bash
riffnav comment context [--json]
riffnav comment list [--file <path>] [--author <name>] [--json]
riffnav comment add --file <path> (--new-line <n> | --old-line <n>) --body <text>
                    --author <name> [--reply-to <id>]
riffnav comment apply --stdin
riffnav comment rm <id>
riffnav comment clear [--file <path>] --yes
```

### Anchors

Every comment names one file and exactly one line:

- `--new-line <n>` — a line number on the **post-image** side: added or context
  lines. This is what you almost always want.
- `--old-line <n>` — a line number on the **pre-image** side: removed or context
  lines. Use this to comment on code that was deleted.

Line numbers are the ones git prints in the hunk header, 1-based, and are the
same ones the user sees in riffnav's gutter.

### One comment

```bash
riffnav comment add --file src/app.rs --new-line 103 --author claude \
  --body "This retry loop has no backoff."
```

Pass `--body -` to read the text from stdin when it is long or full of quotes.

### Several comments

Prefer one batch over many invocations. The whole batch is validated before any
of it is written, so a bad line number fails cleanly instead of half-applying.

```bash
printf '%s' '{"comments":[
  {"file":"src/app.rs","newLine":103,"author":"claude","body":"This retry loop has no backoff."},
  {"file":"src/app.rs","oldLine":88,"author":"claude","body":"Why was the guard here dropped?"},
  {"file":"README.md","newLine":12,"author":"claude","body":"Stale: the flag is now --diff."}
]}' | riffnav comment apply --stdin
```

Each item needs `file`, `body`, `author`, and exactly one of `newLine` or
`oldLine`; `replyTo` is optional. Field names are camelCase. `apply` takes no
flags but `--stdin`, so every item carries its own author.

### Replying

`comment list` shows each note's short id. Thread under one with `--reply-to`:

```bash
riffnav comment add --file src/app.rs --new-line 103 --reply-to a3f1c2 \
  --author claude --body "Agreed — I'll add exponential backoff."
```

Replies render inside the same box as the comment they answer, marked `↳`.

## Reading what the user wrote

The user leaves comments in the TUI with `c`. Read them back with:

```bash
riffnav comment list --json
```

That is the channel for the user to hand you review feedback to act on. Check it
when the user says they've left notes, or before revising code they reviewed.

## Writing good review comments

- Comment on what the user would not spot themselves: intent, risk, a subtle
  interaction, a follow-up. Not every hunk.
- One idea per comment, anchored to the exact line it's about.
- Say what and why, not just what. "No backoff here, so a flapping upstream
  turns into a tight loop" beats "add backoff".
- Prefer `--new-line` unless you're specifically discussing deleted code.

## Errors

- **"no file `x` in this diff"** — the path isn't in the changeset. The error
  lists what is; or run `riffnav comment context`.
- **"line N is not in ...'s diff"** — the line exists in the file but not in the
  diff. The error lists the commentable ranges; pick one inside them.
- **"pass one of --new-line or --old-line"** — exactly one side, never both.
- **"no riffnav session and not inside a git repository"** — run from inside the
  repo you're reviewing.
- **"no comment with id X to reply to"** — check `riffnav comment list`.

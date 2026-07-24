---
name: atomic-vcs
description: Inspect repository state and history with the Atomic VCS CLI — status, log, change (including -p provenance and -a AI attestation), and diff. Use this whenever you need to see what changed, review your own recorded work, audit AI provenance, understand a teammate's change, or check the working copy before acting. These are read-only commands, safe to run anytime.
---

# Working with Atomic VCS

Atomic is the version control system for this repository — not Git. You and Atomic
are a pair: **hooks record your work automatically with full AI provenance** (model,
tokens, cost, session, the decision graph), and these commands let you **read that
history back**. Use them to ground yourself in reality instead of guessing.

You do **not** run `atomic add` or `atomic record` — the hook system does that at turn
end. Everything in this skill is **read-only inspection**, safe to run at any point in
a turn, as often as you like.

## The four commands

| Command | Answers |
|---------|---------|
| `atomic status` | What's different in the working copy right now? |
| `atomic log`    | What's the recent history of this view? |
| `atomic change` | What exactly is in one change? (deps, hunks, provenance, AI attestation) |
| `atomic diff`   | What are the precise line/token edits, working copy vs. recorded? |

Start with `status` to orient, `log` to scan history, `change`/`diff` to drill in.

## `atomic status` — working copy state

```bash
atomic status                  # full, human-readable
atomic status -s               # short/porcelain: one line per file, machine-parseable
atomic status src/             # restrict to a path
atomic status --no-untracked   # hide untracked files (only tracked changes)
atomic status --reindex        # rebuild FILE_INDEX first (use after git import / reset mtimes)
```

Short-format status codes (first column):

| Code | Meaning | Code | Meaning |
|------|---------|------|---------|
| `A ` | Added (new tracked) | `R ` | Renamed |
| `M ` | Modified | `C ` | Conflicted |
| `D ` | Deleted | `T ` | Type changed |
| `P ` | Permissions changed | `??` | Untracked |

Run `status` **before you start editing** (to see the starting point) and **before the
turn ends** (to confirm what your changes will record as).

## `atomic log` — change history

```bash
atomic log                     # full history of the current view, newest first
atomic log -n 10               # last 10 changes
atomic log -f oneline          # compact: one line per change
atomic log -f short            # hash + first line of message
atomic log -f json             # machine-readable array (for parsing)
atomic log --view dev          # another view's history without switching
atomic log --path src/auth.rs  # only changes that touched this path
atomic log --tags-only         # only tagged changes (release history)
atomic log --reverse           # oldest first
atomic log --from 42           # start at sequence 42
atomic log --full-hash         # full 52-char Base32 hashes (default is abbreviated)
```

`log` is your replacement for `git log`. Use `-f oneline` to scan quickly, then copy a
hash into `atomic change` to inspect one in detail.

## `atomic change` — inspect a single change

```bash
atomic change                  # the most recent change on the current view
atomic change R4YQUAS2UZV5     # by full hash
atomic change R4YQ             # by hash prefix
atomic change '#42'            # by sequence number (quote the # so the shell ignores it)
atomic change --view dev '#3'  # sequence lookup against another view
```

Detail flags:

```bash
atomic change <id> --show-deps     # show each dependency change's message
atomic change <id> --show-hunks    # show per-hunk graph-op details
atomic change <id> --full-hash     # full hashes
atomic change <id> -f json         # machine-readable
atomic change <id> -a              # AI attestation  (see below)
atomic change <id> -p              # provenance graph (see below)
```

### `change -a` — AI attestation (`--attest`)

Shows the AI metadata recorded inline in the change header:

- **vendor / model / model version** — which model authored the change
- **token usage** — input / output / total
- **cost** — what the change cost to produce
- **session info** — the session/request it came from

Use this to **audit authorship**: was a change human-written, AI-assisted, or fully
AI-authored, and at what cost? This is the trust layer that makes AI commits reviewable.

### `change -p` — provenance decision graph (`--provenance`)

Shows the causal decision DAG stored in the change's `.provenance` file — the *why*
behind the *what*:

- **goals** — what the turn was trying to achieve
- **tool executions** — the commands/searches that were run
- **explorations** — what was investigated
- **commitments** — the TODOs/decisions made
- **patch proposals** — the edits that became this change

This is uniquely powerful for collaboration: instead of reverse-engineering intent from
a diff, you read the actual reasoning chain. Use `change -p` to **understand why a prior
change was made before building on or modifying it** — including your own earlier turns.

Combine them: `atomic change <id> -a -p --show-deps` gives the full picture — what it
depends on, who/what authored it, what it cost, and the reasoning that produced it.

## `atomic diff` — precise edits

```bash
atomic diff                    # working copy vs. recorded state, all modified tracked files
atomic diff src/auth.rs        # one or more specific files
atomic diff -c R4YQ            # compare against a specific change
atomic diff --stat             # summary: files + line counts
atomic diff --name-only        # just the changed paths
atomic diff --name-status      # paths with M/A/D indicators
atomic diff --untracked        # include untracked files
atomic diff --view dev         # compare against another view
atomic diff --word-diff        # token-level highlighting (CRDT-powered)
atomic diff --algorithm patience   # better for moved blocks (default: myers)
```

`--word-diff` is special to Atomic: it shows exactly which **tokens** changed within a
line, not just that the line changed. Reach for it during code review when a line was
edited subtly.

## Collaboration playbook

**Orient at the start of a turn**
```bash
atomic status -s            # what's dirty?
atomic log -n 5 -f oneline  # what happened recently?
```

**Review your own last recorded change**
```bash
atomic change -a -p         # what did I just do, and is the provenance correct?
```

**Understand a change before modifying its code**
```bash
atomic log --path src/auth.rs -f oneline   # find the change that introduced it
atomic change <hash> -p --show-deps        # read the reasoning + dependencies
```

**Audit AI authorship across recent history**
```bash
atomic log -n 20 -f json    # then inspect interesting ones:
atomic change <hash> -a
```

**Verify before the turn ends**
```bash
atomic status -s            # confirm the set of files that will record
atomic diff --stat          # confirm the size/shape of the change
```

## When to use what

| Goal | Command | Not this |
|------|---------|----------|
| See uncommitted changes | `atomic status` / `atomic diff` | ~~git status / git diff~~ |
| Scan recent history | `atomic log -f oneline` | ~~git log~~ |
| Inspect one change in full | `atomic change <id> --show-hunks` | ~~git show~~ |
| See why a change was made | `atomic change <id> -p` | ~~guessing from the diff~~ |
| Check model/tokens/cost of a change | `atomic change <id> -a` | ~~assuming~~ |
| Token-level review of an edit | `atomic diff --word-diff` | ~~eyeballing the line~~ |
| Find changes touching a file | `atomic log --path <file>` | ~~grep through history~~ |
| Feed history to a script | add `-f json` / `--name-status` | ~~parsing human output~~ |

## Tips

- These commands are read-only — run them freely; they never modify the repo.
- You don't record (`atomic add`/`record`) — hooks do, with provenance. These commands
  let you read that provenance back.
- Quote sequence references so the shell doesn't treat `#` as a comment: `atomic change '#42'`.
- Add `-f json` (log/change) or `--name-status` (diff) when you need to parse output.
- For *code structure and content* search (functions, definitions, text), use the
  `code-intelligence` skill (`atomic vault query ...`). This skill is for *version
  history and working-copy state*.
- For the goals/intents/memory and recording workflow, see the `atomic-vault` skill.

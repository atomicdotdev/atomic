# Git Shadow — POC Task List

> **Goal:** Run Atomic alongside Git so that every commit, rebase, squash, and
> force-push is captured as immutable Atomic provenance. When Git rewrites
> history, the true record survives in Atomic's content-addressed change store.

## Design Principles

1. **Zero new tables.** Reuse `CHANGE_UNHASHED`, `DEPS`, `ProvenanceGraph`,
   `STACKS`, and `CHANGE_META`. No schema changes to the pristine.
2. **Git stays primary.** Developers keep using Git normally. Atomic is a
   sidecar that observes and records.
3. **Link, don't alias.** Each git commit produces its own Atomic change.
   Rebases and squashes create **new** changes linked to the originals via
   provenance edges — not aliases to the same change.
4. **Graceful gaps.** If a developer doesn't have hooks installed, the
   provenance graph has unlinked nodes, not missing data. Gaps are
   detectable and recoverable after the fact.

---

## Why Link, Not Alias

The original design tried to map multiple git SHAs to a single Atomic
change hash by inserting aliases into `INTERNAL`. This fails because
rebasing changes the diff — different context lines, different byte
offsets — producing different content and therefore a different Blake3
hash. You can't alias things that aren't identical.

The revised model accepts that each git commit is its own Atomic change:

```
Wrong model (alias):

  git:abc ──alias──▶ Atomic Change X ◀──alias── git:def
  "same change, multiple git names"
  ❌ Breaks because the diffs are different bytes

Right model (provenance link):

  Atomic Change X (from git:abc, pre-rebase)
       │
       └──[rewrote]──▶ Atomic Change Y (from git:def, post-rebase)

  "different changes, linked by what happened"
  ✅ Works because we record the relationship, not claim identity
```

For squash merges, the `post-rewrite` hook gives you the mapping directly:

```
Atomic Change A (from git:aaa, Alice, "add login")
Atomic Change B (from git:bbb, Bob, "handle expiry")
Atomic Change C (from git:ccc, Alice, "add tests")
       │
       └──[absorbed]──▶ Atomic Change D (from git:ddd, squash merge)

No diff decomposition needed. The hook tells you (aaa→ddd, bbb→ddd, ccc→ddd).
You already have A, B, C. You just draw the edges.
```

This eliminates the three failure modes of the alias design:

| Failure Mode | Alias Design | Link Design |
|---|---|---|
| **Identity instability** | Fatal — different diffs can't share a hash | Non-issue — each commit gets its own hash |
| **Graph isn't composable** | Pretends to be a VCS, isn't one | Honest provenance store, no pretense |
| **Adoption gaps** | Missing alias = broken lookup | Missing link = unlinked node, detectable |

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                    Git Repo (primary VCS)                            │
│                                                                     │
│  .git/                                                              │
│  ├── hooks/                                                         │
│  │   ├── post-commit  ──→  atomic shadow record                    │
│  │   ├── post-rewrite ──→  atomic shadow detect-rewrite            │
│  │   └── post-merge   ──→  atomic shadow record --event merge      │
│  └── ...                                                            │
│                                                                     │
│  .atomic/              (gitignored)                                 │
│  ├── pristine/         Atomic graph database (redb)                 │
│  ├── changes/          Content-addressed change files               │
│  └── shadow.toml       Shadow-mode config (protected branches, etc) │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

### Existing Infrastructure Reused

```
Existing Structure    │ Shadow Usage
──────────────────────┼──────────────────────────────────────────────
CHANGE_META           │ Header (message, author, timestamp) per change
CHANGE_UNHASHED       │ JSON with git SHA + branch + event type
DEPS / REV_DEPS       │ Link rewritten changes to their originals
ProvenanceGraph       │ Rewrite events (rebase, squash, amend)
STACKS                │ One stack per git branch (optional, for log)
```

No modifications to `INTERNAL`, `EXTERNAL`, or `NODE_TYPES`.

---

## Phase 0 — Core Plumbing

### Task 0.1 — Git metadata schema for `CHANGE_UNHASHED`

Define the JSON schema for git metadata stored alongside each Atomic change
and provide helper functions to read/write entries.

Every Atomic change created from a git commit carries this in its unhashed
metadata. It's a 1:1 mapping — one git SHA per change.

**Files:**

| File | Change |
|------|--------|
| New: `atomic-core/src/change/git_metadata.rs` | Types + helpers |
| `atomic-core/src/change/mod.rs` | Add `pub mod git_metadata;` |

**Types:**

```rust
/// Git metadata attached to an Atomic change via CHANGE_UNHASHED.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GitMetadata {
    /// The git commit SHA-1 this change was recorded from (hex-encoded).
    pub sha: String,

    /// Branch name at time of recording.
    pub branch: String,

    /// What git operation produced this commit.
    pub event: GitEvent,

    /// Unix timestamp of the git commit (may differ from Atomic change timestamp).
    pub git_timestamp: i64,

    /// Git author name (may differ from Atomic author if mapped).
    pub git_author: String,

    /// Git author email.
    pub git_email: String,

    /// If this change was created by a rewrite, the Atomic hash(es) of the
    /// original change(s) it rewrote or absorbed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rewrites: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub enum GitEvent {
    Commit,
    Rebase,
    Squash,
    Amend,
    ForcePush,
    Merge,
    CherryPick,
}
```

**Helper functions:**

- `extract_git_metadata(unhashed: &serde_json::Value) -> Option<GitMetadata>` — parse from existing unhashed JSON.
- `set_git_metadata(unhashed: &mut serde_json::Value, meta: &GitMetadata)` — write into the `"git"` key.
- `has_git_metadata(unhashed: &serde_json::Value) -> bool` — quick check.

**Acceptance:**

- Round-trip: serialize → deserialize → compare.
- `set_git_metadata` preserves existing non-git keys in unhashed JSON.
- `set_git_metadata` on `Value::Null` creates the structure from scratch.

**Depends on:** nothing

---

### Task 0.2 — Provenance node/edge kinds for git rewrites

Extend `ProvenanceNodeKind` and `ProvenanceEdgeKind` with git-specific
variants so rewrite events can be recorded in `ProvenanceGraph`.

**Files:**

| File | Change |
|------|--------|
| `atomic-core/src/change/provenance_graph.rs` | Add variants to both enums, update `label()`, `increment()`, and `ProvenanceStats` |

**New variants:**

```rust
// ProvenanceNodeKind
GitCommit,       // A git commit was observed and recorded
GitRewrite,      // A rewrite operation (rebase/squash/amend) was detected

// ProvenanceEdgeKind
Rewrote,         // "rewrite operation transformed change X into change Y"
Absorbed,        // "squash merge absorbed changes [X,Y,Z] into change W"
```

**Acceptance:**

- Serde round-trip works (`rename_all = "snake_case"`).
- `ProvenanceStats` has `git_commit_count` and `git_rewrite_count` fields.
- Existing provenance tests still pass.

**Depends on:** nothing

---

### Task 0.3 — `find_change_by_git_sha` helper

A query function that finds the Atomic change hash for a given git SHA by
scanning `CHANGE_UNHASHED`. For the POC this is a linear scan over changes
in a stack. At scale, a secondary index can be added later.

**Files:**

| File | Change |
|------|--------|
| New: `atomic-core/src/change/git_metadata.rs` | Add `find_change_by_git_sha` function |

**Signature:**

```rust
/// Scan changes on a stack to find one with a matching git SHA.
/// Returns the Atomic hash and parsed git metadata if found.
pub fn find_change_by_git_sha(
    txn: &impl GraphTxnT,
    stack: &StackState,
    git_sha: &str,
) -> Result<Option<(Hash, GitMetadata)>, PristineError>
```

**Implementation:**

Walk `STACK_CHANGES` for the stack, load each change's `CHANGE_UNHASHED`,
parse git metadata, compare SHA prefix. Short-circuit on first match.

**Acceptance:**

- Finds a change by full 40-char SHA.
- Finds a change by 7+ char prefix (like `git log --oneline`).
- Returns `None` for unknown SHAs.
- Unit test: register 3 changes with git metadata, look up each by SHA.

**Depends on:** 0.1

---

## Phase 1 — Shadow Recording (post-commit)

Convert a git commit into an Atomic change and store the git metadata
alongside it. This is the critical path — everything else builds on it.

### Task 1.1 — `atomic-git` bridge crate (scaffold)

Create a new crate that depends on `git2` (libgit2 bindings) and
`atomic-core` / `atomic-repository`. This crate owns all git ↔ atomic
translation logic.

**Files:**

| File | Change |
|------|--------|
| New: `atomic-git/Cargo.toml` | Crate definition, deps: `git2`, `atomic-core`, `atomic-repository`, `serde_json`, `blake3`, `chrono` |
| New: `atomic-git/src/lib.rs` | Module declarations |
| `Cargo.toml` (workspace) | Add `atomic-git` to workspace members |

**Modules to declare (empty stubs):**

- `pub mod shadow;` — shadow init/config
- `pub mod convert;` — git commit → atomic change conversion
- `pub mod hooks;` — hook event handlers
- `pub mod detect;` — rewrite detection
- `pub mod query;` — shadow log/blame queries

**Acceptance:**

- `cargo check -p atomic-git` passes.
- Crate can import from `atomic-core`, `atomic-repository`, and `git2`.

**Depends on:** nothing

---

### Task 1.2 — `shadow::init` — Initialize shadow mode

Set up `.atomic/` inside a git repo, create `shadow.toml` config, and
add `.atomic/` to `.gitignore`.

**Files:**

| File | Change |
|------|--------|
| `atomic-git/src/shadow.rs` | `pub fn init(git_root: &Path) -> Result<ShadowConfig>` |

**`shadow.toml` schema:**

```toml
[shadow]
# Branches that map to Shared stacks (edges in GRAPH)
protected_branches = ["main", "master", "release/*", "develop"]

# Automatically install git hooks
auto_hooks = true

# Include merge commits
record_merges = true
```

**Behavior:**

1. Verify `.git/` exists at `git_root`.
2. Call `Repository::init()` to create `.atomic/`.
3. Write `shadow.toml` with defaults.
4. Append `.atomic/` to `.gitignore` if not already present.
5. Return `ShadowConfig`.

**Acceptance:**

- Running in a git repo creates `.atomic/` and `shadow.toml`.
- Running again is idempotent.
- `.gitignore` is updated.
- Running outside a git repo returns a clear error.

**Depends on:** 1.1

---

### Task 1.3 — `convert::git_commit_to_change` — Core conversion

Read a git commit and produce an Atomic `Change` with git metadata in
its unhashed section.

**Files:**

| File | Change |
|------|--------|
| `atomic-git/src/convert.rs` | `pub fn git_commit_to_change(repo: &git2::Repository, commit: &git2::Commit, event: GitEvent) -> Result<Change>` |

**POC approach:**

For the POC, we do NOT need full Atomic graph ops. The change is a
provenance record with the diff stored as opaque content:

1. Extract commit metadata → `ChangeHeader` (message, author, timestamp).
2. Extract the unified diff as bytes → `Change.contents`.
3. Set dependencies from parent commit's Atomic change hash (if known).
4. Build `GitMetadata` and attach to `Change.unhashed`.

The change is content-addressed and immutable. The content is the raw diff,
which is sufficient for display and comparison. Full graph ops can be added
post-POC if needed.

**Acceptance:**

- Given a git commit, produces a `Change` with valid header and contents.
- `ChangeHeader` preserves: message, author name/email, timestamp.
- `Change.unhashed` contains valid `GitMetadata` JSON.
- The change can be saved to `.atomic/changes/` and loaded back.
- Unit test: create a git repo with 3 commits, convert each, verify
  dependency chain and metadata.

**Depends on:** 0.1, 1.1

---

### Task 1.4 — `hooks::post_commit` — Record a single commit

The handler called by the `post-commit` git hook. Orchestrates the full
recording pipeline: read HEAD → convert → save → update stack.

**Files:**

| File | Change |
|------|--------|
| `atomic-git/src/hooks.rs` | `pub fn post_commit(git_root: &Path) -> Result<ShadowRecordResult>` |

**Sequence:**

```
1. Open git repo + atomic repo + load shadow.toml
2. Read HEAD commit from git
3. Check if already recorded (scan CHANGE_UNHASHED for this SHA)
4. Determine stack name from current git branch
5. Ensure stack exists (open_or_create_stack)
6. Convert commit → Change (Task 1.3)
7. Save change to atomic repo
8. Apply change to stack (STACK_CHANGES entry)
9. Return result with atomic hash + git sha
```

**Return type:**

```rust
pub struct ShadowRecordResult {
    pub atomic_hash: Hash,
    pub git_sha: String,
    pub branch: String,
    pub already_recorded: bool,
}
```

**Acceptance:**

- After `git commit`, the Atomic repo has a new change.
- `find_change_by_git_sha(HEAD)` returns the new Atomic hash.
- The change is on the correct stack (branch name → stack name).
- Running twice for the same commit is a no-op (`already_recorded: true`).
- Integration test: `git init` → `git commit` → verify atomic state.

**Depends on:** 0.1, 0.3, 1.2, 1.3

---

### Task 1.5 — Git hook installer

Write the actual hook scripts into `.git/hooks/` and provide
install/uninstall functions.

**Files:**

| File | Change |
|------|--------|
| `atomic-git/src/shadow.rs` | Add `pub fn install_hooks(git_root: &Path) -> Result<()>` |
| `atomic-git/src/shadow.rs` | Add `pub fn uninstall_hooks(git_root: &Path) -> Result<()>` |

**Hook scripts (shell shims):**

```bash
#!/bin/sh
# .git/hooks/post-commit (appended by atomic)
# atomic:shadow:begin
atomic shadow record --event commit 2>/dev/null || true
# atomic:shadow:end
```

```bash
#!/bin/sh
# .git/hooks/post-rewrite (appended by atomic)
# atomic:shadow:begin
atomic shadow detect-rewrite --cause "$1" 2>/dev/null || true
# atomic:shadow:end
```

```bash
#!/bin/sh
# .git/hooks/post-merge (appended by atomic)
# atomic:shadow:begin
atomic shadow record --event merge 2>/dev/null || true
# atomic:shadow:end
```

**Behavior:**

- If a hook already exists, append between `atomic:shadow:begin/end` markers.
- `uninstall_hooks` removes only the lines between markers.
- Hooks fail silently (`|| true`) so git operations never break.
- Scripts are marked executable.

**Acceptance:**

- After install, `.git/hooks/post-commit` exists and is executable.
- After uninstall, the atomic lines are removed but pre-existing content preserved.
- If a pre-existing hook exists, it is not clobbered.
- Double-install doesn't duplicate the lines (markers are checked).

**Depends on:** 1.2

---

### Task 1.6 — `atomic shadow init` CLI command

Wire up `shadow::init` + `shadow::install_hooks` as a CLI subcommand.

**Files:**

| File | Change |
|------|--------|
| New: `atomic-cli/src/commands/shadow/mod.rs` | Subcommand routing |
| New: `atomic-cli/src/commands/shadow/init.rs` | `atomic shadow init [--no-hooks]` |
| `atomic-cli/src/commands/mod.rs` | Register the `shadow` subcommand group |

**Acceptance:**

- `atomic shadow init` in a git repo sets up everything from Tasks 1.2 + 1.5.
- `atomic shadow init --no-hooks` skips hook installation.
- Clear error message if not in a git repo.
- Prints summary of what was created.

**Depends on:** 1.2, 1.5

---

### Task 1.7 — `atomic shadow record` CLI command

The entry point that git hooks call. Reads the current git state and records
the appropriate change.

**Files:**

| File | Change |
|------|--------|
| New: `atomic-cli/src/commands/shadow/record.rs` | `atomic shadow record [--event commit|merge]` |
| `atomic-cli/src/commands/shadow/mod.rs` | Register subcommand |

**Behavior:**

- `--event commit` (default): calls `hooks::post_commit`.
- `--event merge`: same as commit but marks the git event as `Merge`.
- Quiet by default (git hooks should be silent).
- `--verbose` flag for debugging.
- Exit code 0 on success, non-zero on failure.

**Acceptance:**

- `atomic shadow record` after a git commit creates an atomic change.
- `atomic shadow record --verbose` prints the atomic hash and git SHA.
- Integration test: full `git init` → `atomic shadow init` → `git add` →
  `git commit` → `atomic shadow record` pipeline.

**Depends on:** 1.4, 1.6

---

## Phase 2 — Rewrite Detection (post-rewrite)

Detect when git rewrites history and record the new changes linked to the
originals via provenance edges.

### Task 2.1 — `detect::parse_rewrite_stdin` — Parse post-rewrite input

Git's `post-rewrite` hook receives `<old-sha> <new-sha>` pairs on stdin,
plus the cause (`rebase` or `amend`) as an argument. Parse them.

**Files:**

| File | Change |
|------|--------|
| `atomic-git/src/detect.rs` | `pub fn parse_rewrite_pairs(stdin: impl BufRead) -> Vec<RewritePair>` |

**Types:**

```rust
pub struct RewritePair {
    pub old_sha: String,  // hex-encoded, 40 chars
    pub new_sha: String,  // hex-encoded, 40 chars
}
```

**Acceptance:**

- Parses `"abc123def... 456789abc...\n"` format correctly.
- Handles empty input (no-op rewrite).
- Handles extra whitespace and trailing newlines.
- Unit test with sample post-rewrite output.

**Depends on:** 1.1

---

### Task 2.2 — `detect::handle_rewrite` — Process rewrite pairs

For each `(old_sha, new_sha)` pair:

1. Look up `old_sha` → original Atomic change via `find_change_by_git_sha`.
2. Record `new_sha` as a **new** Atomic change (convert the new commit).
3. Set the original change as a dependency of the new change (`DEPS`).
4. Store `rewrites: [original_atomic_hash]` in the new change's `GitMetadata`.

**Files:**

| File | Change |
|------|--------|
| `atomic-git/src/detect.rs` | `pub fn handle_rewrite(git_root: &Path, cause: &str, pairs: Vec<RewritePair>) -> Result<RewriteReport>` |

**Return type:**

```rust
pub struct RewriteReport {
    /// old_sha was found in atomic, new change created and linked.
    pub linked: Vec<LinkedRewrite>,
    /// old_sha was unknown (pre-shadow commit, no hooks at the time).
    pub unlinked: Vec<RewritePair>,
}

pub struct LinkedRewrite {
    pub old_git_sha: String,
    pub new_git_sha: String,
    pub original_atomic_hash: Hash,
    pub new_atomic_hash: Hash,
    pub event: GitEvent,   // Rebase or Amend
}
```

**Key behavior:**

- For `cause = "rebase"`: each pair gets `event: Rebase`.
- For `cause = "amend"`: each pair gets `event: Amend`.
- If `old_sha` is unknown (pre-shadow commit), record the new commit as a
  standalone change and add the pair to `unlinked`. This is a **graceful gap**
  — the data isn't lost, it's just not linked.
- The new change's `dependencies` in `HashedChange` includes the original
  Atomic hash. This puts the link in `DEPS`/`REV_DEPS` automatically.

**Acceptance:**

- After a `git rebase`, new Atomic changes exist for the rebased commits.
- Each new change's `GitMetadata.rewrites` contains the original Atomic hash.
- `DEPS` links new → original.
- `find_change_by_git_sha(old_sha)` still returns the original change.
- `find_change_by_git_sha(new_sha)` returns the new change.
- Both changes exist independently — deleting one doesn't affect the other.
- Integration test: commit → rebase → verify both changes and the link.

**Depends on:** 0.1, 0.3, 1.3, 2.1

---

### Task 2.3 — Record rewrite provenance graph

After processing rewrite pairs, create a `ProvenanceGraph` that captures
the rewrite event as a whole. This is the human-readable audit record.

**Files:**

| File | Change |
|------|--------|
| `atomic-git/src/detect.rs` | `fn record_rewrite_provenance(repo: &Repository, report: &RewriteReport, cause: &str) -> Result<Hash>` |

**Graph structure for a rebase of 3 commits:**

```
[GitRewrite: "rebase onto main"]
    ──Rewrote──▶ [GitCommit: "abc123 → def456 (feat: add login)"]
    ──Rewrote──▶ [GitCommit: "111222 → 333444 (fix: handle expiry)"]
    ──Rewrote──▶ [GitCommit: "555666 → 777888 (test: add auth tests)"]
```

`changes_explained` contains ALL affected Atomic hashes (both originals
and new changes).

**Acceptance:**

- Provenance graph is saved and registered via `register_provenance`.
- `changes_explained` contains both original and new Atomic hashes.
- Graph can be loaded back and nodes/edges are correct.
- `stats.git_rewrite_count` is incremented.

**Depends on:** 0.2, 2.2

---

### Task 2.4 — `atomic shadow detect-rewrite` CLI command

The entry point called by the `post-rewrite` git hook.

**Files:**

| File | Change |
|------|--------|
| New: `atomic-cli/src/commands/shadow/detect_rewrite.rs` | `atomic shadow detect-rewrite --cause <rebase|amend>` |
| `atomic-cli/src/commands/shadow/mod.rs` | Register subcommand |

**Behavior:**

- Reads stdin for `(old_sha, new_sha)` pairs.
- Calls `detect::handle_rewrite` then `record_rewrite_provenance`.
- Quiet by default, `--verbose` shows linked/unlinked counts.
- Exit code 0 on success.

**Acceptance:**

- `echo "oldsha newsha" | atomic shadow detect-rewrite --cause rebase` works.
- `--verbose` prints the number of linked and unlinked rewrites.

**Depends on:** 2.2, 2.3

---

## Phase 3 — Squash Merge Detection

Squash merges are a special case of the rewrite model. When git's
`post-rewrite` hook fires for a squash, it provides the `(old, new)` pairs.
The challenge is detecting squash merges that come through `post-merge`
(which doesn't provide old SHAs).

### Task 3.1 — Squash detection via `post-rewrite`

When `post-rewrite` fires with `cause = "squash"` or when multiple old SHAs
map to the same new SHA, detect and handle as a squash merge.

**Files:**

| File | Change |
|------|--------|
| `atomic-git/src/detect.rs` | Add squash detection logic to `handle_rewrite` |

**Detection heuristic:**

If multiple `RewritePair` entries share the same `new_sha`, it's a squash:

```rust
// Group pairs by new_sha
let groups: HashMap<&str, Vec<&RewritePair>> = pairs.group_by(|p| &p.new_sha);

for (new_sha, old_pairs) in &groups {
    if old_pairs.len() > 1 {
        // This is a squash merge: N old commits → 1 new commit
        handle_squash(git_root, new_sha, old_pairs)?;
    } else {
        // Regular rewrite: 1 old → 1 new
        handle_single_rewrite(git_root, cause, &old_pairs[0])?;
    }
}
```

**For squash merges:**

1. Record the squashed commit as a new Atomic change with `event: Squash`.
2. Set ALL original changes as dependencies of the squashed change.
3. `GitMetadata.rewrites` contains all original Atomic hashes.
4. Create a `ProvenanceGraph` with `Absorbed` edges from each original.

**Acceptance:**

- After a squash merge, the squashed change has deps on all original changes.
- `GitMetadata.rewrites` lists all original Atomic hashes.
- Each original change's author is preserved in its own Atomic change.
- Provenance graph shows `Absorbed` edges.
- Integration test: 3 commits on a branch → squash merge → verify all links.

**Depends on:** 2.2, 2.3

---

### Task 3.2 — Squash detection via merge commit heuristic

For squash merges done through `git merge --squash` (which fires
`post-merge`, not `post-rewrite`), detect by comparing the merged
branch's commits against the squash result.

**Files:**

| File | Change |
|------|--------|
| `atomic-git/src/detect.rs` | `pub fn detect_squash_from_merge(git_repo: &git2::Repository, merge_commit: &git2::Commit, atomic_repo: &Repository) -> Result<Option<SquashInfo>>` |

**Return type:**

```rust
pub struct SquashInfo {
    /// The Atomic changes that were squash-merged.
    pub absorbed_changes: Vec<Hash>,
    /// The branch that was squash-merged (if detectable from reflog).
    pub source_branch: Option<String>,
}
```

**POC approach:**

1. Check if the commit is a single-parent commit on a protected branch.
2. Look at the reflog for recent branch deletions (squash-merge workflow
   typically deletes the source branch afterward).
3. If a recently-deleted branch's changes are all "covered" by this commit's
   diff, it's a squash merge.
4. Fall back: if the commit message matches GitHub/GitLab squash format
   (e.g., contains `(#123)` or lists commit subjects), extract the SHAs
   from the message.

**Acceptance:**

- Detects a basic squash merge done via `git merge --squash`.
- Returns the correct `absorbed_changes`.
- Returns `None` for regular merge commits and non-squash commits.

**Depends on:** 0.3, 1.3

---

## Phase 4 — Query Commands

Surface the shadow data through CLI commands.

### Task 4.1 — `atomic shadow log` — True history

Show the Atomic change history for a stack, annotated with git SHA and
rewrite provenance.

**Files:**

| File | Change |
|------|--------|
| New: `atomic-cli/src/commands/shadow/log.rs` | `atomic shadow log [--stack <name>] [--show-rewrites]` |
| `atomic-cli/src/commands/shadow/mod.rs` | Register subcommand |

**Default output (no `--show-rewrites`):**

```
A3F2C8D1  abc1234  Alice <alice@example.com>  2024-01-15
    feat: add authentication middleware

B7E9F4A2  fff1234  Bob <bob@example.com>      2024-01-15
    fix: handle token expiry edge case
```

**With `--show-rewrites`:**

```
A3F2C8D1  abc1234  Alice <alice@example.com>  2024-01-15
    feat: add authentication middleware
    ├── rebased → E5D6C7B8 (git:def5678, 2024-01-16)
    └── squashed → F9A8B7C6 (git:7890abc, 2024-01-17)

B7E9F4A2  fff1234  Bob <bob@example.com>      2024-01-15
    fix: handle token expiry edge case
    └── squashed → F9A8B7C6 (git:7890abc, 2024-01-17)
        ⚠ Git attributes this to Alice. Original author: Bob.
```

**Implementation:**

Walk `STACK_CHANGES` for the stack. For each change, load `CHANGE_UNHASHED`
to get git metadata. If `--show-rewrites`, follow `REV_DEPS` to find
changes that list this one in their `GitMetadata.rewrites`.

**Acceptance:**

- Shows all changes on a stack with git SHAs.
- `--show-rewrites` shows the rewrite chain.
- Squash merges display the authorship discrepancy warning.

**Depends on:** 0.1, 1.4

---

### Task 4.2 — `atomic shadow lookup` — Resolve a git SHA

Given any git SHA (even one from a commit that was rebased away), find the
Atomic change and show its full lifecycle.

**Files:**

| File | Change |
|------|--------|
| New: `atomic-cli/src/commands/shadow/lookup.rs` | `atomic shadow lookup <git-sha>` |
| `atomic-cli/src/commands/shadow/mod.rs` | Register subcommand |

**Output:**

```
$ atomic shadow lookup abc1234

Git SHA:       abc1234
Atomic Hash:   A3F2C8D1E5...
Author:        Alice <alice@example.com>
Date:          2024-01-15 10:30:00 UTC
Message:       feat: add authentication middleware

Lifecycle:
  abc1234  commit   feature/auth  2024-01-15  → A3F2C8D1 (this change)
  def5678  rebase   feature/auth  2024-01-16  → E5D6C7B8 (rewrote A3F2C8D1)
  7890abc  squash   main          2024-01-17  → F9A8B7C6 (absorbed A3F2C8D1 + B7E9F4A2)
```

**Implementation:**

1. `find_change_by_git_sha` to get the Atomic change.
2. Follow `REV_DEPS` to find downstream changes that list this one in their
   `GitMetadata.rewrites`.
3. For each downstream change, show the git event and SHA.

**Acceptance:**

- Resolves any recorded git SHA, including pre-rebase ones.
- Prefix matching (7+ chars) works.
- Shows the full lifecycle chain.
- Returns clear message for unknown SHAs.

**Depends on:** 0.1, 0.3

---

### Task 4.3 — `atomic shadow blame` — True authorship

Like `git blame` but resolves through squash merges to show original authors.

**Files:**

| File | Change |
|------|--------|
| New: `atomic-cli/src/commands/shadow/blame.rs` | `atomic shadow blame <file>` |
| `atomic-cli/src/commands/shadow/mod.rs` | Register subcommand |

**Output:**

```
$ atomic shadow blame src/auth.rs

 Line │ Author (Git) │ Author (True) │ Atomic Change │ Git SHA
──────┼──────────────┼───────────────┼───────────────┼─────────
   42 │ Alice        │ Bob           │ B7E9F4A2      │ 7890abc ← squash
   43 │ Alice        │ Bob           │ B7E9F4A2      │ 7890abc ← squash
   44 │ Alice        │ Alice         │ A3F2C8D1      │ 7890abc
   45 │ Carol        │ Carol         │ C1D2E3F4      │ aaa1111
```

**Implementation:**

1. Run `git blame --porcelain <file>` to get per-line git SHAs.
2. For each unique SHA, `find_change_by_git_sha` to get the Atomic change.
3. If the change has `event: Squash` and `rewrites` is non-empty, walk
   the original changes to find who actually authored each line.
4. For non-squash lines, both columns show the same author.

**Walk logic for squash blame:**

The squashed change's diff contains all the lines, but we need to attribute
them to the original changes. For each line in the squashed diff:
1. Get the original changes from `rewrites`.
2. Check each original change's diff to see which one introduced this line.
3. Use that change's author as the "true" author.

**Acceptance:**

- Correctly shows true authors for squash-merged lines.
- Lines that weren't squashed show the same author in both columns.
- `--porcelain` flag for machine-readable output.
- Handles files that were never squashed (both columns identical).

**Depends on:** 0.1, 0.3, 4.2

---

## Phase 5 — Bulk Import (backfill)

Import existing git history so the shadow system works for repos that
weren't using Atomic from the start.

### Task 5.1 — `atomic shadow import` — Backfill existing history

Walk git log and create Atomic changes for historical commits.

**Files:**

| File | Change |
|------|--------|
| New: `atomic-cli/src/commands/shadow/import.rs` | `atomic shadow import [--branch <name>] [--since <date>] [--limit N]` |
| `atomic-git/src/convert.rs` | Add `pub fn import_branch(git_repo: &git2::Repository, branch: &str, atomic_repo: &Repository, opts: ImportOptions) -> Result<ImportReport>` |

**Behavior:**

1. Walk `git log --reverse` for the target branch.
2. For each commit, check if already recorded (`find_change_by_git_sha`).
3. If not, run the Task 1.3 conversion and save.
4. Show progress: `[142/1337] Importing abc1234 — feat: add auth (Alice)`.

**Return type:**

```rust
pub struct ImportReport {
    pub imported: usize,
    pub skipped: usize,  // already recorded
    pub errors: Vec<(String, String)>,  // (sha, error message)
}
```

**Acceptance:**

- `atomic shadow import --branch main` imports all commits on main.
- After import, every git SHA on main resolves via `find_change_by_git_sha`.
- `--since` and `--limit` work for partial imports.
- Idempotent: re-running skips already-imported commits.
- Progress output shows current/total.

**Depends on:** 0.3, 1.3, 1.4

---

## Task Dependency Graph

```
Phase 0 (Core Plumbing)            ┌────────────────────────────┐
  0.1  GitMetadata schema ─────────┤ No dependencies on each    │
  0.2  Provenance rewrite kinds ───┤ other. All three are       │
  0.3  find_change_by_git_sha ◀─0.1┤ parallelizable.            │
                                   └────────────────────────────┘
Phase 1 (Shadow Recording)
  1.1  atomic-git crate ───────────┐
  1.2  shadow::init ◀── 1.1       │
  1.3  git_commit_to_change ◀── 0.1, 1.1
  1.4  post_commit handler ◀── 0.1, 0.3, 1.2, 1.3
  1.5  hook installer ◀── 1.2     │
  1.6  CLI: shadow init ◀── 1.2, 1.5
  1.7  CLI: shadow record ◀── 1.4, 1.6

Phase 2 (Rewrite Detection)
  2.1  parse_rewrite_stdin ◀── 1.1
  2.2  handle_rewrite ◀── 0.1, 0.3, 1.3, 2.1
  2.3  rewrite provenance ◀── 0.2, 2.2
  2.4  CLI: detect-rewrite ◀── 2.2, 2.3

Phase 3 (Squash Detection)
  3.1  squash via post-rewrite ◀── 2.2, 2.3
  3.2  squash via merge heuristic ◀── 0.3, 1.3

Phase 4 (Query Commands)
  4.1  CLI: shadow log ◀── 0.1, 1.4
  4.2  CLI: shadow lookup ◀── 0.1, 0.3
  4.3  CLI: shadow blame ◀── 0.1, 0.3, 4.2

Phase 5 (Bulk Import)
  5.1  CLI: shadow import ◀── 0.3, 1.3, 1.4
```

---

## POC Milestone Definitions

### M1 — Shadow Recording (Phases 0 + 1)

`atomic shadow init` + `atomic shadow record` works. Every `git commit`
produces an Atomic change with git metadata in `CHANGE_UNHASHED`. An
engineer can look up any git SHA via `find_change_by_git_sha` and get the
Atomic change with original authorship.

**Task count:** 10 tasks (0.1–0.3, 1.1–1.7)

### M2 — Rewrite Detection (+ Phase 2)

After a `git rebase`, new Atomic changes are created and linked to the
originals via `DEPS`. The `post-rewrite` hook captures `(old, new)` pairs
automatically. Both old and new git SHAs resolve to their respective
Atomic changes, and the provenance link between them is recorded.

**Task count:** 4 additional tasks (2.1–2.4)

### M3 — Demo-Ready (+ Phases 3–4)

`atomic shadow log` and `atomic shadow blame` show true authorship through
squash merges. This is the "wow" demo — run `atomic shadow blame` on a file
that was squash-merged and see the real authors, not the merger.

**Task count:** 5 additional tasks (3.1–3.2, 4.1–4.3)

### M4 — Complete POC (+ Phase 5)

`atomic shadow import` backfills existing git history. A team can adopt
shadow mode on an existing repo and immediately get provenance for all
historical commits.

**Task count:** 1 additional task (5.1)

---

## What This Is NOT

To set expectations for the engineer picking this up:

1. **Not a parallel VCS.** The Atomic changes created by shadow mode are
   provenance records with raw diffs as content. They do not have full
   graph ops and cannot be used with Atomic's native apply/merge/diff
   operations. This is intentional.

2. **Not a git replacement.** Git stays primary. Atomic is read-only
   observation. Shadow mode never modifies the git repo (except adding
   hooks and `.gitignore` entries).

3. **Not 100% coverage guaranteed.** If a developer doesn't have hooks
   installed, their commits are recorded when they push (by whoever has
   hooks), but rewrite detection is missed for their local rebases. Gaps
   are unlinked nodes, not missing data. `atomic shadow import` can
   backfill the commits; the rewrite links are lost.

4. **Not real-time.** Recording happens synchronously in git hooks. For
   large repos, the `post-commit` hook adds latency to `git commit`.
   Profile and optimize in post-POC.

---

## Open Questions

1. **Hook latency.** How much time does the `post-commit` hook add? If
   it's >100ms, developers will notice. Measure and consider async
   recording (write to a queue, flush on next `atomic shadow` command).

2. **Merge commits.** Record as a single Atomic change with `event: Merge`,
   or skip (since the constituent changes are already recorded)? POC:
   record for completeness.

3. **Shallow clones.** `git2` may not have full history in a shallow clone.
   Detect and warn, don't crash. `atomic shadow import` should handle
   `--depth` gracefully.

4. **Multi-worktree.** Git worktrees share `.git/` but have separate working
   directories. Shadow mode should work per-worktree. Verify hook behavior.

5. **find_change_by_git_sha performance.** Linear scan is O(n) per lookup.
   For repos with 100k+ shadow changes, this will be slow. Post-POC: add a
   secondary index (new table or in-memory cache built at startup). For the
   POC, the scan is fine — most lookups happen right after recording when
   the change is near the top of the stack.

6. **Blame accuracy for squash merges.** The line-level attribution in
   Task 4.3 requires matching lines from the squashed diff back to
   individual original diffs. This is heuristic (context may differ).
   Acceptable for POC; flag low-confidence attributions in output.
# Atomic Git Shim — Operating Guide

> **Clean-clone correction (2026-10-26):** after the non-Rust restoration into
> `atomic-shim-2`, fresh intent 5 corrected the missing percent-path boundaries
> without transferring whole historical Rust files. The single-percent and
> distinct escape-lookalike regressions now pass, including repeat reconciliation
> and Git edit/rename/delete. The focused validation passed 42 Rust tests and
> 17 offline harness-49 tests, including native-path nonregressions. Fresh results
> and the retained pre-fix failures are
> recorded in [the validation report](../tests/harness/train_tracks.md). Subsequent
> harness-48 Git- and Atomic-authored GitHub round trips both passed (PRs #3/#4).
> Harness 47 initially refused the fork's non-pristine default branch, then passed
> all filename cases and real push after an explicitly approved backup/reset of
> the personal fork's `release`. Upstream remained unchanged. Historical fork
> results below remain separate from these fresh runs.

Updated 2026-10-13 for PR #207, source `27d06ff` (`atomic 0.19.1`).
The shim must be enabled for each repository. It is not enabled by default.
Use Linux or macOS; Windows shim support has not been validated.

Start with the [testing walkthrough](shim-review-testing.md) for setup,
commands, and expected results. The [RFC](RFC-ATOMIC-GIT-CAUSAL-BRIDGE.md)
contains the full design. The [task tracker](RFC-ATOMIC-GIT-CAUSAL-BRIDGE-TODO.md)
contains implementation and review history.

## Build availability and repository enrollment

Git interoperability is implemented by the independent
[`atomic-git-shim`](../atomic-git-shim/README.md) crate. The CLI's `git-shim`
Cargo feature includes that adapter; it is enabled in default builds to
preserve existing behavior. This is separate from the per-repository
`[git.shim] enabled` setting, which remains opt-in.

```sh
# Existing CLI, including Git commands and teams.
cargo build -p atomic-cli

# Native CLI with teams, without the Git adapter.
cargo build -p atomic-cli --no-default-features --features teams

# Git adapter enabled, teams disabled.
cargo build -p atomic-cli --no-default-features --features git-shim
```

A build without `git-shim` omits `atomic git`, `atomic stage`, and
`atomic unstage`, and refuses Git clone/adoption, `status --git`, and
`diff --git`. Native init, record, and view operations
remain available. An already enrolled workspace requires a shim-enabled
binary: the native-only CLI refuses before command execution, including RPC
dispatch, instead of updating Atomic without its Git publication adapter.
Use a shim-enabled binary to continue or to explicitly disable enrollment.
Read-only `atomic diagnostics` remains available for support, including in
enrolled or misconfigured workspaces. Without the feature, its report preserves
the repository's configured shim status and marks Git hook inspection unavailable.

`atomic stage` and `atomic unstage` are Git interoperability commands for
colocated repositories. Stage writes file content to the Git index; unstage
restores index entries to Git HEAD while keeping worktree edits. Native Atomic
uses `atomic add` for tracking and `atomic record` for recording changes,
without requiring a staging step.

This split does not remove Git-related repository data or every transitive
Git dependency. Repository guards, graph history, closure resolution, and
workspace transactions retain their existing owners. The `shadow` module
inside the adapter still coordinates projection and checkpoint rollback;
its name does not make that behavior obsolete.

Fresh eligible `atomic git import` runs use the rolling importer from #302.
Use `--no-stream` to select the ordinary importer. The default rolling payload
window is 32 MiB (`--stream-window-mib`); this is not a process memory limit.
`--agent-mode` uses an animation on supported terminals and falls back to
normal output when either stream is redirected or `TERM=dumb`. Both import
visuals and their terminal dependencies are part of the optional shim crate.

## 1. Current behavior and scope

The shim imports Git history, projects Atomic records into Git, checks that
both sides agree, and helps switch between views and branches. Git hooks and
an optional watcher notice changes; commands still check the current state.

**Fixed (2026-10-23):** creating a new Git branch from an imported tip and
committing there used to make `reconcile` fail with
`view '<branch>' not found` (then `forged Atomic change closure`), and a
fast-forward merge of such a branch back into `main` imported nothing. Record
projection commits are now verified by the view state they name, and the
incremental marker set is per target view. The
[reproduction](shim-review-testing.md#9-regression-new-git-branch-after-atomic-records)
is kept as a regression check.

**Hardened (2026-10-23, review of the founding-team rollout):**

- `atomic git import` refuses to materialize a branch that is not checked
  out over unrecorded work (tracked edits, or untracked files the branch
  tree would overwrite). Before, a detached HEAD or `-b <other>` import
  wrote the imported tree over them.
- Paths with spaces or non-ASCII bytes are tracked under the name on disk;
  `atomic status` is clean right after an import and incremental imports
  verify. A literal `%` in a file name still keeps its escaped identity.
- **Fixed (2026-10-26):** literal-percent names reconcile without a false dirty-worktree refusal
  or stale checkpoint. Filesystem status decodes imported identities once,
  while the Git-index overlay and semantic verification compare canonical
  identities without double-escaping them. Names such as `a%41.txt`, `aA.txt`,
  and `a%2541.txt` remain distinct. Native repositories that have not enrolled
  in the shim keep their raw path handling.
- Truncating a file to zero bytes imports on the mainline walk.
- Untracked files, gitignored or not (`node_modules/`, `target/`,
  `.DS_Store`, scratch files), are not divergence: `reconcile` and
  `verify` ignore them.
- `atomic git shim switch <view>` refuses a branch whose tip the view has
  never imported instead of projecting over it (which reverted those
  commits); reconcile the branch first.
- A detached checkout of an imported ancestor (`git checkout <sha>`,
  `git bisect`) maps to that commit's own closure; `reconcile` there and
  back to the branch both work.
- `git reset --hard` (or a rebase/force-pull) onto an imported ancestor
  rewinds the view: the dropped changes are unrecorded and the next
  `reconcile` verifies.
- A `.git/REBASE_HEAD` left behind by a completed interactive rebase is
  advisory; it no longer refuses every command as an in-progress rebase.
- The pre-push hook binds each pushed branch to its own view when one
  exists, so `git push origin <other-branch>` and `git push --all` pass for
  reconciled branches; a branch without a view of its own (`master`
  publishing the `main` view) is checked against the current view's
  verified tree as before.
- `atomic git shim switch` (and native `atomic view switch`) handle
  repositories whose tree holds symlinks, including a symlink to a
  directory (the `.claude/skills/<name>` layout).
- The first `reconcile` of a branch created from an anchored tip references
  the tip's closure instead of restoring and re-projecting it, and a branch
  whose view does not exist yet is seeded from the nearest interpreted
  first-parent ancestor, so only the commits above it import
  (`Seeded view 'topic' from interpreted ancestor ...; 1 commit(s) ahead`).
  Before: the whole history replayed into the new view (92 minutes on a
  363-commit repository).
- A native `atomic record` that deletes or rewrites lines in a file Git
  already holds no longer makes the next `git commit` unreconcilable: the
  record folds the file's existing branches through its own view instead
  of reading the shared rows, which a WIP evidence snapshot can tombstone.
  The same fold covers a native rename (`mv` plus `atomic record -a`) of a
  file whose last line has no newline, which used to leave the file's
  semantic closure holding every line twice.
- `atomic git shim switch` handles a directory that later became a symlink
  (the `.claude/commands -> ../.agents/commands` agents layout): the old
  files beneath the link are no longer scheduled for removal through it.
- Deleting a Git branch whose view was adopted (`git switch -c topic`,
  `git switch main`, `git branch -D topic`) retires the branch's ref
  mapping; it no longer refuses every later reconcile with "the mapped ref
  ... advanced beyond the last observed mapping". A renamed branch
  (`git branch -m`) still refuses with the explicit remediation.
- A branch created after a no-ff merge is seeded from the source view's own
  log, so the merged line renders once; the commit's persisted closure
  (the union of the merge's parents) is no longer used as a seed.
- An edit of a file whose graph vertices no longer line up with its lines
  (seen on pallets/markupsafe after a maintenance-branch merge) rewrites
  the file wholesale from the full new content instead of keeping only the
  edited lines; the staged check used to refuse the import at that commit.
- The staged check of an incremental single-parent commit replays the
  semantic closure only for the paths the commit touches (every path still
  gets the byte, mode and kind checks); merges, exclusions and fresh
  imports keep the whole-tree replay, and `ATOMIC_FULL_SEMANTIC_VERIFY=1`
  forces it.
- A change that truncates one file to zero bytes and extends another no
  longer captures the next edit of the extended file: a zero-length vertex
  sits exactly where the neighbouring file's last line ends, and the
  position-only context lookup used to attach the new line to it. Context
  positions now resolve through the file's own vertex index first. Seen as
  `atomic status` reporting a file modified right after a clean native
  record, and as `atomic git import` of stretchr/objx refusing at the
  commit that deleted the truncated `go.sum`.
- An empty lockfile or checksum file (`package-lock.json`, `Cargo.lock`,
  `go.sum`) that later gains content, or one truncated and regenerated,
  imports and reconciles: the regeneration is recorded as a whole-file
  replace on the file's existing semantic trunk instead of the
  "no semantic state" fast path, which emitted no semantic ops and left
  the staged closure rendering zero bytes.
- `atomic record` of a moved directory whose first entry is an empty file
  next to a non-empty sibling (a Python package with an empty
  `__init__.py`) applies: the empty file's inode marker starts where the
  sibling's name vertex starts, and the FOLDER edge source is now chosen by
  the marker/name alternation instead of failing with `ambiguous source ...
  for prior FOLDER edge` (or, on longer histories, projecting an
  `unresolved path claims` tree).
- Truncating a file to zero bytes no longer records a zero-length content
  vertex (the deletion of its lines is the whole change): that vertex sat
  at the same offset as the first line of whatever the same change
  rewrote next, and the position-only lookups in the projection, the
  content retrieval and the whole-file deletion walk read the neighbour's
  first line as the truncated file's content. Seen on stretchr/objx as
  `atomic git import` refusing the commit that deleted the truncated
  `go.sum` while editing `map_test.go` ("staged path 'go.sum' is present
  but the commit tree does not hold it"), and natively as `atomic status`
  reporting the deleted file as `D` right after the deleting record. The
  whole-file deletion walk now also collects a file's vertices through
  its own index.
- A commit that moves a file into a subdirectory while a symlink to the
  moved file takes its old path (srid/emanote: `README.md`, `LICENSE` and
  `CHANGELOG.md` became links into `emanote/`), and the mirror shape (a
  symlink replaced by the regular file it pointed to), import. Rename
  detection paired the vacated blob with the moved path and the fold then
  wiped the entry the same commit put at the old path, refusing with
  `prospective import tree mismatch`; both shapes are now folded the way
  Git itself reports them (a typechange beside an add, or a delete).
- A repository whose committed tree already carries conflict markers (a
  `Cargo.lock` merged by hand upstream) records through the shim: the
  post-record Git projection discounts markers that Git HEAD already holds
  byte-for-byte at the same path instead of refusing every later record
  after it had landed (which left Git permanently behind Atomic). A record
  that itself writes markers is still refused before anything lands.
- Importing a history whose tree is large (hundreds of MB of committed
  binaries, thousands of files) no longer holds a private copy of every
  file's bytes per imported commit: the prospective fold and the per-commit
  Git object store now share bytes between the per-commit trees. A 285 MB
  tree over 427 commits reached a 70 GB footprint and was killed by the OS.
- A CRLF file committed before the repository adopted `* text=auto eol=lf`
  (Git keeps the blob as committed and reports the worktree clean by its
  index stat) imports, and `atomic status`, `reconcile` and `verify` treat
  it as clean: a worktree file whose raw bytes are exactly the recorded
  blob is Git's own clean state even though the clean filter would
  normalize it on the next add. The importer used to refuse with `changed
  worktree path ... does not match the verified Git tree` and status
  reported the file modified forever.
- `git mv` of the last file out of a directory leaves the emptied
  directory on disk (Git cannot represent it). In a repository colocated
  with Git, `atomic status` no longer reports it as a reappeared `new dir`,
  so reconcile, verify and the next record proceed; a leftover directory
  holding only ignored entries (`.DS_Store`) is treated the same way; an
  empty directory you do mean to track is added with `atomic add
  --directory <path>`. A repository without Git keeps tracking empty
  directories as before.

**Repositories imported before this change must be re-imported.** The old
importer tracked paths with spaces or non-ASCII bytes under their escaped
names (`SolanaLogo%2022.png`); such a repository reports those entries
deleted and the real files untracked, and every reconcile refuses. Remove
`.atomic` (after recording anything pending elsewhere) and run
`atomic git import` again with this build.

**Known failures (not fixed; avoid until tracked fixes land):**

- Linked `git worktree`s are not managed by the shim yet. In a worktree that
  was never registered with the repository (plain `git worktree add`, no
  `.atomic/repository` pointer) Atomic's hooks pass through (one `Atomic
  shim: linked worktree '...' is not managed; hooks pass through` line on
  stderr; `git commit` and `git push` proceed) and `atomic status` refuses
  with `linked git worktrees are not supported by the shim yet; run from
  the main worktree '<path>'` instead of initializing a second repository.
  A linked worktree registered with the repository (the `.atomic/repository`
  pointer) adopts its own view and reconciles as before.
- A native `atomic record -a` of a moved directory that follows another
  native record without an `atomic status` in between can refuse with
  `cannot build Atomic publication project tree: ... unresolved path
  claims: <new directory>, ...` (seen on a 36-file notebook directory with
  byte-identical files under two subdirectories): the workspace boundary
  first captures the pending move as a snapshot, whose tree maintenance
  already moves the inodes, and the durable record then plans the moves
  against that state and re-creates the directory. Run `atomic status`
  before the record, or move with `git mv` + `git commit` + `reconcile`.
- Committing `.atomicignore` to Git makes `verify`/`reconcile` report
  `GitIndex/ExclusionPolicy .atomicignore`; keep it out of Git.
- Interrupting an import between a change write and its Git SHA index
  write can leave that commit unable to re-import (`already has a
  different persisted interpretation closure`); the only recovery is a
  fresh import.
- Performance: `atomic record` and `reconcile` still take minutes on
  repositories with a few hundred files (a one-commit reconcile spent
  about ten minutes on a 967-file repository before the scoped replay
  above; the retrieval hot paths remain). Importing a thousand-commit
  history takes hours.

Other limits:

- `atomic git shim enable --adopt-git` is still unavailable.
- `atomic init --adopt-git` and Git-transport clones use verified bindings to
  restore Atomic history. For Git history without a binding, use
  `atomic git import`. Import cannot recover agent information that Git never
  stored.
- `watch` and `cutover` are available, with the limits below.
- Passing tests does not enable automatic rollout or complete every RFC item.

## 2. Enable and disable

Having both `.git` and `.atomic` does not enable the shim. Ordinary Atomic
commands stay native until explicit shim setup, import, or a verified shim
checkpoint enrolls the repository.

For a local repository with imported Git history:

```sh
atomic git shim enable --no-refspecs --binding-key-file /path/to/binding.key
atomic git shim reconcile
atomic git shim verify
```

The key file contains a hex-encoded 32-byte Ed25519 secret key. Keep it outside
tracked files. It signs the Anchor binding. A checkpoint from `reconcile` alone
is not proof of a signed Anchor.

`enable` installs Atomic's Git hooks and sets `[git.shim] enabled = true` in
`.atomic/config.toml`. A notice that automatic rollout is blocked is expected.
For a remote, omit `--no-refspecs` and use `--remote <name>` (default `origin`).
A missing remote produces a warning and leaves binding transfer unconfigured.

Anchoring requires Git and Atomic to agree. `--adopt-atomic` can explicitly
project Atomic's state first, but refuses a dirty working copy. If the Git
branch has no commits, create its first commit before reconciling.

Atomic leaves custom `core.hooksPath` setups, existing hook binaries, and
symlinks untouched and prints integration instructions instead.

```sh
atomic git shim disable
```

Disable sets shim consent to false, stops automatic shim telemetry, and
removes every Atomic-owned hook dispatcher that `enable` installed
(post-checkout, pre-commit, pre-push, post-rewrite, reference-transaction),
so a plain `git push` is never refused by a shim nobody consents to any
more. Existing checkpoints stay in place: a previously anchored workspace
still checks for a stale baseline, and a workspace that was never anchored
returns to native behavior.

`atomic git hooks uninstall` removes the same set of dispatchers (plus
Atomic-only legacy import hooks) without touching the consent flag. Both
only remove exact Atomic-owned content: a custom hook, symlink, or foreign
script under one of those names is reported and left untouched, and a
custom `core.hooksPath` is never modified. `atomic git hooks status`
reports all five dispatchers.

## 3. Vault policy: private by default, tracked under explicit record

The vault (`.vault/`) participates in the shim projection according to RFC §6.2:
it is **private by default** — excluded from the repository manifest projection,
invisible to Git equivalence checks and publication — **but may be tracked** when
the colocated Git repository itself tracks `.vault/` in its tree.

Git publication is not a complete backup or synchronization protocol for
Atomic's knowledge graph. Materialized vault files alone do not carry the
full database state, goal/intent relationships, or query indexes. The crate
split preserves the existing vault conversion policy below; it does not
silently untrack an existing vault or promise those capabilities on a Git
clone.

The policy is recorded in `.atomic/config.toml`:

```toml
[git.shim]
vault = "tracked"   # or "private" (the default)
```

Resolution order:

1. An explicit `vault` record wins, in both directions. `atomic git import`
   writes `tracked` once, at first import, when it detects a `.vault` entry in
   the imported Git HEAD tree, and prints a notice — that first-import record
   is the "explicit policy" consent.
2. A configuration that predates the field omits it: "undecided". The Git HEAD
   tree then decides — a tracked `.vault/` entry resolves to `tracked`.
3. A corrupt or unreadable configuration fails closed: the policy refuses
   rather than silently changing.

Why a Git-tracked vault must join the projection: the manifest is what
equivalence, checkpoints, and publication compare against the raw Git tree. A
projection that silently lacked a Git-tracked `.vault/` could never verify
against that tree, and a Git commit published from it would delete the vault
from the branch. Under `tracked`, the vault is compared byte-for-byte like any
other tracked path; under `private` (no `.vault/` in Git), the vault is
Atomic-owned state that Git ignores via the managed exclude block.

Consequences of the record being explicit and durable:

- The conversion policy — and therefore the manifest root and every
  checkpoint's projection evidence — is frozen once recorded. Changing the
  record later invalidates prior projections; the verifier reports the
  divergence fail-closed instead of silently re-basing.
- Bindings published under one vault policy verify against that policy; a
  policy change requires re-publication.
- To force `private` on a repository that tracks `.vault/` in Git, record
  `vault = "private"` in `.atomic/config.toml` before importing; the verifier
  will then honestly report the tracked `.vault/` as a manifest divergence.

`atomic git import` on a repository that already tracks `.vault/` preserves the
tracked vault byte-for-byte: it hydrates the vault database from the working
copy and never installs templates over existing files (defaults are only
created for a repository with no `.vault/` at all). An unreadable file at a
default path — bad permissions, invalid UTF-8 — fails the import closed with an
actionable refusal rather than overwriting the bytes.

## 4. Working copies

Each physical directory has a working-copy ID in `.atomic/working_copy_id`.
Linked Git worktrees and agent sandboxes have separate IDs, shelves, and caches.
Some operation history is shared across the repository. An unfinished shared
operation can block work in another linked worktree until it is recovered.
A shim-enrolled repository does not manage its linked Git worktrees yet: work
from the main worktree (see the linked-worktree limit in §1).

## 5. Draft views, detached HEAD, and explicit publish

A Draft view can leave Git at a detached HEAD. That is expected when HEAD points
to the view's projection commit.

```sh
atomic view switch feature
atomic git shim reconcile
atomic git shim publish feature --branch feature
```

Publish requires a projection commit, so record or reconcile first. It creates
the named Git branch; the Atomic view remains a Draft and HEAD stays detached.
`atomic git shim switch <view>` switches to an existing mapped branch; it does
not create a missing branch.

## 6. Staging and status parity limits

```sh
atomic status --git
atomic status --git --json
```

These commands show the supported subset of Git-style staging status. They do
not promise full Git porcelain compatibility. Clean human-readable output can
be empty.

Draft conflicts can be represented by conflict snapshot commits on Draft refs.
Shared export has additional conflict checks; do not assume a Git commit means
all Atomic conflicts are resolved.

## 7. Trust, untrusted provenance, and signed boundaries

Content checks and trust checks are separate. A valid signature does not make
an unknown signer trusted. Repository trust settings live under `[git.trust]`:
explicitly revoked signers remain revoked, and unknown signers' provenance is
not trusted for publication.

Signed Anchor bindings use the supplied Ed25519 key. Managed-session capture
currently uses a session MAC, not an agent-DID signature; its signature coverage
has limits recorded in the RFC tracker.

`atomic git shim verify-receive` is the receiving-side check for a controlled
remote. A local pre-push hook can block this machine's push, but another client
or `git push --no-verify` can bypass it. The remote must enforce its own checks.

## 8. Safe recovery

Operations that use the journal store their planned changes and outcomes.
Writable open recovers interrupted operations before accepting new work.

Use `atomic op log` and `atomic op show` to inspect them. `atomic op undo` and
`atomic op restore` apply supported inverses and check that the current state
still allows the operation.

Do not delete `.atomic`, `.vault`, shelves, or Git objects to repair a failure.
They can contain the only remaining copy of pending work or recovery data.

## 9. WIP retention and incomplete sessions

If an agent turn leaves unexplained edits, Atomic retains WIP evidence and marks
the session incomplete. It must not silently discard those edits or claim that
another change explains them.

`atomic status --no-reconcile` shows drift without trying to reconcile it.
Incomplete sessions and their retained snapshots are not discarded just because
they are old.

## 10. Namespace fallback

Bindings and view refs use `refs/atomic/bindings/*` and `refs/atomic/views/*`.
Enablement configures fetch rules for the selected remote unless
`--no-refspecs` is used. Fetch rules cannot make a server accept a namespace it
rejects on push.

For those servers, `atomic git shim binding queue --degraded-head-fallback`
explicitly uses `refs/heads/atomic/bindings/*`. These refs look like ordinary
branches and can be affected by ordinary branch tools. This fallback is not
equivalent to the normal namespace and is never selected automatically. Check
published refs with `git ls-remote`.

## 11. Hooks and watchers are optional — never the guarantee

Git's filesystem monitor or Watchman can speed up candidate-file discovery.
The setting is `[git] watch = "off" | "auto" | "fsmonitor" | "watchman"` in
`.atomic/config.toml`. The default is `auto`; `off` is supported. Atomic checks
the candidates and falls back to a scan when needed.

The separate shim watcher is disabled by default. After shim enablement,
edit the existing config tables; do not append duplicate tables:

```toml
[git]
watch = "auto"

[git.shim]
enabled = true

[git.shim.watch]
enabled = true
```

```sh
atomic git shim watch --once
atomic git shim watch
```

The first command runs one pass. The second runs in the foreground until Ctrl-C.
Startup requires both shim and watcher consent. `git.watch = "off"` overrides
watcher consent.

The watcher waits for quiet Git metadata, then reconciles metadata only. It
never writes working files or moves Git refs. Locks, active Git operations,
pending recovery, or work that needs file changes can delay reconciliation.
`--once` returns nonzero when it cannot complete the pass and prints the reason.
The foreground loop reports the problem and retries.

Disabling the shim or setting `git.watch = "off"` stops the loop. Ordinary
commands still check repository state when the watcher is off.

## 12. Arbitrary Git push outside the protection guarantee

Local hooks do not protect a remote that accepts unchecked pushes. Use a
receiving-side verifier or a required server-side check when publication must
require verified provenance.

## 13. Privacy and security rules

The Git projection excludes `.atomic/`. The vault is private when Git does not
track it; a tracked `.vault/` joins the projection under the vault policy in
[section 3](#3-vault-policy-private-by-default-tracked-under-explicit-record).
Saving unrelated bridge settings does not create an explicit vault-policy record.

Bridge event telemetry uses fixed event types, identifiers, and counts. It does
not have a free-text field for prompts, transcripts, secrets, or environment
values. Git hook evidence records ref and operation events; it does not prove
that two rewritten commits have the same identity.

## 14. Observability: the structured event journal

Bridge events are stored in `.atomic/bridge/events.jsonl`. The file is capped at
4 MiB. New events can be dropped at the limit or when another writer holds the
lock. The journal is not fsynced and must not be used as recovery history.
Durable operations and receipts are stored separately.

Events include reconciliation, recovery, publication refusals, binding fetches,
import results, and scan fallbacks. Automatic writes require shim consent;
explicit shim commands provide their own consent. Observational status calls
do not write this telemetry.

Imports can fetch and verify binding data. Fetch loss is reported per fetch;
import correlation IDs connect related events. Event counts are diagnostic
information, not proof that every operation or event was captured.

## 15. Shim cutover and database migration

### Legacy Shadow → colocated shim

```sh
atomic git shim cutover
atomic git shim cutover --rollback
```

These are migration commands, not normal fresh-repository setup. Cutover requires
shim consent and passing readiness checks. It blocks the legacy shadow writer
and removes Atomic-owned hooks through the operation journal. Foreign hooks or
unsupported repository state can cause a refusal. Running a completed cutover
again does nothing.

Rollback requires the shared repository head to still point to that cutover.
It restores the saved hooks and lifts the cutover's fence while keeping newer
format fences. Later operations can prevent rollback.

**Known limit:** a `record` can save its Atomic change, then fail with
`git-bridge-cutover` when legacy shadow projection runs. Check `atomic log`,
`atomic op log`, and both statuses before retrying. See the
[cutover tests](../atomic-cli/tests/it/git_cutover_cb13b_test.rs).

### redb consolidation

The current database is `.atomic/atomic.redb`. Writable open can combine legacy
split databases and keep the old files under `.atomic/legacy/`. This migration
does not require shim enablement or `git shim cutover`.

Test a backup copy made with all source writers stopped. Compare history hashes,
files, views, provenance, and unrecorded work before and after migration. Then
reopen and record another change.

A held legacy database lock or new legacy writes after an interrupted migration
must cause a refusal rather than hide history. Keep both database copies if that
happens. The [testing walkthrough](shim-review-testing.md#8-database-migration)
lists the migration checks.

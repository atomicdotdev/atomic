# Atomic Git Bridge — Operating Guide

Updated 2026-10-13 for PR #207, source `27d06ff` (`atomic 0.19.1`).
The bridge must be enabled for each repository. It is not enabled by default.
Use Linux or macOS; Windows bridge support has not been validated.

Start with the [testing walkthrough](bridge-review-testing.md) for setup,
commands, and expected results. The [RFC](RFC-ATOMIC-GIT-CAUSAL-BRIDGE.md)
contains the full design. The [task tracker](RFC-ATOMIC-GIT-CAUSAL-BRIDGE-TODO.md)
contains implementation and review history.

## 1. Current behavior and scope

The bridge imports Git history, projects Atomic records into Git, checks that
both sides agree, and helps switch between views and branches. Git hooks and
an optional watcher notice changes; commands still check the current state.

**Known bug:** after recording an edit and rename with Atomic, creating a new
Git branch and committing there can make `reconcile` fail with
`view '<branch>' not found`. See the [reproduction](bridge-review-testing.md#9-known-failure-new-git-branch-after-atomic-records).
This remains unfixed in the tested revision.

Other limits:

- `atomic git bridge enable --adopt-git` is still unavailable.
- `atomic init --adopt-git` and Git-transport clones use verified bindings to
  restore Atomic history. For Git history without a binding, use
  `atomic git import`. Import cannot recover agent information that Git never
  stored.
- `watch` and `cutover` are available, with the limits below.
- Passing tests does not enable automatic rollout or complete every RFC item.

## 2. Enable and disable

Having both `.git` and `.atomic` does not enable the bridge. Ordinary Atomic
commands stay native until explicit bridge setup, import, or a verified bridge
checkpoint enrolls the repository.

For a local repository with imported Git history:

```sh
atomic git bridge enable --no-refspecs --binding-key-file /path/to/binding.key
atomic git bridge reconcile
atomic git bridge verify
```

The key file contains a hex-encoded 32-byte Ed25519 secret key. Keep it outside
tracked files. It signs the Anchor binding. A checkpoint from `reconcile` alone
is not proof of a signed Anchor.

`enable` installs Atomic's Git hooks and sets `[git.bridge] enabled = true` in
`.atomic/config.toml`. A notice that automatic rollout is blocked is expected.
For a remote, omit `--no-refspecs` and use `--remote <name>` (default `origin`).
A missing remote produces a warning and leaves binding transfer unconfigured.

Anchoring requires Git and Atomic to agree. `--adopt-atomic` can explicitly
project Atomic's state first, but refuses a dirty working copy. If the Git
branch has no commits, create its first commit before reconciling.

Atomic leaves custom `core.hooksPath` setups, existing hook binaries, and
symlinks untouched and prints integration instructions instead.

```sh
atomic git bridge disable
```

Disable sets bridge consent to false and stops automatic bridge telemetry.
It leaves installed hooks and existing checkpoints in place. A previously
anchored workspace still checks for a stale baseline. A workspace that was
never anchored returns to native behavior.

`atomic git hooks uninstall` currently removes only Atomic's post-checkout
hook. The other installed hooks need separate removal when decommissioning
that integration.

## 3. Working copies

Each physical directory has a working-copy ID in `.atomic/working_copy_id`.
Linked Git worktrees and agent sandboxes have separate IDs, shelves, and caches.
Some operation history is shared across the repository. An unfinished shared
operation can block work in another linked worktree until it is recovered.

## 4. Draft views, detached HEAD, and explicit publish

A Draft view can leave Git at a detached HEAD. That is expected when HEAD points
to the view's projection commit.

```sh
atomic view switch feature
atomic git bridge reconcile
atomic git bridge publish feature --branch feature
```

Publish requires a projection commit, so record or reconcile first. It creates
the named Git branch; the Atomic view remains a Draft and HEAD stays detached.
`atomic git bridge switch <view>` switches to an existing mapped branch; it does
not create a missing branch.

## 5. Staging and status parity limits

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

## 6. Trust, untrusted provenance, and signed boundaries

Content checks and trust checks are separate. A valid signature does not make
an unknown signer trusted. Repository trust settings live under `[git.trust]`:
explicitly revoked signers remain revoked, and unknown signers' provenance is
not trusted for publication.

Signed Anchor bindings use the supplied Ed25519 key. Managed-session capture
currently uses a session MAC, not an agent-DID signature; its signature coverage
has limits recorded in the RFC tracker.

`atomic git bridge verify-receive` is the receiving-side check for a controlled
remote. A local pre-push hook can block this machine's push, but another client
or `git push --no-verify` can bypass it. The remote must enforce its own checks.

## 7. Safe recovery

Operations that use the journal store their planned changes and outcomes.
Writable open recovers interrupted operations before accepting new work.

Use `atomic op log` and `atomic op show` to inspect them. `atomic op undo` and
`atomic op restore` apply supported inverses and check that the current state
still allows the operation.

Do not delete `.atomic`, `.vault`, shelves, or Git objects to repair a failure.
They can contain the only remaining copy of pending work or recovery data.

## 8. WIP retention and incomplete sessions

If an agent turn leaves unexplained edits, Atomic retains WIP evidence and marks
the session incomplete. It must not silently discard those edits or claim that
another change explains them.

`atomic status --no-reconcile` shows drift without trying to reconcile it.
Incomplete sessions and their retained snapshots are not discarded just because
they are old.

## 9. Namespace fallback

Bindings and view refs use `refs/atomic/bindings/*` and `refs/atomic/views/*`.
Enablement configures fetch rules for the selected remote unless
`--no-refspecs` is used. Fetch rules cannot make a server accept a namespace it
rejects on push.

For those servers, `atomic git bridge binding queue --degraded-head-fallback`
explicitly uses `refs/heads/atomic/bindings/*`. These refs look like ordinary
branches and can be affected by ordinary branch tools. This fallback is not
equivalent to the normal namespace and is never selected automatically. Check
published refs with `git ls-remote`.

## 10. Hooks and watchers are optional — never the guarantee

Git's filesystem monitor or Watchman can speed up candidate-file discovery.
The setting is `[git] watch = "off" | "auto" | "fsmonitor" | "watchman"` in
`.atomic/config.toml`. The default is `auto`; `off` is supported. Atomic checks
the candidates and falls back to a scan when needed.

The separate bridge watcher is disabled by default. After bridge enablement,
edit the existing config tables; do not append duplicate tables:

```toml
[git]
watch = "auto"

[git.bridge]
enabled = true

[git.bridge.watch]
enabled = true
```

```sh
atomic git bridge watch --once
atomic git bridge watch
```

The first command runs one pass. The second runs in the foreground until Ctrl-C.
Startup requires both bridge and watcher consent. `git.watch = "off"` overrides
watcher consent.

The watcher waits for quiet Git metadata, then reconciles metadata only. It
never writes working files or moves Git refs. Locks, active Git operations,
pending recovery, or work that needs file changes can delay reconciliation.
`--once` returns nonzero when it cannot complete the pass and prints the reason.
The foreground loop reports the problem and retries.

Disabling the bridge or setting `git.watch = "off"` stops the loop. Ordinary
commands still check repository state when the watcher is off.

## 11. Arbitrary Git push outside the protection guarantee

Local hooks do not protect a remote that accepts unchecked pushes. Use a
receiving-side verifier or a required server-side check when publication must
require verified provenance.

## 12. Privacy and security rules

The Git projection excludes `.atomic/` and `.vault/`. Files that users explicitly
track are ordinary Git content.

Bridge event telemetry uses fixed event types, identifiers, and counts. It does
not have a free-text field for prompts, transcripts, secrets, or environment
values. Git hook evidence records ref and operation events; it does not prove
that two rewritten commits have the same identity.

## 13. Observability: the structured event journal

Bridge events are stored in `.atomic/bridge/events.jsonl`. The file is capped at
4 MiB. New events can be dropped at the limit or when another writer holds the
lock. The journal is not fsynced and must not be used as recovery history.
Durable operations and receipts are stored separately.

Events include reconciliation, recovery, publication refusals, binding fetches,
import results, and scan fallbacks. Automatic writes require bridge consent;
explicit bridge commands provide their own consent. Observational status calls
do not write this telemetry.

Imports can fetch and verify binding data. Fetch loss is reported per fetch;
import correlation IDs connect related events. Event counts are diagnostic
information, not proof that every operation or event was captured.

## 14. Bridge cutover and database migration

### Legacy Shadow → colocated bridge

```sh
atomic git bridge cutover
atomic git bridge cutover --rollback
```

These are migration commands, not normal fresh-repository setup. Cutover requires
bridge consent and passing readiness checks. It blocks the legacy shadow writer
and removes Atomic-owned hooks through the operation journal. Foreign hooks or
unsupported repository state can cause a refusal. Running a completed cutover
again does nothing.

Rollback requires the shared repository head to still point to that cutover.
It restores the saved hooks and lifts the cutover's fence while keeping newer
format fences. Later operations can prevent rollback.

**Known limit:** a `record` can save its Atomic change, then fail with
`git-bridge-cutover` when legacy shadow projection runs. Check `atomic log`,
`atomic op log`, and both statuses before retrying. See the
[cutover tests](../atomic-cli/tests/git_cutover_cb13b_test.rs).

### redb consolidation

The current database is `.atomic/atomic.redb`. Writable open can combine legacy
split databases and keep the old files under `.atomic/legacy/`. This migration
does not require bridge enablement or `git bridge cutover`.

Test a backup copy made with all source writers stopped. Compare history hashes,
files, views, provenance, and unrecorded work before and after migration. Then
reopen and record another change.

A held legacy database lock or new legacy writes after an interrupted migration
must cause a refusal rather than hide history. Keep both database copies if that
happens. The [testing walkthrough](bridge-review-testing.md#8-database-migration)
lists the migration checks.

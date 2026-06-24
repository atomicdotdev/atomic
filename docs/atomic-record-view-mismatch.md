# atomic-agent record-view mismatch (engine bug)

> Status: **FIXED** on branch `fix/record-turn-view-mismatch` (2026-06-24).
> Surfaced by the env-5 admission test (`atomic-agent/tests/env5_admission.rs`),
> root-caused by two independent investigators (workflow `wf_025ec4b0-43a`),
> fix direction chosen by a safety investigation (workflow `wf_40655da1-f5b`).
> Regression test: `atomic-agent/tests/record_view_mismatch_regression.rs`.

## TL;DR

`record_turn` detected changes against the repo's `current_view` but applied them
to `session.view_name`. The **real agent hook flow is unaffected** — `session_start`
switches `current_view` to the session view before any turn is recorded
(`session_start.rs:192`), so detection and apply already agree. The bug only bit
**callers that bypass session start** — tests and, critically, the planned
**noname ACP→TurnOrchestrator bridge**, whose multi-turn runs would silently drop
every change after turn 1. Fixed by aligning the working view to the record view
inside `record_turn` itself, so the engine is self-consistent for any caller.

## Symptom

A second `record_turn()` on a **modified, already-tracked** file returns
`AgentError::EmptyTurn` ("nothing to record") in a fresh repository — even though
the file's content and size genuinely changed on disk. The first `record_turn()`
on the same file (when it was new/untracked) succeeds and yields a change hash.

Not a timing/flush issue: reproduced with a 1.1s delay + a large size change.

## Root cause

`record_turn` detects changes against one view but applies them to another:

- `Repository::init` creates `DEFAULT_VIEW = "dev"` and sets it as `current_view`
  (`atomic-repository/src/repository/mod.rs:144`, `:283`).
- `AgentSession::new` sets `view_name` to a **random haikunator name** via
  `make_view_name()` → `generate_goal_name()` (`atomic-agent/src/turn/session.rs:213`, `:243`).
- In `record_turn` (`atomic-agent/src/record/mod.rs`):
  - **Detection**: `status()` at `:156` runs against the repo's `current_view`
    (`dev`) — `status()` uses `self.current_view`, it takes no view argument
    (`atomic-repository/src/repository/status.rs:34`).
  - **Apply**: `repo.record()` is given `RecordOptions.view(session.view_name)`
    at `:278` — the random view.

Turn 1 (new file): the file is untracked, so it bypasses the view filter, is
auto-added, and is recorded onto the **random** view. Turn 2 (modified file):
`status("dev")` iterates the global TREE, finds the file, but the view-aware
filter (`status.rs:97-106`) sees that the file's creating change lives on the
random view, not on `dev`, so it **skips** the file. With nothing dirty,
`is_clean()` returns true → `EmptyTurn`.

The FILE_INDEX and TREE are global, so this is purely the view-filter logic — the
file metadata is correct.

## Fix (applied)

`record_turn` aligns change detection with the record view in two places
(`atomic-agent/src/record/mod.rs`):

1. **Before the first `status()`**, on the read-only handle, in memory only:
   `repo.set_current_view_in_memory(&options.session.view_name)`. This is a new
   `Repository` method (`atomic-repository/src/repository/mod.rs`) that assigns
   `self.current_view` with **no transaction, no lock, no disk write** — so the
   read-only no-lock fast path (added to avoid hook hangs) is preserved. If the
   session view does not exist yet (pure first turn), `status()`'s filter falls
   back to "show everything", so untracked detection still works.
2. **On the write handle**, the persisting `set_current_view(&session.view_name)`
   remains, now with explicit error handling (see below) so the post-add
   `status()` refresh and `record()` apply agree with the in-memory alignment.

Why the in-memory pre-alignment matters: without it, the **first** `status()` +
early `EmptyTurn` check run on `dev` *before* any view switch. The dangerous
case is a turn whose net effect is invisible from `dev` — e.g. **deleting the
last file that lived on the session view**: the first `status()` sees a clean
tree and the early check fires, silently dropping the turn. (A *modify* does not
trigger it — from `dev` the file shows as Untracked, so `untracked_count() != 0`
and the early check is skipped; only a full deletion opens the early-return
window. This is why the regression suite includes a delete-only case.)

Error handling on the persisting switch: tolerate `RepositoryError::ViewNotFound`
(legitimate first turn — apply creates the view; detection already aligned in
memory) but propagate DB / `.atomic/current_view` write failures as
`RecordFailed`, rather than swallowing all errors.

Why this shape (chosen over threading a view param into `status()`):

- **Minimal blast radius.** `record()` already filtered AND applied against
  `session.view_name` (`record.rs:122`, `:705`) — only the standalone pre-flight
  `status()` calls were mis-targeted. `status()`'s signature is untouched, so its
  many other callers (`status_quick`/`status_tracked`/CLI `status`) are unaffected.
- **No change to provenance/attestation landing** (already on the session view);
  attestation coverage *improves* because turns 2..N stop being skipped as `EmptyTurn`.
- **Real flow is a no-op** (`current_view` already switched at session start); only
  bypass callers (tests, noname bridge) gain correctness.

Sole production caller verified: `atomic-agent/src/turn/orchestrator/turn.rs:242`.
No cross-crate callers of `record_turn`.

## Known adjacent risk (NOT fixed here): racy `EmptyTurn` on same-size rewrites

`record_turn`'s detection uses `StatusOptions::fast()` (`hash_contents = false`),
whose FILE_INDEX fast path (`atomic-repository/src/repository/status.rs:271-330`)
decides Clean purely from `(mtime_secs, mtime_nanos, size)`. With no content-hash
fallback, a turn that rewrites a tracked file to the **same byte length** within
the **same filesystem mtime granule** as the previous turn's recorded mtime is
mis-classified Clean → spurious `EmptyTurn`. This is the classic "racy git"
problem and is **independent of the view-mismatch bug above**.

It was investigated while chasing parallel-test flakiness but could **not** be
reproduced here: each test has an isolated tempdir repo (so nothing is shared),
and the regression/admission tests already change file size between turns, which
the size check catches regardless of mtime. The one reproducible failure during
this work was a `cargo` package-cache lock collision from running two `cargo
test` invocations concurrently — a tooling artifact, not a test or engine defect
(direct test-binary runs: 40/40 green; full suite: 15/15 green; single-threaded:
5/5 green).

Left unfixed deliberately: the proper fix (verify by content hash when an mtime
match is "racy", i.e. within the current granule) touches `status.rs`'s widely
used fast path and is out of scope for this isolated view-mismatch fix. It is
flagged for the noname Y bridge, whose real multi-turn ACP runs could in
principle hit a same-size rewrite under load; if it ever surfaces there, fix it
in `status.rs` with the racy-clean → hash-fallback mitigation.

## Implication for the Sherpa Y bridge (P0)

This is a **real P0 requirement**, not just a test artifact: the noname
ACP→TurnOrchestrator bridge must keep the recording view aligned with the
detection view — either record onto the default view, or switch to the session
view before recording — or a real multi-turn ACP run will silently drop changes
after the first turn. Captured in `docs/sherpa-execution-v2.md` §1.

## Workaround used by the admission test

`atomic-agent/tests/env5_admission.rs` pins `session.view_name = DEFAULT_VIEW`
so detection and apply target the same view. With that alignment, modified-file
recording works correctly (test passes), confirming the engine's core recording
logic is sound and the defect is isolated to view routing.

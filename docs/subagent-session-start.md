# Subagent session-start contention

Follow-up: the shared-directory file-ownership limitation described below is
addressed by the separate `fix/subagent-file-ownership` branch; see
[file ownership](subagent-file-ownership.md).


This follow-up is based on PR #190 at
`67b74c8881ce4afe8a758beea093bc486a1da3f7`. It keeps redb 4.2 and does not
include Aaron's identity-delegation changes. Atomic intent: `ATOM::vince::12`
(`01M2NYA6AV3JHQZP31Z4R8R4MG`), view `fix-subagent-session-start`.

## Problem and change

Live OpenCode parent/subagent testing exposed session-start warnings that skipped
view creation when another process held the database. The existing record and
ordinary-read waiting paths did not cover session initialization.

Session creation, fallback creation and lifecycle synchronization now use the
existing typed contention retry helper, with a ten-second limit per database
acquisition. Only opening the handle is retried; view creation and lifecycle
operations are not replayed. Non-contention errors retain their existing
behavior. This does not make a permanently occupied database available.

## Validation (2026-09-16, macOS)

- New regression launches eight independent foreground OpenCode session-start
  processes while a real writable handle holds the database. After releasing
  the handle, all eight create distinct views and persist their lifecycle rows,
  without lock warnings or test-side retries.
- 19 database-owner integration tests, 4 ordinary-read contention tests, and
  36 orchestrator tests passed. Workspace Clippy with `-D warnings` passed.
- An initial concurrently loaded run hit an existing three-second corruption
  test deadline. The complete two-integration-test-binary rerun passed unchanged;
  the deadline was not relaxed.

An independent fix in `atomic-opencode` is also required: its former plugin-global
session ID and buffers mixed parent/child events. That fix keeps separate state
and ordered foreground hook calls per session.

With both fixes and Aaron's PR #189 at
`9c6fdff35a6a6ce73ea6a8c79eea1a26af136828` merged **only in a disposable local
build**, OpenCode 1.18.30 ran a real parent and two overlapping atomic subagents
using `openrouter/openai/gpt-5-mini`:

- All 21 tool calls completed; both child task intervals overlapped for about
  39 seconds. Each child ran three intent-list/status pairs.
- Three separate journal checkpoints reached `Published`. Their provenance
  content hashes verified. Frozen journal tool IDs matched their originating
  OpenCode sessions, with zero foreign or missing completed tool IDs.
- No database-lock warnings or plugin hook failures were observed.

The earlier run on #190 plus these fixes also published three correctly routed
checkpoints. Its parent attempted an external `/tmp` write that the test's
OpenCode permissions rejected; that run is not counted as a complete three-file
write test. The combined run constrained all files to its disposable directory.

## Remaining shared-directory limitation

This is not a claim of complete multi-agent workspace isolation. In the combined
run, one child's Stop recorded both child result files; the other child's
checkpoint contained provenance but no change hash. The parent recorded its own
file. Both child files exist in the recorded change, but appear untracked from
the parent's view after it becomes current again.

Separate hook event streams do not assign ownership of all files in a shared
working tree. Workspace/view isolation and merging child changes into the parent
remain separate integration work. This result does not establish overall bridge
or release readiness.

Local raw evidence is under `.gstack/subagent-fixes-20260916/` in the enclosing
workspace: Rust test logs, plugin regression logs, and `live-with189/` containing
the OpenCode events, session audit, frozen-journal audit and child change details.

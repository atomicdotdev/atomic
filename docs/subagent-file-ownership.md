# Shared-workspace file ownership

Follow-up to the session-start lock and OpenCode session-routing fixes. The
Atomic implementation remains on redb 4.2 and is isolated from PR #190 on
`fix/subagent-file-ownership`. Intent: `ATOM::vince::13`
(`01M2NZT2H8RPJ0RFPAYX8G8QCY`). The matching atomic-opencode change has intent
`ATOM::vince::2` (`01M2NZT228QP7FKNGJM14E4TMM`).

## Behavior

Previously each Stop scanned and recorded the whole working directory. A child
could therefore record its sibling's file even with correctly routed hook events.

The new OpenCode plugin negotiates `explicit-files-v1` through the CLI's
`agent hooks opencode file-snapshot` operation. It compares dirty-file fingerprints
before and after potentially mutating tools (including bash and unknown tools),
then sends only that session's changed files in `record_files` on Stop.
Fingerprints of candidate files cover contents, executable mode, symlink targets
and deletions. Candidate discovery uses Atomic status and therefore inherits
its file-index/mtime detection limits. It is not a full filesystem audit.
Unchanged pre-existing edits are excluded. Changed pre-existing unowned files,
foreign session rewrites and view changes during a tool block automatic recording.

Potentially mutating tools, session startup and publication use a workspace queue
with a 30-second acquisition limit. A timeout prevents that queued tool from
starting and does not unlock another running operation. Read tools and model
reasoning can overlap; a parent waiting on `task` never holds the queue. Error
and idle callbacks drain unfinished tool snapshots so partial writes remain
attributed and other sessions can proceed.

Parent and child sessions in the same plugin instance adopt one working view.
Each retains its own session ID, scoped change and provenance. This matches the
single actual filesystem baseline: the parent sees recorded child work without
an implicit cross-view merge. Separate session views remain the default for
other integrations and sessions that do not request shared-workspace recording.

The Rust recorder validates the manifest and current fingerprints before adding
and recording, filters status and envelope files to the same scope, and persists
the explicit-scope requirement in the session. An empty scope is a provenance-only
checkpoint. A missing/invalid scope is an error, not permission to sweep the tree.
SessionEnd does not sweep unrelated files after an already-completed scoped turn.

The plugin must be installed with the matching CLI. It refuses an older CLI
instead of falling back to unscoped recording.

## Validation on macOS, 2026-09-16

- 1,342 atomic-agent unit tests passed, including three new scoped-record tests.
- 20 owner integration and 4 ordinary-read integration tests passed. The added
  concurrent-Stop test verifies distinct file lists, a common declared workspace
  view, duplicate-Stop idempotency and no SessionEnd sweep.
- 13 plugin tests / 47 assertions passed, covering routing plus file snapshots,
  unrelated edits, foreign rewrites, deletions, partial failures, view drift,
  capability refusal and bounded queue acquisition.
- Workspace Clippy with `-D warnings` passed.
- A real OpenCode 1.18.30 parent and two overlapping atomic subagents used
  openrouter/openai/gpt-5-mini. All 21 tools completed. Three separate changes
  contained exactly `results/parent.txt`, `results/child-a.txt`, and
  `results/child-b.txt`, respectively. All three checkpoints published; frozen
  journal call IDs matched their sessions and all provenance hashes verified.
  There were no hook errors or lock warnings, and the final working directory
  was clean without a manual insert or record after the run.

The same real scenario also passed after a disposable local Git merge with
Aaron's PR #189 at `9c6fdff35a6a6ce73ea6a8c79eea1a26af136828`:
21 successful tool calls, three correctly scoped changes, three verified
published checkpoints, no hook errors, and a clean working directory. Child
execution intervals overlapped. This merge is test-only and is not included in
the follow-up branch. Evidence: `live-combined/`.

## Scope and remaining limits

This is coordinated attribution within one OpenCode instance, not an operating
system sandbox. Other processes/instances or human edits can still change files;
a post-tool fingerprint change is rejected, but snapshots cannot prove who wrote
bytes concurrently inside an uncoordinated tool interval. Independent overlapping
writers should use separate working directories.

Conflicting shared-file writes are not automatically merged or rolled back. The
plugin blocks publication and leaves the disk state for explicit reconciliation.
In-memory ownership and queued events are not a crash-persistent execution log.
Immediate OpenCode process exit may still interrupt pending callbacks; the live
test used a persistent server and a 15-second drain period.

An earlier scoped-record experiment retained separate child views. It preserved
each file's authorship, but explicit insertion into the parent left a child file
untracked even after reindexing. That experiment is retained in local evidence;
the shipped shared-workspace mode uses one baseline instead. It does not claim
to repair general cross-view directory merging.

Raw local evidence: enclosing workspace `.gstack/subagent-ownership-20260916/`,
particularly `live-shared/`, `integration.log`, `shared-integration.log`,
`agent-tests-final.log`, `integration-final.log`, `plugin-tests.log`, and `clippy-final.log`.

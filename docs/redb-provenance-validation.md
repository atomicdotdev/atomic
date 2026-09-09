# redb / provenance validation — 2026-09-09

Validated Lee's `fix/orchestrator_pass` at `b3dd89246607` on macOS ARM64,
using Rust 1.95.0. Follow-up changes live on `fix/redb-provenance-validation`.

## Findings and fixes

### Concurrent intent readers acquired exclusive database locks

With 16 concurrent workers issuing 64 `atomic intent list --json` requests,
the original branch returned `Database already open. Cannot acquire lock.`
for 60 requests. `Pristine::open_readonly` actually opened a writable redb
`Database`, and `intent list/show` called the writable repository constructor.

The fix uses redb's `ReadOnlyDatabase` and routes intent projections and
read-only sandbox opens through it. A deterministic integration test keeps a
reader open while eight CLI processes query intents from the canonical
repository and a sandbox. Writes through a read-only handle are rejected.
Legacy migration and required allocator recovery finish through a temporary
writable handle before acquiring shared read access.

After the fix, the same 64-request smoke test completed with 64 successes and
zero failures. This is a correctness check, not a performance benchmark.

### A read-only agent turn contaminated the next turn's provenance

On the original branch, a Codex turn with no file changes left the session
`active` with `turn_count = 0`. The next turn reused the same journal event IDs,
so its recorded goal was the first turn's prompt instead of the current prompt.

Read-only turns now publish their own immutable checkpoint with an empty
change list, advance the session, clear the completed prompt, and transition
to idle. Repeated Stop events after completion do not create extra checkpoints.
The regression test covers Codex, Claude Code, and OpenCode, asserting separate
goals, linked provenance hashes, and the absence of a change in the first turn.

### Windows owners retained hook output pipes

Windows CI hung in every owner integration test. Bounded subprocess diagnostics
showed that the calling processes had already exited, but their stdout pipes
remained open. The detached owner had inherited the caller's original standard
handles even though its own standard streams were redirected to NUL.

The owner bootstrap now clears `HANDLE_FLAG_INHERIT` on the caller's standard
handles before spawning. Explicit standard-stream inheritance remains available
through Rust's normal handle duplication. This follows the Windows behavior
of [Rust's process spawning](https://github.com/rust-lang/rust/issues/161158).
After this fix, the Windows suite no longer hung and seven owner tests passed.
The remaining four failures exposed a test shutdown race: a failed ping and a
fixed 100 ms sleep did not guarantee that redb had closed. Tests now wait for
the owner election lock to be released, which follows runtime/database cleanup.
Child commands and output collection have deadlines so future failures remain
diagnosable.

### Existing checks needed two small corrections

- The database owner lock now explicitly uses `truncate(false)`, satisfying
  Clippy's `suspicious_open_options` check while preserving the lock file.
- The attestation alias test extracts the numeric suffix from canonical
  `PROJECT::author::sequence` IDs before handling legacy hyphenated IDs. Its
  original split failed for the locally configured `vscode-e2e` identity.

## Validation

Local results: **8,820 tests passed, 0 failed, 242 ignored** across the full
workspace (including documentation tests). Clippy, formatting, and rustdoc
with warnings denied passed. Both shell suites (09 and 25) passed; suite 25
contains 13 provenance assertions. The OpenCode plugin passed 121 tests.

Commands:

```sh
cargo test --workspace --no-fail-fast
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps
ATOMIC_BIN="$PWD/target/debug/atomic" bash tests/harness/run_all.sh 09 25
(cd packages/opencode-atomic-hooks && bun test src/__tests__/)
```

The owner integration suite exercises real child processes for concurrent
append, duplicate event delivery, election/reconnect, owner crashes before and
after commit, checkpoint recovery, stale generation fencing, lifecycle
stop/resume/abandon, legacy graph migration, and previous-turn immutability.
Additional coverage includes legacy redb migration, concurrent intent reads
from sandboxes, and read-only-to-writing turns across three hook adapters.
A smoke test also exercised Codex's default detached Stop path and verified
that it eventually published the expected session ledger.

## Limits and follow-up

- redb 4.2 permits multiple read-only process handles, but a writable handle
  still excludes them. This change does not promise concurrent readers and
  writers of `pristine.redb`. The owner service separately owns `changes.redb`.
- Harness checks invoke actual hook CLI processes with fixture payloads and
  transcripts. They do not run paid model sessions or validate every host UI.
- The local totals above cover macOS ARM64. PR CI additionally exercises
  Linux, Windows, and the Rust 1.90 MSRV. Release readiness requires the checks
  on the final code revision to pass; current results are tracked on the PR.
- redb 5.0 evaluation and Git–Atomic bridge integration remain separate tasks.

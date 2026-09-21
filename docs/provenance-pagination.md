# Bounded frozen-journal reads

Atomic intent: `ATOM::vince::2`, in `fix/redb-provenance-validation`.

Long turns could persist every event successfully and then fail at Stop. The
checkpoint requested the entire frozen journal in one owner response; JSON's
numeric byte arrays could exceed the 8 MiB frame limit. The owner closed the
connection, leaving the durable checkpoint prepared but unpublished.

The hook now uses `LoadFrozenEnvelopesPage`. Each page is tied to the persisted
attempt generation and exclusive event cutoff. The `(sequence, byte offset)`
cursor survives owner restart and allows a large individual envelope to cross
pages. The client reconstructs complete envelopes in order before the existing
replay and publication path. Reads do not change the journal or checkpoint.

A Stop retry after an unsuccessful read may see a clean working copy because
the previous invocation already recorded its change. Such retries now reuse the
change hashes in the pending frozen checkpoint. The owner requires the current
generation and checkpointing state, then retains the existing source validation.
A different nonempty change set or incompatible source still fails explicitly.

A page contains at most 1 MiB of raw payload and 256 fragments. Its budget
accounts for the actual escaped request ID, fragment metadata, and the worst
case of four JSON bytes per payload byte. The 8 MiB frame limit remains in force.
The fragment-count bound also covers empty envelopes. Old-format journal
records are skipped consistently with the existing frozen-envelope reader.

## Compatibility and scope

The v1 Ping response advertises `frozen_envelope_paging`; older v1 clients can
still use their existing requests. New clients detect an older running owner
before sending page requests and report an explicit restart instruction:

```sh
atomic agent database-owner shutdown --repository /path/to/repo
# Retry the Stop hook with the updated CLI; it starts an updated owner.
```

Use the updated CLI for both the hook and owner. The legacy unpaged request
remains for compatibility and retains its original size limitation. There is
no database schema migration, redb dependency upgrade, or write batching change.
Aaron and Bradley's bridge view is outside this change.

This removes the single-response size ceiling. It does not make graph building
constant-memory: the client still collects complete envelopes for replay. Page
reads decode one persisted envelope at a time, including when an envelope spans
multiple pages.

## Regression coverage

```sh
cargo test -p atomic-cli --bin atomic commands::agent::owner::tests
cargo test -p atomic-repository --test provenance_checkpoint_test
cargo test -p atomic-cli --test database_owner_integration_test
cargo test --workspace
```

The new coverage exercises:

- Frame bounds under worst-case JSON expansion and an unusually long escaped
  request ID; fragment count limits and an envelope spanning multiple pages.
- Exact byte/order reconstruction with empty envelopes and legacy sequence gaps;
  identical page retries after reopening redb; stale generation, cutoff mismatch,
  and invalid cursor rejection.
- Real Codex, Claude Code and OpenCode Stop hooks with 1,024 committed tool
  envelopes, each retaining a 1 KiB output plus its raw payload. The unpaged
  response would exceed 8 MiB. Tests seed through the owner storage API to avoid
  1,024 CLI bootstraps, then verify actual hook publication and every tool ID.
- A real owner abort after the first page of a large envelope, followed by client
  reconnect and successful checkpoint publication. Duplicate Stop preserves the
  existing ledger and manifest head.
- A page-read failure that exhausts retries and ends the hook process; a later
  Stop completes the same attempt with the original change hashes and cutoff.

On macOS ARM64 with Rust 1.95.0, the final `cargo test --workspace` run passed
**8,827 tests**, with zero failures and 242 ignored. The owner integration suite
contains 14 passing tests. Targeted formatting checks and the release build pass.
This run does not establish new Linux or Windows CI results; the three harness
checks above exercise real CLI hook processes using fixture payloads.

An upgrade test detected the old owner's missing capability and completed the
original prepared checkpoint after an explicit restart. Separately, the original
1,024-event benchmark was run against the old binary until its >8 MiB response
failed. Retrying that same frozen checkpoint with the new binary succeeded,
retaining its source, generation, cutoff, all 1,024 tool payloads, and immutable
ledger. Its five page frames measured 2,118,514; 2,126,829; 2,127,113; 2,127,201;
and 11,760 bytes. Duplicate Stop left the ledger unchanged.

## Release comparison

Same machine, three repetitions per case, alternating binary order; deterministic
1 KiB tool outputs are also retained in the raw payload. The baseline is PR #190
at `b8f8c140a7ad34ff0f5833bf353e64edb696fe7f`, using redb 4.2. Both binaries
use that dependency. Values below are checkpoint medians, with min–max in
parentheses, in milliseconds.

| Scenario | Baseline | Pagination | Successful checkpoints, baseline / pagination |
| --- | ---: | ---: | --- |
| 128 events, 16 per RPC | 153 (148–157) | 152 (150–160) | 3/3 / 3/3 |
| 512 events | 167 (165–177) | 195 (172–381) | 3/3 / 3/3 |
| 1,024 events | Failed: frame limit | 189 (188–309) | 0/3 / 3/3 |
| Five successive 32-event turns | 138 (130–153) | 134 (127–156) | 15/15 / 15/15 |

The fixed binary completed all 24 checkpoints in 12 fixture runs. Every
successful checkpoint verified payloads, graph hashes, manifest head, previous
ledger rows and duplicate Stop. The unchanged baseline's three boundary failures
are expected. Pagination adds RPCs; this small sample shows a 28 ms median
increase for 512 events, including one 381 ms sample. These measurements support
the correctness repair, not a general performance improvement claim.

The existing evaluation driver is included with capability-aware page verification:

```sh
# Python 3 with the blake3 package installed; Unix socket fixture.
python tools/provenance-eval/run.py \
  --binary baseline=/absolute/path/to/baseline/atomic \
  --binary pagination=/absolute/path/to/fixed/atomic \
  --scenarios rpc-batch16 long-turn frame-boundary five-turn-history \
  --repeats 3 --output /tmp/provenance-pagination.json
```

The driver retains `frozen_frame_bytes` as the hypothetical unpaged frame size;
`verification_page_sizes` contains the actual paged response sizes. Raw results,
logs, the original-failure recovery fixture and immutable binaries are retained
locally in the workspace's `.gstack/provenance-pagination-2026-09-10/` directory.
The tested release binary SHA-256 is
`4f56676aaae13b6e5a8ac95de2d95ef348756efda4c0590c536e416fce66e2c3`.

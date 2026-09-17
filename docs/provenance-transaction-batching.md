# Provenance transaction batching

`ATOM::vince::8` · `fix/redb-provenance-validation` · redb 4.2

An owner RPC containing 16 new provenance envelopes previously opened and
committed 16 write transactions. It now writes those envelopes, event-ID index
entries and the turn's sequence frontier in one durable transaction. The owner
returns acknowledgments only after the commit succeeds. The single-envelope
API delegates to the same implementation.

## Behavior and compatibility

- Acknowledgments follow input order. Repeated IDs, including repetitions within
  the same batch, retain the first bytes and sequence. Changed retry observation
  timestamps do not overwrite the original event.
- Existing-event retries remain valid after generation/state fencing, matching
  the previous single-event contract. Any new event must match the running
  generation. Empty and entirely duplicate batches need no durable commit.
- **A batch is now all-or-nothing.** For example, a new event followed by a
  conflicting legacy event previously could leave the first event committed
  despite an error response. It now leaves neither event nor index/frontier
  changes from that batch. Already committed events remain intact.
- Checkpoint preparation and batch append serialize through redb's write
  transaction. The checkpoint sees either the full batch or none of it; if
  preparation wins, new appends using the old generation are fenced.
- No wire-format change, timer-based aggregation, dependency upgrade or
  durability weakening. This accelerates calls that already contain multiple
  events. Single-event hook calls still commit individually.

## Validation

Repository regressions cover input-order acknowledgments, within-batch and
cross-reopen duplicate retries, fenced mixed batches, rollback after a later
legacy conflict, sequence exhaustion and a 16-iteration checkpoint race.

The owner integration regression kills the owner immediately before or after
the batch commit. After restarting, retrying the same 16 events returns one
ordered set of acknowledgments; the database contains exactly 16 events. The
pre-commit crash leaves zero, and the post-commit crash leaves all 16.

Workspace validation: **8,358 unit/integration tests passed, zero failed, two
existing ignored tests** (`cargo test --workspace --lib --tests --locked --offline`).
Doctests are not included in that count. Five tests are new for this change;
the rest are existing workspace coverage. The owner suite passed all 18 tests,
including the existing large-journal, recovery and concurrent Stop regressions.

## Measured performance

Same-machine macOS release comparison against the previous Stop-coordinated
4.2 binary (`ATOM::vince::5`). Five repetitions, rotated mode order, 128 events
per case and 1,024-byte tool outputs. Both builds include the pagination and
Stop coordination fixes. The existing driver is reused with one added scenario
for eight concurrent 16-event RPCs. No builds or tests ran during measurement.

| Path | Before, events/s | After, events/s | Ratio |
|---|---:|---:|---:|
| One event per RPC | 238.0 | 240.4 | 1.01x |
| 16 events per RPC | 245.4 | 2,387.4 | 9.73x |
| Eight concurrent 16-event RPCs | 247.8 | 2,665.8 | 10.76x |

Values are medians across runs, not a general application speedup guarantee.
The 16-event request's median p50 latency fell from 63.44 ms to 5.98 ms;
single-event p50 stayed approximately 4.05 ms. Concurrent request latency
includes queueing. The application schema still requires serialization,
indexing and transport, so the result need not equal the raw KV benchmark.

All **30 measured checkpoints** verified sequence IDs, duplicate delivery,
payloads, graph/ledger links and completion. The separate 1,024-event boundary
test with 2,048-byte tool outputs passed for both binaries, including paged
retrieval of journals larger than the former 8 MiB frame limit.

The real-CLI multi-session regression used 1/4/8 workers, balanced/read-heavy
workloads, two repetitions and two turns per session, without external Stop
serialization. All **104 Stops and 52 sessions** verified, covering 1,664 tool
payloads. Ordinary `intent list` reads still hit the known database-open lock
conflict: **448 of 4,160 reads failed** with `Database already open`. There were
no session verification or worker failures, but this is not an all-green read
workload or a release-readiness claim.

[Results, build hashes and validation summary](../tools/provenance-eval/results/transaction-batching-2026-09-11.json)
and [raw results and logs](../tools/provenance-eval/results/transaction-batching-2026-09-11.tar.gz).

This change does not address ordinary CLI read-open contention, implement a
redb5 single-sync protocol, integrate the bridge, or declare a release candidate.

## Atomic storage handoff

Implementation change `JFZCKVVQIRIJPRSDYDQIV3TW3K4IEVR7MYAYT2IULTFBT2YBIUQQ`
was pushed to the existing `oss / atomic` storage project under
`fix/redb-provenance-validation`. The push stored six missing changes, including
the preceding validation/Stop work, and declared that view with 187 changes.
A fresh remote inventory check reported `Already up to date - nothing to push`.
The bridge view was not updated. Intent completion and its signed attestation
are recorded in the following metadata change.

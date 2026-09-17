# Concurrent Stop checkpoint publication

Work item: `ATOM::vince::5`, view `fix/redb-provenance-validation`.
This change retains redb 4.2 and the existing pagination/recovery repair.

## Problem and change

The existing `sessions/<session>/turn-end.lock` only deduplicates Stops for
the same session. Independent sessions can still open competing writable
handles to the canonical `pristine.redb`. In addition, checkpoint preparation
used to treat any ledger-open/read error as an empty session history.

Stops now acquire a canonical `.atomic/turn-publication.lock` before loading
the session and checking the working copy, and hold it through recording,
checkpoint publication and session save. Sandboxes resolve to the same
canonical lock. The per-session duplicate lock remains in place. The lock file
is retained; removing it could split waiters across different inodes. OS file
locks release on normal return, error or process termination.

Acquiring this Stop lock waits at most 10 seconds. Database opens used by
recording and checkpoint preparation/publication also wait up to 10 seconds
per acquisition for typed `DatabaseAlreadyOpen` contention. Other database
errors return immediately. These are bounded acquisition waits, not a single
10-second deadline for the entire Stop. Ordinary repository-open APIs retain
their immediate-failure behavior.

Ledger reads use a short-lived read-only handle and propagate errors before
binding a checkpoint. Journal replay occurs without a pristine handle held.
Existing checkpoint binding, atomic publication and acknowledgment remain in
place. A journal-backed record failure now leaves the session active and
returns an error so it can be retried; legacy orchestrators without a durable
journal retain best-effort recording behavior.

## Regression coverage

- Eight independent subprocess sessions publish two turns each, including a
  real file change, while a separately held pristine handle causes transient
  contention. No external Stop coordinator or test-side retry is used. Verify
  ledger ordinals, predecessors, graphs and exactly one recorded file change.
- A publication-lock timeout returns failure; retry and duplicate delivery
  produce exactly one checkpoint with the original goal.
- Kill a real foreground Stop while it holds the publication lock and waits
  for pristine, then retry and verify one recorded change and checkpoint.
- Database acquisition has a bounded wait; a corrupt database fails promptly.
- A sandbox resolves to its canonical database and can acquire it after a
  competing handle closes.
- Existing owner-death, pagination, frozen-checkpoint recovery and
  cross-harness hook tests remain applicable.

## Results, September 11, 2026

`cargo test --workspace --lib --tests --locked`: **8,353 passed, zero failed,
2 ignored**. This includes five new regression tests. Doctests were not rerun
and are not counted. `cargo fmt --all --check` passed. The release build passed.

The same release workload ran on the immutable pagination baseline and the
fixed binary, with 1/4/8 independent sessions, 16 tool events per turn, two
turns per session, balanced and read-heavy scenarios, and three repetitions.
Modes alternate order across repetitions. There is **no `--serialize-stop`**.
The fixed binary SHA-256 is
`151212dc8c11d4aa3ae825b0f6541e7c2d60b996d444860d20884ca51ba2e72a`.

| Result across 18 runs per binary | Pagination baseline, redb 4.2 | Fixed, redb 4.2 |
|---|---:|---:|
| Successful / attempted Stop calls | 40 / 100 | **156 / 156** |
| Sessions with both turns fully verified | 18 / 78 | **78 / 78** |
| Successful / attempted CLI reads | 3,983 / 4,000 | 5,602 / 6,240 |

The baseline aborts a worker after a failed Stop, so it attempts fewer second
turns and reads. Its shorter concurrent wall times are not a performance win.
The fixed runs verify all 2,496 tool-event payloads, frozen checkpoint source,
ledger links, content hashes and duplicate Stop behavior. Read errors are
collected separately; **638 ordinary CLI reads still failed**, so these are
successful checkpoint runs, not entirely error-free application workloads.

| Fixed workload | Sessions | Median complete workload | Stop p50 / p95 |
|---|---:|---:|---:|
| Balanced | 1 | 1.037 s | 54 / 66 ms |
| Balanced | 4 | 1.476 s | 103 / 319 ms |
| Balanced | 8 | 2.902 s | 308 / 1,334 ms |
| Read-heavy | 1 | 1.797 s | 53 / 66 ms |
| Read-heavy | 4 | 3.365 s | 112 / 317 ms |
| Read-heavy | 8 | 4.993 s | 301 / 1,252 ms |

Stop latency includes coordination and database acquisition. These are local
measurements with three repetitions, not a general throughput guarantee or a
redb 5 comparison. The single-session baseline medians were 1.298 s balanced
and 2.211 s read-heavy; there was no observed single-session regression in this
sample. A causal speedup claim would require more profiling and repetitions.

Machine-readable results:
[summary](../tools/provenance-eval/results/concurrent-stop-2026-09-11.json),
[raw results, logs and source/binary manifest](../tools/provenance-eval/results/concurrent-stop-2026-09-11.tar.gz).
The summary includes the archive checksum. The archive was read back and
checked against each original file.

Reproduce using `tools/provenance-eval/multi_session.py` with two immutable
release binaries:

```sh
python tools/provenance-eval/multi_session.py \
  --binary baseline-4.2=/absolute/path/to/atomic-pagination \
  --binary fixed-4.2=/absolute/path/to/atomic-stop-coordinated \
  --workers 1,4,8 --events 16 --turns 2 \
  --scenarios balanced,read-heavy --repetitions 3 --output release.json
```

## Scope and remaining limits

September 15 follow-up: ordinary CLI read commands now also use bounded
database acquisition. See [ordinary read lock waiting](ordinary-read-lock-wait.md)
for scope and the new zero-read-failure workload results. The measurements above
describe the earlier Stop-only fix and are retained as historical evidence.

This coordinates cooperating Stop publishers. It does not turn redb 4.2 into
a database that supports readers alongside a writable process. Ordinary CLI
reads may still fail while a writer is open. Other long-lived writers can
exhaust the acquisition timeout; retry the Stop after contention clears.

The workload replay invokes real independent CLI/hook processes with
deterministic events; it does not run language models. Sessions share a managed
view. Concurrent model edits to the same working files and automatic view
switching are outside this experiment. The fix has not been merged into the
experimental redb 5 or Git–Atomic bridge views, and is not a claim that the
combined release candidate is ready.

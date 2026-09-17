# Bounded database acquisition for ordinary CLI reads

Work item: `ATOM::vince::9` (`01M2KK12S425M8QGZ7RVS2XTES`), independent
`fix/redb-provenance-validation` view. Retains redb 4.2.

## Behavior

Stop/checkpoint publication already waited for transient database contention.
Ordinary queries still used immediate read-only opens and failed when a Stop
held the incompatible writer handle. They now acquire the repository through
a shared CLI helper that calls `Repository::open_readonly_wait` with a ten-second
limit. It retries only typed `DatabaseBusy`, not the command or its side effects.
Corruption, permission and other errors propagate immediately; persistent
contention returns an error after the acquisition budget expires.

Covered commands: `intent list`, `intent show`, `vault show`, `vault context`,
`status` (read phase), `log`, `diff`, `change`, and `conflicts`. Sandbox reads
resolve to the canonical database. The public Repository immediate-open APIs,
write paths, advisory evidence/import probes, and redb version are unchanged.
This is bounded retry, not an API server, a fair queue, or redb 5 shared mode.

## Regression verification

- Actual subprocess queries remain alive while a writer is held, then return
  successfully when it closes; includes canonical and sandbox intent reads
  and status/log/diff/conflicts. No test-side command retry is used.
- A continuously held writer causes an ordinary read to fail after ten seconds.
- A corrupt database fails within the three-second test deadline instead of
  spending the contention budget or returning empty success.
- Existing concurrent-reader and owner/checkpoint recovery tests remain green.
- Older change/log fixtures now release their initializer writer before invoking
  a query. Empty-log cases assert success instead of accepting any database error.

**1,909 CLI tests passed, zero failed or ignored** with
`cargo test --locked --offline -p atomic-cli --all-targets --no-fail-fast`.
`cargo fmt --all -- --check`, the release build, and the CI-equivalent
`cargo clippy --locked --offline --workspace -- -D warnings` passed.
An additional all-targets Clippy run reports the existing
`items_after_test_module` lint in `triage/output.rs:761` (HTML_STYLE at 1058).
That file is byte-identical to PR head 82f488b; it is outside this change.

## Release workload comparison

The immutable baseline is PR #190 head `82f488b0164086decf720569fe34bdced4e1a398`,
which already contains pagination, concurrent Stop coordination and batching.
Both binaries use the same redb 4.2 dependency lockfile. Existing
`tools/provenance-eval/multi_session.py` runs 1/4/8 independent sessions,
balanced/read-heavy queries, two turns of 16 tool events, three repetitions
per case, alternating binary order. No `--serialize-stop` or external retries.
Compilation and tests finished before measuring.

| Result across 18 runs per binary | Before read waiting | With read waiting |
|---|---:|---:|
| Reads successful/attempted | 5562/6240 | 6240/6240 |
| Stop successful/attempted | 156/156 | 156/156 |
| Fully verified sessions | 78/78 | 78/78 |

All 2,496 tool-event payloads, event identities, ledger predecessors, graph
hashes and duplicate Stop behavior were verified in the fixed runs.

| Workload | Sessions | Before wall median (s) | Fixed wall median (s) | Fixed read p50/p95 (ms) | Fixed Stop p50/p95 (ms) |
|---|---:|---:|---:|---:|---:|
| balanced | 1 | 1.221 | 1.533 | 15.5/42.2 | 50.6/116.7 |
| balanced | 4 | 2.181 | 1.533 | 16.4/48.5 | 107.5/277.6 |
| balanced | 8 | 2.697 | 2.784 | 19.9/72.5 | 268.0/1023.7 |
| read-heavy | 1 | 2.283 | 1.727 | 9.8/15.5 | 50.3/53.8 |
| read-heavy | 4 | 2.778 | 2.734 | 12.5/31.5 | 95.5/275.0 |
| read-heavy | 8 | 5.536 | 4.964 | 19.1/48.3 | 173.8/833.3 |

Read latency includes internal lock waiting; baseline failed reads are counted
separately, so wall-time ratios are not an equivalent-success speed comparison.
Three local repetitions are validation evidence, not a universal performance
guarantee. These are deterministic sessions using real CLI/owner processes,
without model calls or concurrent model edits to working files. Writers that
remain open beyond the budget still cause a bounded failure. This does not
declare the separate bridge/redb 5 work or a combined release candidate ready.

Reproduce with `multi_session.py --binary before=/path/to/baseline
--binary waited=/path/to/fixed --workers 1,4,8 --events 16 --turns 2
--scenarios balanced,read-heavy --repetitions 3 --output results.json`.

[Summary](../tools/provenance-eval/results/ordinary-read-wait-2026-09-15.json) and
[raw results, commands, build manifest and logs](../tools/provenance-eval/results/ordinary-read-wait-2026-09-15.tar.gz).

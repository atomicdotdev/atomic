# Initial materialization baseline

Follow-up: [many-file switch optimization and balanced measurements](SWITCH-OPTIMIZATION.md).

Two sequential runs per profile, each with seven measured iterations and one
excluded warmup, two Rayon threads, on macOS/aarch64. All four runs used the same
optimized executable, SHA-256:

```text
9e2e606f59404461bfe9377e37055f1be7791e670997bfd416e62ea6d1174402
```

These are matched synthetic native histories, **not imported histories or cold
OS caches**. See [README.md](README.md) for timing boundaries and limitations.
The second hot-file run followed the second many-file run; workloads were not
run concurrently. Unrelated system load was not controlled.

## Median milliseconds, graph / span

| Case | Many files, run 1 | Many files, run 2 | Hot file, run 1 | Hot file, run 2 |
|---|---:|---:|---:|---:|
| Render session | 19.879 / 11.479 | 18.618 / 11.991 | 26.615 / 6.087 | 26.567 / 5.689 |
| Reopen + render | 79.108 / 67.086 | 76.854 / 67.346 | 81.716 / 57.147 | 81.074 / 55.913 |
| No-op materialization | 59.488 / 68.418 | 62.469 / 64.301 | 104.701 / 60.666 | 101.865 / 57.205 |
| Missing-file materialization | 515.078 / 491.784 | 509.120 / 488.628 | 140.755 / 97.291 | 138.410 / 94.687 |
| One-file switch forward | 170.279 / 240.252 | 193.557 / 238.088 | 193.673 / 128.816 | 190.012 / 124.809 |
| One-file switch back | 166.407 / 200.402 | 178.411 / 218.465 | 192.236 / 129.753 | 187.829 / 125.484 |
| Many-file switch forward | 593.525 / 595.991 | 589.467 / 621.105 | 194.396 / 130.727 | 190.844 / 124.910 |
| Many-file switch back | 591.356 / 629.987 | 599.503 / 628.057 | 194.247 / 129.663 | 189.254 / 123.764 |

Many files: 24 × 128 lines, 24 hot-file edits, plus an empty file.
Hot file: one × 4096 lines, 48 edits, plus an empty file. With one nonempty file,
the hot-file profile's one/many switch cases are effectively the same workload,
not independent evidence of multi-file scaling.

Every run completed all 112 backend samples (56 graph/span pairs), with expected
bytes/modes and a single verified working-copy operation head checked outside
sample timing. Four runs total 448 checked backend samples. No skips count as
passes. Benchmark smoke also passed all eight cases; runner unit tests and
example Clippy passed.

## Interpretation and next target

**Subsequent measurement caveat:** per-iteration inspection during optimization
found a strong first-position penalty in one-file forward switching. These
seven-sample runs give one backend four first-position samples and the other
three, so aggregate medians can overstate the backend gap. Preserve these raw
observations, but use even counts and order-stratified summaries for follow-up
comparisons. The runner now defaults to eight iterations and reports first/
second-position distributions explicitly. This does not establish the cause
of the order effect.

- Session rendering favors spans in both profiles: 1.55–1.73× for many files and
  4.37–4.67× for the hot file.
- Hot-file end-to-end results consistently favor spans: missing-file restoration
  1.45–1.46×, no-op materialization 1.73–1.78×, switching about 1.48–1.53×.
- Many-file one-file switching is consistently slower with spans. Forward
  medians are 238–240 ms versus graph's 170–194 ms. No-op results vary enough
  that a precise small regression claim would be premature.
- Many-file missing-file restoration improves only slightly; rendering savings
  do not translate proportionally into journaled filesystem operations.

In first-run session probes, span-context construction takes 2.934 ms versus
0.245 ms rendering for many files; the hot file takes 2.187 ms versus 0.071 ms.
Graph render phases are 12.918 and 23.370 ms respectively. Total probe timings
also include work outside those phases, including teardown. Do not add phase
medians and assume they equal the median total. Reopened probes spend roughly
50–56 ms opening either backend, limiting gains from rendering alone.

**Next priority: profile the many-file small-switch regression**, including
closure/context construction counts, per-file content retrieval, repeated plan
validation/rendering, and operation preparation/execution. Sweep file count
with total bytes and history held fixed to distinguish file-count overhead from
history/size effects. Context creation, prewarming, and repeated validation are
candidates from code inspection, not established causal explanations. Existing
materialization tracing starts after context construction and therefore cannot
by itself account for that cost.

No production performance optimization is included in this baseline. Retain
hash/index/causal validation, provenance, leases and receipts; reuse verified
work only within explicit validity boundaries.

## Raw evidence

Artifacts were retained outside the repository under:

```text
/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/span-harness01-1gFvn7/
  materialize-many-01/
  materialize-hot-01/
  materialize-many-02/
  materialize-hot-02/
```

Each contains `raw.json`, `run.json`, the pinned executable and stdout/stderr.
The first run also contains Cargo build logs and before/after source fingerprint
checks. Subsequent supplied-binary runs explicitly mark source association as
unverified; their executable SHA-256 matches the first build. A formatting-only
change to the example's function signature occurred before the second runs;
those runs still used the original frozen executable. Scratch paths are local
evidence, not permanent downloadable artifacts; this table preserves the
baseline if temporary storage is reclaimed.

# Many-file small-switch optimization

## Outcome

Batching switch lease rendering removes the measured span small-switch penalty
in the synthetic many-file fixture. Two balanced before/after runs show about
**16–19% lower forward span-switch latency** and **17–18% lower backward
latency**. Graph switching stays roughly unchanged. This is not a universal
performance guarantee, and imported-history behavior was not benchmarked.

Median end-to-end milliseconds; each cell shows the two separate run medians:

| Case | Graph before | Graph after | Span before | Span after |
|---|---:|---:|---:|---:|
| One-file forward | 184.911 / 189.600 | 192.233 / 189.912 | 226.140 / 223.808 | 183.791 / 187.740 |
| One-file back | 169.239 / 169.110 | 166.837 / 170.054 | 203.644 / 204.697 | 167.846 / 168.150 |
| Many-file forward | 577.404 / 587.715 | 590.208 / 596.941 | 615.565 / 628.793 | 577.510 / 571.247 |
| Many-file back | 598.902 / 600.790 | 597.419 / 598.245 | 628.626 / 636.846 | 597.138 / 592.881 |

Hot-file control (one balanced run before/after): span forward switching goes
from 126.280 to 120.804 ms and back from 125.843 to 120.651 ms. Afterward it
remains about 1.56–1.59× faster than graph. This preserves the existing hot-file
advantage; the small additional gain needs more repetitions before treating it
as a precise estimate. Materialization outside switching is unchanged by the
optimization; small differences there are controls, not claimed improvements.

## What changed and what did not

Previously, lease planning called `switch_target_file_bytes` for every target
file. Each call opened a read transaction, resolved visibility, projected the
namespace, and constructed a validated span history context for a span file.
Diagnostic traces showed **25 contexts for 25 files** in the many-file fixture.

`Repository::for_each_switch_target_file` now streams target bytes under one
read snapshot and namespace projection. Ordinary span files share one lazily
constructed `SpanRenderContext`. Per-file capability/root checks and indexed
edit validation still run. Graph rendering, semantic conflict resolution,
smudge filtering and symlink handling retain their existing behavior.
Name-conflict sides retain their existing renderer and are not claimed to share
this context; these benchmarks contain no genuine conflicts.

The visitor hashes one file's exact output into its lease at a time. No extra
full-checkout byte cache is retained. The context dies with the read snapshot;
it is not reused across views, transactions, publication, execution or recovery.
Preflight rendering, operation preparation, effect leases, receipts and
independent execution validation remain intact. Immutable authored changes,
causality and provenance are not rewritten.

Diagnostic samples (not performance distributions) show lease-planning context
counts dropping **25 → 1**. Span file-lease planning drops from roughly
93–103 ms to 54–68 ms across the four switch directions in those traces.
These logged times include diagnostic overhead and are not substitutes for
the untraced end-to-end measurements above.

`ATOMIC_TRACE_SWITCH` now emits namespace, preflight render, nonfile planning,
file lease, operation preparation and execution timings.
`ATOMIC_TRACE_SPAN_RENDER` emits context construction count/timing information.
Both are opt-in diagnostics; the benchmark runner strips inherited `ATOMIC_*`
variables, so diagnostic runs invoke the pinned example directly with a new
output path. Execution timing includes initial publication and preparatory
effects but not final working-copy verification; namespace timing starts after
the initial working-copy identity/state reads. The diagnostics do not account
for every part of total switch time.

## Ordering correction

The initial seven-sample runs revealed a strong first-position penalty in
many-file forward switches. Seven alternating samples give one backend four
first-position runs and the other three, biasing comparisons of aggregate
medians. No underlying cause of that ordering effect has been established.

The runner now defaults to **eight** measured iterations and reports separate
first/second-position distributions. Balanced runs above use eight iterations,
one excluded warmup and two Rayon threads: four observations of each order per
backend/case. Many-file run scheduling was before → after → after → before;
hot-file scheduling was before → after. Everything ran sequentially, without
build/test load overlapping measurements. OS caches and unrelated system load
remain uncontrolled.

The improvement holds within both forward-switch order groups:

| Backend/version | First-position medians (two runs) | Second-position medians (two runs) |
|---|---:|---:|
| Graph before | 209.834 / 207.440 | 170.168 / 168.343 |
| Graph after | 208.683 / 210.781 | 170.082 / 171.105 |
| Span before | 245.613 / 244.396 | 204.891 / 204.594 |
| Span after | 200.767 / 205.687 | 168.992 / 169.104 |

## Validation and retained evidence

- Repository library: **1,556 passed, 0 failed, 3 ignored** (ignored is not pass).
- Native span integration suite: **30 passed, 0 failed, 0 ignored**.
- Benchmark smoke: passed, exercising all eight paired cases.
- Three new unit regressions cover graph/span/mixed histories, filtered bytes,
  empty/missing paths, view isolation, visitor error propagation, symlink targets,
  and missing roots/edits in a later file after the shared context is constructed.
  Corruption failures preserve working bytes, working-copy record and head.
- Example/library Clippy, formatting and three runner unit tests passed.
- All six balanced reports validated: **768 checked backend samples** (384
  graph/span pairs), with four first/second samples per backend/case/run.

Benchmark artifacts remain under the same approved scratch root as the initial
baseline:

```text
/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/span-harness01-1gFvn7/
  switch-balanced-before-many-01/
  switch-balanced-before-many-02/
  switch-balanced-after-many-01/
  switch-balanced-after-many-02/
  switch-balanced-before-hot-01/
  switch-balanced-after-hot-01/
  switch-profile-before.{json,log}
  switch-profile-after.{json,log}
  switch-fix-library-tests.log
  switch-fix-span-tests.log
  switch-fix-smoke.log
```

Before executable SHA-256 (diagnostics added, original per-file planning):
`eaacb4276c37bb02bb87d33fe133ebadb30d522730a3d969307d1fde7bba8056`.
After executable SHA-256:
`578762e8faa992934c2a571927d074262efacecb3f4a5ebcc03316c2b9cc6f58`.
The after executable's fingerprinted offline release build is retained in
`switch-after-many-01/`. Balanced runs pin those exact supplied executables;
their source association is explicitly marked unverified by the runner and
correlated here by hash. Runner order-summary/default changes were made after
building the after executable; neither executable changed during these runs.

The demo checkout was not modified. No remote push, Git-history rewrite,
validation bypass or unsupported-case pass is part of this work.

## Next investigation

Keep the balanced ordering and retained baselines for subsequent changes.
Many-file switch time now includes substantial non-rendering work: exact
filesystem observations, durable operation preparation and receipted execution.
Measure those before introducing another cache. The first-position effect also
deserves isolation (for example independently scheduled cases and a fixed-byte,
fixed-history file-count sweep) rather than guessing at its cause.

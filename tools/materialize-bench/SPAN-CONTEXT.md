# Span-context phases and transaction-local reuse

Follow-up: [verified loading and repeated decoded-data reuse](VERIFIED-LOADING.md)
measures a substantial improvement and documents phase pipeline version 2.

## Implemented

`SpanRenderContext::build_metrics()` now exposes wall-clock construction phases
and lookup counters. `ATOMIC_TRACE_SPAN_RENDER=1` emits the same metrics once
per context. The imported-history probe retains nested
`span_context_phases_ns` and `span_context_counts` separately from its outer
open/render/close phases; do not sum nested and outer timings together.

Phases:

- `resolve_changes`: resolve visible change hashes.
- `load_changes`: load/decode complete changes in bounded parallel batches.
- `verify_hashes`: canonical hash checking and extraction of render-required data.
- `validate_dependencies`: compare indexed and hashed dependencies and resolve IDs.
- `assemble_edits`: construct exact expected per-inode edit sets.
- `causal_proof`: build bounded reachability from the already checked adjacency.
- `total`: construction including allocation and loop overhead, excluding trace
  output. It need not equal the sum of phase medians.

Loading and hashing are separate parallel stages of at most 64 changes, so these
are wall-clock phase times, not sums of worker CPU times. This also temporarily
retains full change metadata for one bounded batch. Failures are still consumed
in dependency order; a later load failure cannot mask an earlier hash failure.

Construction-local maps reuse:

- One indexed dependency-list read per visible change, used both for comparison
  against hashed dependencies and for causal-proof construction.
- Successful `INTERNAL` lookups per referenced hash.
- Successful portable-file-to-inode lookups per `(owner hash, file ordinal)`.

The inverse-ID cache is **not** populated merely from `EXTERNAL`: the actual
inverse mapping must still exist. These caches are local to construction and
do not cross transactions or publication. Per-file root/edit/index checks stay
in the renderer. The proof remains bounded by the existing budget, rejects
invalid topological order, and never treats numeric NodeId order as causality.
The existing checked fallback remains when the proof does not fit.

No change identity, provenance, schema, capability, migration or operation-journal
contract changes. This does not convert existing graph history into span history.
No persistent verification bit or validation bypass was added.

## Measured outcome: useful diagnosis, no demonstrated cold speedup

Balanced cold measurements reuse the same explicitly repaired Hyperfine
fixtures as the [first imported smoke](IMPORTED-COLD-RESULTS.md): 1,097 Git
first-parent commits, 75 selected files, 1,098 visible Atomic changes including
native bootstrap. Eight iterations per run, two vCPUs / two Rayon threads,
8 GiB guest, no warmups. Scheduling: before → after → after → before.

Median milliseconds, showing both separate runs:

| Case/backend | Before | After |
|---|---:|---:|
| Span render total | 252.749 / 253.190 | 254.085 / 255.686 |
| Span full materialization | 966.453 / 991.761 | 965.450 / 962.131 |
| Graph render total (control) | 2,535.972 / 2,659.396 | 2,508.129 / 2,520.813 |
| Graph full materialization (control) | 5,737.083 / 5,821.720 | 5,691.245 / 5,706.087 |

Rendering is essentially unchanged (slightly slower in these runs), and the
small materialization differences do not establish a reliable optimization
gain given control-run drift. **The 100–200 ms materialization target remains
unmet.** This pass demonstrates removal of repeated lookups, not a large cold
latency improvement. Instrumentation/staging and reuse changed together; there
is no instrumentation-only ablation here.

Candidate span-context phase medians across the two untraced runs:

| Phase | Run 1 | Run 2 |
|---|---:|---:|
| Resolve visible hashes | 0.288 ms | 0.289 ms |
| Load/decode changes | **121.227 ms** | **121.281 ms** |
| Canonical hash verification / render-data extraction | **90.185 ms** | **92.773 ms** |
| Validate dependencies / resolve IDs | 1.510 ms | 1.507 ms |
| Assemble expected edits | 0.831 ms | 0.851 ms |
| Causal proof | 0.102 ms | 0.100 ms |
| Total construction | 214.623 ms | 215.533 ms |

The load and hash phases dominate. Importantly, `Change::hash()` serializes the
complete V3 change into a temporary buffer; this phase is not just Blake3 on an
already available byte slice. Investigating a verified reader that avoids an
unnecessary decode/re-encode round trip is now higher value than another cache
for dependency or file-ID lookup. It must preserve the actual canonical identity
checks; the measurements do not authorize trusting an unchecked loader.

Deterministic counters per candidate render context:

```text
visible changes / dependency-list reads: 1,098 / 1,098
expected edits:                         4,278
internal-ID database lookups:           1,096
internal-ID cache hits:                 9,058
portable-file database lookups:           116
portable-file cache hits:               4,162
```

The original constructor read dependency lists again while building the proof
and performed individual inverse-ID / file-ID lookups. The new counters and
counting-transaction unit test demonstrate reuse without inferring it from
elapsed time.

## Separate diagnostic run

`imported_guest.py measure --trace-span-context` explicitly enables tracing and
labels its outcome `diagnostic-guest-cache-cold`. Do not pool it with untraced
baseline timings. Nested render metrics are available without trace output.

Two diagnostic full-span-materialization samples each constructed **two
contexts**, costing approximately **214–217 ms apiece**. This confirms repeated
setup across the real materialization path; it does not prove that an entire
context can safely cross its transaction boundary. A next implementation should
consider reusable verified immutable data with explicit revalidation of the
relevant new snapshot, rather than blindly carrying a proof across publication.

Remaining materialization time also includes prewarming, planning, filesystem
observations, durable operation preparation, receipts and execution; it cannot
all be called disk-write overhead by subtracting the render probe.

## Validation and limitations

- **3,747 core library tests passed**, none ignored.
- **1,556 repository library tests passed**, 3 existing tests ignored.
- **30 native span integration tests passed**, none ignored.
- Both benchmark example smokes passed. The imported probe smoke checks the new
  nested phase/counter fields as well as bytes and operation heads.
- Five new core tests cover exact lookup counts, snapshot isolation, missing
  inverse mappings, changed dependency indexes, canonical hash corruption,
  deterministic error priority, bounded proofs and non-NodeId causal ordering.
- Strict core-library and imported-example Clippy passed. Broader core-test
  Clippy was blocked by two pre-existing lints: the `projection_ids` test helper's
  complex return type and `git_closure.rs`'s `vec![hash(1), hash(2)]`. They were not
  suppressed or counted as a pass.
- **128 untraced backend samples** and **8 separate diagnostic samples** passed
  selected-file byte/kind/mode and completed-head checks. Every materialization
  wrote 75 files / 593,962 bytes. Git indexes were unchanged.

These are guest-page-cache-cold observations after guest `sync` / `drop_caches=3`.
Host disk-image and physical storage caches are uncontrolled. Both backend
fixtures required explicit directory-cache repair during the earlier setup;
the ordinary fresh-import bug is still not fixed. This pass does not change
those qualifications or claim clean fresh-import certification. The demo
checkout was not modified.

## Identities and evidence

```text
source archive SHA-256: 55939776748db48f6cecbe10894fec7c45943480b76c4079cb99361781442463
baseline probe SHA-256: 46de9a01e69f0182f4741eb37169c498ec45f120c9a2234ab333ac7d3139746d
candidate probe SHA-256: eb59a7732289e6fd14346d4b3a8fce02582c7a8d0cc0f57e4547ecaf0547969a
evidence archive SHA-256: 67c501a66b96c033c86a122172cd28777cc0dd22e4d7fe384593076838f1e479
```

Approved host scratch directory:

```text
/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/span-harness01-1gFvn7/span-context-vm-01/
```

It retains source identities/archive, full test logs and
`span-context-evidence-01.tar.gz`. The evidence includes all per-sample reports,
stdout/stderr, cache-reset observations, phase/counter metrics, candidate binary,
build log, runner scripts and prepared fixture identities. The baseline binary
is pinned in the prior smoke evidence. Guest runs are retained as
`/workspace/span-context-{before,after}-0{1,2}` and
`/workspace/span-context-diagnostic-01`.

# Verified loading and operation-local decoded-data reuse

## Result

Matched cold-guest runs show **about 3.5× faster full span materialization** than
the preceding phase/reuse build. It is now **310–311 ms**, not yet the 200 ms
upper target. Render-only meets that latency range, but is not the acceptance
case and must not be substituted for full materialization.

Eight measured iterations per run; cells show the two separate run medians:

| Case/backend | Before | After |
|---|---:|---:|
| Span validated render API total | 277.436 / 277.013 ms | **150.056 / 150.909 ms** |
| Span full materialization API total | 1,083.927 / 1,083.482 ms | **311.478 / 309.741 ms** |
| Graph render API total (control) | 2,729.901 / 2,798.006 ms | 2,737.330 / 2,742.637 ms |
| Graph materialization API total (control) | 5,977.694 / 6,020.893 ms | 5,947.551 / 5,982.658 ms |

After-change full-span p95 observations are **317.807 / 316.955 ms**. With eight
samples, nearest-rank p95 is the maximum, not a reliable tail estimate. Both
median and p95 remain above 200 ms. Scheduling was before → after → after →
before, with equal first/second backend counts within each run. No builds or
tests overlapped measurement. These are combined changes, not an isolated
ablation of each component.

## What changed

### Checked original-byte identity instead of decode/re-encode identity

The filesystem store already uses `Change::deserialize`, which verifies hashed
encoded sections against the trailer and performs version-appropriate section
manifest and decoded-data validation. Span construction then called
`Change::hash()`, serializing, chunking and compressing the complete value again.

`VerifiedChange` is an opaque, immutable witness produced either by:

- Checked decoding of the actual original bytes, with complete consumption and
  no trailing garbage; or
- Conservative canonical serialization of a decoded value for stores that do
  not provide a checked encoded reader.

It exposes no mutable change reference, and is not deserializable as a trusted
cache marker. The filesystem path additionally binds the verified hash to the
requested filename identity; a valid change placed under another hash refuses.
This verifies content identity and structure, **not an author's cryptographic
signature**. Existing signature/provenance handling remains separate.

The original encoded identity is authoritative. A valid change with different
content chunk boundaries can have an original hash different from a fresh
writer's hash of its decoded value. The new regression verifies that the original
hash is retained, not silently normalized or rewritten. No authored object,
historical provenance, semantic ownership or change reference is replaced.

`ChangeStore::get_verified_change` defaults to the conservative checked-value
path. Unknown/custom stores therefore do not gain an unchecked fast path.
The existing general `get_change` / filesystem decoded LRU contract is unchanged.

### Scoped reuse with real source revalidation

Direct journaled materialization creates a `VerifiedChangeCache` for the
operation. It shares decoded immutable data between planning and execution,
**not** a transaction, namespace projection, dependency index or causal proof.

Every filesystem cache hit rereads the actual file and computes an exact-byte
fingerprint, including unhashed notes and signature bytes. Only a match to the
previously checked source reuses its immutable decoded value. Changed source
bytes require another complete checked decode; deletion, truncation, invalid
framing, hash corruption and wrong-identity replacement fail closed. No mtime,
size, prior receipt or global decoded LRU is sufficient to authorize reuse.
Changed valid unhashed notes are reread without rewriting the authored identity.

The cache is limited to the visible count or 4,096 entries, whichever is smaller.
This is an **entry-count limit, not a decoded-byte budget**. Full metadata may
still be large. Further entries are verified without admission; there is no
global cache, persistent verification bit, eviction thrash or across-operation
trust. This benchmark does not certify memory use for arbitrarily large history.

Each context still rebuilds checked dependency adjacency/causal proof and
compares span roots/edits against its own read snapshot. Filesystem effect leases,
journal preparation, receipts, execution and independent recovery remain intact.
Filtered output is not cached as if filters were immutable. The source checks
are observations, not a new cross-process filesystem lock or a complete
concurrent-mutation certification.

### Avoid redundant whole-closure prewarming

When span construction has already checked every visible change, materialization
does not decode the same closure again merely to populate the ordinary graph LRU.
The external-hash map remains complete. Graph-only materialization keeps its
original prewarm path; non-visible origins retain their explicit preload check.
Mixed graph files retain checked lazy content reads. A missing source is not
converted into a successful skip.

## Phase evidence and interpretation

Render-context phase medians across the two candidate runs:

```text
checked load/decode:                 102.328 / 102.366 ms
witness binding / render extraction: 17.543 / 17.874 ms
dependency validation:                 1.649 / 1.648 ms
expected-edit assembly:                0.978 / 0.972 ms
causal proof:                          0.126 / 0.129 ms
total construction:                  123.329 / 123.585 ms
```

`SpanContextBuildMetrics.pipeline_version` is now **2**. The retained legacy
field `verify_hashes` means witness-to-request binding and render-data extraction,
including release of decoded metadata when no operation cache retains it.
Actual original-byte hashing is part of `load_changes`; generic decoded-only
stores also perform their conservative serialization check there. Do not read
the smaller `verify_hashes` number as proof that hashing was omitted, or compare
individual pipeline-1/2 phase keys as identical workloads. Totals include all
construction work and teardown; repository close also remains timed.

The separately labeled diagnostic run confirms two contexts in each full
materialization:

| Diagnostic sample | First context | Second context |
|---|---:|---:|
| 0 | 117.154 ms | **8.223 ms** |
| 1 | 117.037 ms | **8.076 ms** |

Second-context checked loading is **4.87–4.90 ms**, with source-file rereads and
fingerprints still performed. Dependency/span index checks are not carried over
from the first transaction. This demonstrates reuse of the expensive decode,
not removal of validation. The same physical source can still be rejected after
planning if deleted or corrupted before execution.

The remaining materialization time includes namespace/attribute work, lease
observations, mode probes, durable preparation, filesystem effects, receipts
and final verification. More detailed execution/journal timings are the next
useful experiment; subtracting render medians does not prove it is all disk-write
overhead. The 100–200 ms goal remains open.

## Validation

- **3,752 core library tests passed**, none ignored.
- **1,559 repository library tests passed**, 3 existing tests ignored.
- **30 native-span integration tests passed**, none ignored.
- Both benchmark smokes, strict core/repository library and imported-example
  Clippy, and workspace check passed.
- New regressions cover immutable witness access, original non-default chunk
  identity, corrupted/truncated/trailing bytes, conservative custom-store cache
  revalidation, entry admission bounds, actual filesystem replacement/deletion,
  changed notes, changed span indexes, and source rejection before executor
  effects after a successful plan.
- **128 untraced backend samples** and **8 separate diagnostic samples** passed
  byte/kind/mode and single completed-operation-head checks. Every materialization
  wrote 75 files / 593,962 bytes and skipped the existing native bootstrap file.
- The retained original CLI independently passed final doctor checks in both
  fixtures. Immutable object-tree digests, recorded history and Git indexes
  matched their pre-benchmark identities.

## Scope and reproducibility

The same pinned Hyperfine tip, 1,097 first-parent commits / 75 selected files,
two vCPUs, 8 GiB guest memory, two Rayon threads and guest-local ext4 storage are
used throughout. Each fresh process follows baseline restoration, guest `sync`
and `drop_caches=3`. API totals include open/materialize-or-render/close and
exclude setup/verification; separate process-wall numbers include verification.

Host disk-image and physical storage caches are uncontrolled: these are
**guest-page-cache-cold** results. Both fixtures remain explicitly repaired
imported states; the eight stale-directory fresh-import bug is not fixed or
counted as a pass. This does not benchmark import speed, Git-shim branch switching,
network transport or power loss. No new schema/capability or graph-to-span history
migration was introduced. The demo checkout remains untouched.

SHA-256 identities:

```text
source archive: 54373ccc5b0957d1f72409f89003c7f9de67f6447b5636fffeb84d9bced73735
baseline probe: eb59a7732289e6fd14346d4b3a8fce02582c7a8d0cc0f57e4547ecaf0547969a
candidate probe: 854fe5baf9258ec36a9f03d9be12012e1ed0dc905c745d60ee79eb7d93867b10
evidence archive: 82abd5ab11f08682fcccaa195c638ea322735172e8779e7c1d56233aa654d25d
```

Host scratch artifacts:

```text
/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/span-harness01-1gFvn7/verified-loading-vm-01/
  source.json
  source.tar.gz
  core-tests.log
  repository-tests.log
  native-span-tests.log
  example-tests.log
  workspace-check.log
  verified-loading-evidence-01.tar.gz
```

The evidence archive includes per-sample reports/stdout/stderr, cache-reset
observations, nested phase/counter metrics, candidate binary, build log/identities,
runner scripts, fixture identities and final doctor output. The pinned baseline
is retained in the preceding span-context evidence. Guest run directories are
`/workspace/verified-loading-{before,after}-0{1,2}` and
`/workspace/verified-loading-diagnostic-01`.

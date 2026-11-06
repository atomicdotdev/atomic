# First imported Hyperfine cold-guest smoke

Follow-up: [phase breakdown and transaction-local reuse](SPAN-CONTEXT.md).

**The span speedup is present on the real imported history, but the 100–200 ms
materialization goal is not met.** These are two samples per backend/case, not
a stable performance distribution. The reported p95 for two observations is
just the maximum; do not interpret it as a reliable tail-latency estimate.

## Measurements

Fresh-process API totals include repository open and close, with verification
outside timing. Graph and spans restore the same selected Git files.

| Case | Graph median | Span median | Graph / span |
|---|---:|---:|---:|
| Validated rendering (no file writes) | 2,472.2 ms | 247.8 ms | 9.98× |
| Full journaled materialization | 5,707.9 ms | 926.5 ms | 6.16× |

Full span-materialization samples: **924.7 and 928.3 ms**. Both median and
maximum exceed 200 ms. Each full-materialization sample writes **75 files /
593,962 bytes**, skips one existing native bootstrap file, verifies the selected
Git bytes/kinds/executable bits and a single completed operation head, and leaves
the Git index unchanged.

Span render-probe phase medians:

| Phase | Median |
|---|---:|
| Repository open, including working-copy identification | 8.8 ms |
| Read/visibility setup | 8.3 ms |
| Validated span-context construction | **208.5 ms** |
| Render selected file bytes | **6.1 ms** |
| Repository close | 15.8 ms |

Graph rendering alone takes about 2,446.4 ms. The span context already consumes
the entire target budget before file writes. This identifies a concrete next
investigation: profile content loading/hashing, declared/indexed dependency
checks, expected-edit construction and causal-proof setup, then measure how
often the real materializer repeats that work during plan and execution.

Do not attribute the remaining difference between the render probe and full
materialization entirely to filesystem writes: those cases have different
control paths, and materialization also plans, validates, renders and journals.
No further production performance shortcut was introduced for this smoke.

## Fixture and cache boundary

- Pinned Hyperfine Git tip: `c5161524980522320c7d3425e714cc0965a869a5`.
- **1,097 first-parent commits**, complete **75-file selected Git tree**.
- Dedicated smolvm: `atomic-hyperfine-cold-20261106`.
- Linux/aarch64, two vCPUs, 8 GiB configured memory, two Rayon threads.
- Guest-local ext4 workspace, not a host-shared source/repository mount.
- Before each fresh process: restore the closed baseline database at its original
  canonical working-copy location, prepare selected files, guest `sync`, then
  guest `drop_caches=3`. No warmups. Both backend orders are represented once.
- Host disk-image/physical storage caches are uncontrolled. These results are
  **guest-page-cache-cold**, not host-cache-cold or physical-disk-cold.
- Import, cache reset, fixture reset, VM startup and post-sample verification
  are excluded from API totals. Separate process-wall observations include
  verification and are not interchangeable with API latency.

The source archive is a frozen export of the existing implementation plus the
single-shot measurement example. It excludes host Git/Atomic/OpenCode metadata
and credentials. Both graph and span imports use the same feature-enabled
release CLI. The measured example uses normal optimized release settings, not
a debug build. Failed 4/8 GiB builds are failures, not benchmark samples; the CLI
linked at 16 GiB and the guest returned to 8 GiB before import and measurement.

## Correctness caveat: explicit setup repair in both backends

The original fresh graph-import smoke stopped at `doctor check`; it ran **no
timed samples**. A separate fixture retry revealed the same eight stale
`DIRECTORIES` rows in **both graph and span imports** (inodes 9, 15, 22, 30,
40, 50, 144 and 149).

Only the disposable retry fixtures received explicit
`atomic doctor repair-native-indexes`. Required post-repair checks passed in
both: **806 consistent derived-index rows and 76 clean working-copy files**.
The setup also verified unchanged immutable change files, recorded history,
Git index and all selected Git checkout bytes before saving the baseline.
Pre-repair databases and failed/passing doctor logs are retained.

Consequently these are **post-repair imported-state measurements**, not a clean
ordinary fresh-import pass. The stale-directory import bug remains unfixed.
Repair was not timed, not silently performed by a read, and not applied to the
demo checkout. No unsupported coverage or initial doctor failure counts as a pass.

All **eight measured backend samples** passed their byte/mode/head checks.
Host correctness smoke and Clippy for the new Rust example passed; six runner
unit tests passed. Those tests do not substitute for the imported-state evidence.

## Identities and artifacts

SHA-256 identities:

```text
source archive: fd7372f5ebd72fedce3d779a1bb74128f71b722ea3a955355e5fcf0227a696d2
Git bundle:    0aa93f6a5679412b4250a08f85503e56b4e367b28561a4829dce6461eb3c6758
Linux CLI:     379211242b58ef0e93ddaf910920f24446046b4a6de3f8bf9f7971eec6ad2cd7
Linux probe:   46de9a01e69f0182f4741eb37169c498ec45f120c9a2234ab333ac7d3139746d
evidence tar:  797c32e459ce3e0e1459efa07af1aa456d042ac27c3c546a0938222a23fa5f64
```

Host evidence is retained under:

```text
/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/span-harness01-1gFvn7/imported-cold-vm-01/
  source.json
  linux-build.json
  cold-smoke-run.json
  cold-smoke-evidence-01.tar.gz
```

The archive includes per-sample JSON/stdout/stderr, the pinned probe, cache-reset
observations, prepared fixture identities, Git manifest, doctor/repair logs and
build logs (including the 8 GiB CLI OOM). Guest fixtures remain at
`/workspace/hyperfine-smoke-repaired-fixtures`; the original failed graph fixture
is separate at `/workspace/hyperfine-smoke-fixtures`. Raw results are at
`/workspace/cold-smoke-repaired-01`.

Import setup observations were 34.4 s graph and 53.2 s spans; these are single
observations, **not import performance claims**. Baseline databases were
67,375,104 and 12,193,792 bytes respectively, uncompacted. They are not a new
compacted storage benchmark.

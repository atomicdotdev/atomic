# Imported Hyperfine materialization: cold guest benchmark

The [first real Hyperfine smoke results](IMPORTED-COLD-RESULTS.md) are now
available. They are post-repair, guest-page-cache-cold measurements; the 200 ms
materialization target was not met.
The [verified-loading follow-up](VERIFIED-LOADING.md) reduces full materialization
to about 310 ms under the same qualifications; render-only is not the goal case.

**Performance goal:** approximately 100–200 ms for materialization **after** a Git
import. Importing all history is setup, not part of this latency target. Faster
than 100 ms is acceptable. Report both median and p95 against a 200 ms upper
target; correctness success is not proof the performance goal is met.

This benchmark reuses `SpanRenderContext`, the graph renderer and the ordinary
journaled `Repository::materialize` path. It does not replace existing span
storage, fake historical semantic authorship, bypass validation, or substitute
plain file copying for materialization.

## Two cases

- `render`: fresh-process repository open, visibility setup, validated content
  rendering of every selected Git file, and repository close. No working-copy
  writes. Useful for identifying the rendering/validation floor, **not** for
  satisfying the materialization target by itself.
- `materialize_missing`: fresh-process repository open, ordinary journaled
  materialization after removing the selected Git files, and repository close.
  Includes the real filesystem writes, leases, journal receipts and durability
  work. This is the target case. Native bootstrap files not present in the Git
  tree (for example `.atomicignore`) stay in place and are handled by the normal
  materializer, rather than silently dropping bootstrap history.

The example reports `total_ns` and phase times. Manifest parsing, byte/mode
verification and operation-head checks are outside the reported API total.
Process wall time is also retained, but includes those verification steps and
is labeled accordingly; it is not interchangeable with `total_ns`. VM boot,
Git import, fixture reset, `sync`, and cache reset are outside sample timing.

## Pinning and verification

The Hyperfine fixture is Git tip
`c5161524980522320c7d3425e714cc0965a869a5`, **1,097 first-parent commits**, with
**75 selected Git files**. Export its public Git history to a bundle and a
manifest generated from `git ls-tree -r -z <tip>` and `git cat-file blob <oid>`.
The manifest format is:

```json
{
  "git_tip": "c5161524980522320c7d3425e714cc0965a869a5",
  "first_parent_commits": 1097,
  "files": [
    {"path": "src/main.rs", "mode": "100644", "git_blob": "<Git blob id>", "sha256": "<SHA-256 of blob bytes>"}
  ]
}
```

Use the complete tree, not just the example entry. Preparation rejects unsafe
paths, duplicate entries, unsupported modes, mismatching blobs, incomplete tree
coverage, and the wrong tip/history length. It imports graph and spans using
the same experimental-span-enabled CLI binary and checks the unchanged Git
checkout/index and `doctor check` afterward. Import time and uncompacted database
sizes are retained separately.

The first full-Hyperfine setup smoke failed on graph import: `doctor check`
reported eight stale `DIRECTORIES` rows (inodes 9, 15, 22, 30, 40, 50, 144 and
149). No cold samples ran in that attempt. The original fixture is retained;
the harness does not silently suppress this fresh-import failure.

An explicit `prepare --repair-derived-indexes` retry may repair a **new
disposable fixture** before measurement. It preserves the pre-repair database
and failed doctor logs, runs `repair-native-indexes`, requires a passing doctor
check, and verifies unchanged immutable change files, recorded history, Git
index and expected checkout bytes. The report records whether each backend
needed repair. Such results are **post-repair imported-state measurements**,
not proof that ordinary fresh import passes doctor. Setup repair is not timed
and is never run on the demo checkout. Without that flag, doctor failure still
aborts preparation.

Every sample verifies SHA-256 bytes, file kinds and executable bits for all
selected files, plus a single verified working-copy operation head. Span render
probes require valid span roots; they do not fall back to graph output on a
missing index. The CLI and probe must be optimized Linux/aarch64 binaries built
from frozen source; macOS executables cannot run in the guest. The example's
debug smoke test checks the protocol but is not imported-history or cold-cache
performance evidence.

## Cache and storage policy

Use a dedicated named smolvm machine with no host credentials, SSH forwarding,
source-tree writable mount or demo-checkout access. Copy frozen source and the
public fixture bundle into its **guest-local ext4 workspace**. Build before
measurement and retain source/archive/executable identities and build logs.
Registry/dependency access is setup only; measurement does not need network.

The guest runner restores a closed baseline redb at its **original absolute
working-copy location**, prepares the selected files, calls guest `sync`, writes
`3` to `/proc/sys/vm/drop_caches`, and starts a fresh probe process. Failure to
reset caches fails the run. It refuses non-ext4 backing to avoid accidentally
calling a host-shared filesystem cold. It retains pre/post-reset `/proc/meminfo`.
Eight measured iterations alternate backend order evenly; there are no warmup
samples for this cold protocol. First/second-position distributions stay visible.

These are **guest-page-cache-cold** observations. Host disk-image and physical
storage caches are uncontrolled unless separately proven otherwise. Do not label
the numbers host-cache-cold or physical-disk-cold. Nothing purges host-wide caches.
Resetting guest caches is narrower and less disruptive than restarting the host.

Only the closed database is reset between samples. Immutable change files and
any prior recovery artifacts remain at their original location; the restored
database is authoritative for heads/receipts and does not reference later runs.
This is not a disk-image reset or a claim that the entire disk layout is fresh.
All fixture mutation is confined to the runner's newly created, ownership-marked
disposable directory. User files and other VMs are outside scope.

### smolvm setup lessons

On this installation, requests smaller than the runtime's 20 GiB storage / 10
GiB overlay templates triggered missing-`resize2fs` warnings and the disposable
workspace did not survive a restart. Using the matching template sizes passed a
sentinel write → graceful stop → start → read check. Verify persistence before
an expensive import; do not assume it from CLI help alone. No user VM was changed.

The first 4 GiB, two-job Linux release build was OOM-killed. With 8 GiB and one
Cargo worker the measurement probe built, but the full CLI's release LTO link
was also OOM-killed. The successful CLI link retry used 16 GiB for build only;
the guest returned to 8 GiB before import/measurement, with two vCPUs / two Rayon threads.
Keep the normal optimized release settings and features rather than changing
the measured workload to fit build memory. Preserve failed builds as failures,
not benchmark samples. Resource changes and builds must finish before timing.

## Commands inside the prepared guest

```sh
python3 /workspace/imported_guest.py prepare \
  --root /workspace/hyperfine-fixtures \
  --bundle /workspace/hyperfine.bundle \
  --manifest /workspace/hyperfine-manifest.json \
  --cli /workspace/source/target/release/atomic

python3 /workspace/imported_guest.py measure \
  --root /workspace/hyperfine-fixtures \
  --probe /workspace/source/target/release/examples/imported_materialize_bench \
  --output /workspace/cold-results-01 --iterations 8
```

Both root and output must be new directories. Copy results/build logs back to
approved host scratch storage. Retain raw per-sample JSON and stdout/stderr, not
just a median. No valid imported cold measurements or 200 ms target attainment
is implied merely by adding this harness.

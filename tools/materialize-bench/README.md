# Materialization benchmark

For the separate pinned Git-import / smolvm cold-guest protocol, see
[IMPORTED-COLD.md](IMPORTED-COLD.md). Native synthetic results below do not
establish its 100–200 ms materialization target.
The [span-context phase/reuse follow-up](SPAN-CONTEXT.md) identified the load/hash
bottleneck; lookup caching alone did not demonstrate a cold speedup.
[Verified loading and repeated-data reuse](VERIFIED-LOADING.md) subsequently
reduced full span materialization to about 310 ms, still above the 200 ms target.

Compare matched synthetic **native** graph and opt-in span histories. This is
not a Git-import benchmark. No production performance shortcuts are enabled.

```sh
python3 -B -m unittest discover -s tools/materialize-bench -p 'test_*.py'
cargo test --offline --locked -p atomic-repository --example materialize_bench
python3 -B tools/materialize-bench/run.py --build --profile many-files --output /approved/scratch/run-01
python3 -B tools/materialize-bench/run.py --binary /approved/scratch/run-01/materialize_bench --profile hot-file --output /approved/scratch/run-02
```

Use a new output directory outside the source tree. The release build is offline
and locked; Cargo must already have its dependencies. The runner fingerprints
source before/after build and measurement, copies the executable, and checks its
identity after running. Do not edit source during a run. A caller-supplied binary
has explicitly **unverified source association**; correlate its SHA-256 with the
original build's `run.json`. Debug runs require `--allow-debug` and are smoke
tests, never performance evidence.

Defaults: eight measured iterations, one excluded warmup, two Rayon threads.
`many-files` uses 24 files × 128 lines and 24 hot-file edits; `hot-file` uses one
4096-line file and 48 edits; `smoke` uses two files × eight lines and two edits.
Every fixture also includes an empty file. Override with `--files`, `--lines`,
`--edits`, `--iterations`, `--warmups`, and `--threads`.

## Cases and timing boundaries

- `render_session`: render base-view content using an existing repository handle.
- `render_reopened`: drop/reopen the repository, then render; includes open cost.
- `materialize_noop`: ordinary journaled materialization of already-correct files.
- `materialize_missing`: same API after removing only generated tracked files.
- `switch_one_forward/back`: switch to/from a view with one changed line.
- `switch_many_forward/back`: switch to/from a view changing one line per file.

Render probes report read/closure setup, span-context validation, and rendering
separately. End-to-end cases use the real journaled APIs; the probe is not a
substitute for those timings. Setup, deleting fixture files, and post-sample
byte/mode/head verification are outside timing. Render allocation/teardown may
contribute to total time beyond the individually measured phases.

Backends alternate order per iteration; cases stay in fixed order so forward/
back switches remain paired. These are sequential observations of **uncontrolled
OS caches**, not cold-cache measurements. Reopened handles are not cold disks.
Fixtures, verification and prior cases warm caches. Eight samples give only a
rough distribution: nearest-rank p95 is the maximum for eight samples. Use an
even iteration count to balance backend ordering. First/second-position timing
distributions are retained separately: first-position one-file switches showed
a sizable systematic penalty in initial runs, and seven samples overweighted
one order in each backend's median. This is not proof of the underlying cause.
Repeat runs, reverse scheduling or use more samples before claiming small
improvements. Odd counts remain available for correctness smoke tests; the
metadata flags their unbalanced backend ordering.

`raw.json` retains all sample times, phase times, counters, config, executable
Blake3 and setup sizes. `run.json` retains SHA-256 identity, source fingerprint,
commands, outcome, medians/p95 and logs. A graph/span median ratio above one
favors spans. Database sizes are post-sample, uncompacted sizes, not a storage
efficiency measurement. Errors and incomplete/duplicate samples fail the run;
they are not passes. Fixtures are disposable; `--keep-fixtures` retains them.

The benchmark checks expected bytes and modes after every sample and verifies a
single completed working-copy operation head. It does not certify imported
history, genuine conflicts, cold caches, transport, watchers, crashes or power
loss. Keep provenance, causal validation, leases and receipts intact when
optimizing; cache verified work instead of bypassing checks.

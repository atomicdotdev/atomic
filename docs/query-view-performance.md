# Recorded-query resource diagnostic

Intent 11 optimizes recorded `query code` and KG content promotion. It does not
change the existing ambient ignore policy, graph visibility, or persistent indexes.

## Reproduce

Run serially, with no competing builds replacing the executable:

```sh
rustc --version
git rev-parse HEAD
cargo test -p atomic-repository --features query-test-instrumentation --test query_view_performance --release --locked -- --ignored --test-threads=1 --nocapture
```

The ignored diagnostic constructs 2,000 approximately 4 KiB recorded sources, a
100,000-line dense source, exactly-10-MiB and cutoff+1 files, and a 20-level view
chain. Setup is outside the timings. Each exact-file, prefix, and all-files query
runs at display limits 0, 1, and 50, with one warmup and five measured repeats.
A final arm runs the all-files query through the agent tool executor on the same
fixture. It reports minimum/median/maximum, exact totals, files retrieved,
cumulative bytes retained, consumer output-buffer high-water, and
displayed-record high-water.
These are warm ChangeStore/OS-cache measurements, not cold-disk claims. No cache
flush is attempted. Deterministic resource assertions, not elapsed time, are gates.

Instrumentation is thread-local and opt-in through `query-test-instrumentation`
(also enabled for repository unit tests). It measures synchronous calls on their
calling thread; it does not propagate into service worker threads. Default builds
do not expose the measurement API. No public query response or protobuf changes
are needed.

## Cost and compatibility contract

- Path-union and existing syntext glob/type/exclude filtering precede content I/O.
- View closure, path claims, and eligible inode attributes are validated before
  request filters. Narrow searches still fail on invalid global metadata.
- Content-only corruption outside the selected paths is deliberately unread;
  selected content errors propagate rather than falling back to disk or an index.
- Exact totals and directory counts require scanning every eligible file, even
  when displaying zero results. Top-ten facet ties sort by directory path.
- The consumer retains at most one file output of 10 MiB+1, and no more than the
  requested number of display records. Oversized content is not searched as a
  truncated file. A matching line can itself reach the raw-byte cutoff; lossy
  UTF-8 display can expand those bytes in retained result text.
- Graph/visibility metadata, ChangeStore caches, renderer span buffers, and
  semantic-merge scratch are separate costs, not included in the consumer bound.
  No claim is made that the whole request uses constant memory or O(limit) time.
- KG search preserves its ranked 2,000-match promotion window and per-file
  `content_matches` metadata, rather than substituting exhaustive per-file counts.
  Vault-only class searches do not read irrelevant source content.
- Caller parsing retains its full-source requirements; it is not made streaming
  or bounded-memory by this optimization. No persistent cache is introduced.

## Measurements

Measured on an Apple M5 Max, 48 GiB RAM, macOS, Rust 1.98.0
(`88d9e12ae`, 2026-08-18), with the checked-in Cargo.lock and release profile.
Both stages used Git base `e489ed4356c047ae7ec355a4a25ac0527c10ffcf` plus local
intent-11 work; these are **uncommitted source stages**, not two invented commit
hashes. The baseline was run before the selective projection/collector rewrite,
with test instrumentation and dormant bounded-rendering APIs present. The final
stage used the new metadata-only projection, capped retrieval and streaming
collector. Release stages ran serially, so one build could not replace another
stage's running executable. The final table uses a foreground rerun after other
background checks had completed, with no concurrent builds. These elapsed times
remain diagnostic, not a machine-independent performance promise.

The final inspected query-source stamp is SHA-256
`cec225e96cf73ca517dd03fec9d735ce76e2a6a156605fb7956b016f5bc1ea14`.
It hashes sorted path + NUL + file bytes + NUL for `Cargo.lock`,
`atomic-core/src/output/repo/{file,mod}.rs`, `atomic-repository/Cargo.toml`,
`atomic-repository/src/{content_search,query_diagnostics}.rs`,
`atomic-repository/src/repository/{content,query,vault_triples}.rs`, and
`atomic-repository/tests/query_view_performance.rs`. The stamp includes the final
documentation/derive cleanups and the shared comparator; these do not alter the
recorded-query execution measured by the final benchmark.

The baseline command additionally set `QUERY_BENCH_BASELINE=1`, which disables
only the new resource assertions in this ignored test. That variable is not read
by production code. The failed initial fixture setup hit the record API's default
10-MiB limit; the benchmark explicitly raises that setup-only limit to 11 MiB to
record the deliberately oversized test file. It does not change search's cutoff.

Times below are median **[minimum–maximum]**, in milliseconds, over five warm
runs after one warmup. Setup and compilation are excluded.

| Scope | Display limit | Before (ms) | After (ms) |
|---|---:|---:|---:|
| Exact file | 0 | 183.45 [182.01–186.07] | 6.42 [6.32–6.49] |
| Exact file | 1 | 184.80 [182.95–191.16] | 6.46 [6.43–6.53] |
| Exact file | 50 | 185.40 [182.45–188.24] | 6.41 [6.40–6.43] |
| `src/` prefix | 0 | 212.64 [211.30–214.22] | 174.94 [173.79–175.59] |
| `src/` prefix | 1 | 213.68 [211.98–215.00] | 174.30 [172.68–178.97] |
| `src/` prefix | 50 | 210.87 [209.04–212.39] | 173.87 [173.40–174.34] |
| All files | 0 | 227.93 [213.20–272.88] | 177.74 [175.92–178.59] |
| All files | 1 | 219.28 [217.10–222.06] | 176.72 [175.38–178.10] |
| All files | 50 | 215.38 [211.24–221.32] | 177.46 [175.46–177.80] |

All before/after totals agree: 1 for the exact query, 102,000 for prefix/all.
The exact query went from 2,003 retrievals / 29,862,411 cumulative retained bytes
to **1 retrieval / 4,096 bytes**. Prefix queries now retrieve 2,001 files /
8,890,890 cumulative bytes. All-files queries still retrieve 2,003 files, but
retain 19,376,650 cumulative bytes because oversized content is rejected before
fully collecting it. Cumulative bytes are not simultaneous resident memory.

For prefix/all searches, displayed-record high-water fell from **102,000** to
**0 / 1 / 50** at those limits. The consumer-buffer high-water is now 4,096 bytes
for exact, 700,000 for prefix, and 10,485,760 for all-files queries. Those counters
exclude separately identified renderer/graph/ChangeStore costs; this is not an
RSS benchmark or a claim that the whole request is constant-memory. Machine load
and filesystem caches affect timings, so the diagnostic gates resource counts
and result equivalence, not these elapsed-time numbers.

## Agent tool path

The agent tool loop reaches the same recorded search through
`RepoToolExecutor::code_search`, with the view resolved from the working copy's
registered desired view. The tool adds one view-pointer resolution and JSON
serialization on top of the measured direct path; it retrieves the same files
and retains no more display records. The ignored benchmark measures both on the
same fixture: after the direct arms, it repoints the working copy at the deepest
view and runs the tool at display limit 50, asserting the exhaustive total
(102,000), the same 2,003 file retrievals, and a displayed-record high-water of
at most 50. The dropped trigram prefilter is therefore bounded by the same
documented costs as the direct path, with no additional retrieval or retention.

| Scope | Display limit | Direct median (ms) | Tool median (ms) |
|---|---:|---:|---:|
| All files | 50 | 176.26 | 201.44 |

Measured on the same run as the table above (Apple M5 Max, 48 GiB RAM, macOS,
Rust 1.98.0, release profile). The tool path's minimum/median/maximum was
200.21 / 201.44 / 211.43 ms against the direct path's 174.81 / 176.26 /
183.60 ms — roughly 25 ms of median overhead for the view-pointer resolution
and JSON serialization of the retained records, on a fixture where the search
itself costs ~176 ms. Its resource counters are identical to the direct path:
2,003 files retrieved, 19,376,650 cumulative retained bytes, 10,485,760-byte
consumer-buffer high-water, and a displayed-record high-water of 50. The
exhaustive total is the same 102,000. The dropped trigram prefilter therefore
costs nothing beyond the already-documented scan, and the tool adds no
retrieval or retention of its own.

Run the reproduce command above; the tool line is printed as
`tool limit=50 min=… median=… max=… resources=…` alongside the direct arms.
Elapsed times remain machine-dependent diagnostics; the deterministic resource
assertions are the gate.

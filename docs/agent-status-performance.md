# Agent status latency

Latest representative-history result (intent 17): warm ordinary release status
is about **0.61 s**, previously **3.46 s**, on a fresh isolated copy of the
reported Atomic import. First invocation after rebuilding was **1.95 s**.
See the large-history follow-up below; earlier measurements are retained as
experiment history, not current latency claims.

Work: `ATOM::continuouslee::15`, `::16`, `::17`.

## Fixture and baseline

The user reported slow status after importing `~/Runnables/atomic`. The user's
terminal had a shell-local PATH override; the coding harness's PATH does not
identify the binary they used. The earlier 6.170-second read-only forensic
measurement was **not** ordinary status: `--no-reconcile` builds a manifest for
its report, including full content reconstruction.

Ordinary status was profiled on a disposable APFS clone at
`atomic-status-fast-z8k_m4oi/repo` under the approved OpenCode temporary directory.
The copy has an independent `.atomic` and `.git`, no repository pointer or
hard-linked database, and was registered at its new physical location. The
fixture-only `prepare.rs` preserved the copied materialized baseline and V2
index under its new working-copy ID. No reconciliation, edits, reindexing or
binary/config replacement was performed in the user's original directory.

The fixture has 1,343 Git-tracked files, plus native `.atomicignore`, over 250
first-parent commits. All measured commands use explicit shim-2 binaries and
`ATOMIC_SERVICE=local`; these are local CLI measurements, not RPC service timing.

The previous release binary's ordinary `status -s` took **26.245 s** and
**25.254 s** in unsampled runs. A separately sampled run took 24.197 s and is not
used as the primary timing comparison. All outputs were clean.

## Causes and changes

1. **Watcher failure discarded valid content leases.** Automatic watcher
   selection fell back to a complete metadata scan because neither builtin
   fsmonitor nor Watchman was available. `verify_candidates` used the same flag
   for complete enumeration and content-cache invalidation. It consequently
   rehashed all 1,344 files and rendered their canonical graph content on every
   call, even with a populated clean index. Workspace entry's pending-edit
   capture and the command's own status each perform a status check.

   Enumeration fallback now retains valid clean-content leases. Missing or
   malformed cache rows and canonical policy/attribute/config/filter changes
   still invalidate leases. A failed incremental source still requires full
   enumeration; every file's metadata, identity, attributes, policy and racy
   timestamp checks remain in place. No watcher result alone proves cleanliness.
   Degradation metrics and typed fallback reasons remain visible.

2. **Attribute projection was repeated within one read transaction.** Status
   resolved mode/kind during candidate discovery and again during classification.
   It now retains those exact attribute projections for classification, including
   conflicts. This is request-local reuse, not a cross-view or persistent cache.

3. **A lone attribute writer walked its full ancestry.** A zero/one-event
   dependency frontier has no competing writers. It now returns immediately;
   multi-writer causal reduction, including concurrent equal-value writers,
   remains unchanged. Graph visibility/dependency validation is not replaced.

4. **Namespace span lookup did not reuse the existing index.** Status now
   projects the namespace through `CachedGraphTxn` over its own read transaction.
   That wrapper already implements the graph span-index optimization used by
   content reads. Delegating `PathClaimTxnT` to the underlying read transaction
   lets it serve namespace projection too. No persistent cache, new schema or
   weaker absent-file resurrection check is introduced; the span index is
   discarded with that projection's read transaction.

   Span lookup alone was not enough: the optimized-dev namespace phase still
   took about 1.37 s. `CachedGraphTxn` also now shares successful dependency and
   hash/ID lookups between files within that **same immutable read snapshot**.
   Repeated causal probes no longer reopen the dependency/identity tables for
   every pair in each file. Negative lookups are snapshot-local too; errors
   are not cached. A regression holds one cached read across a concurrent commit
   and verifies both old-snapshot stability and fresh-snapshot visibility.

After the first two changes only, optimized-dev status showed 1,344 leases and
zero content reads, reducing an unsampled repeated command to 8.284 s. This is
diagnostic evidence, **not** the final release result or a same-profile speedup.
The remaining single-writer optimization is included in the final build.

## Correctness and evidence

Tests cover warm fallback scans (watchers unavailable/off/CI/container/token
loss/overflow/malformed responses/timeouts), complete rediscovery after invalid
incremental output, canonical invalidation, racy files, same-size edits with
restored mtime, identity replacement, chmod, kind changes, deletion, conflicts and
filtered bytes. Real-worktree probes run only in the disposable fixture.

Artifacts, preparation source, phase logs, baseline timings and the final
benchmark script are preserved under `atomic-status-fast-z8k_m4oi/`. The final
script runs clean ordinary status, edit/repeat/revert, chmod/revert,
delete/restore and separate shim verification. Cold cache/relocated files may
still require content verification; full forensic projection is not a fast
ordinary-status substitute.

Targeted optimized-test runs passed: 20 change-source/convergence tests, 70
repository/status tests, 71 CLI status tests, 4 pristine attribute tests, 6
repository attribute tests and 17 adoption tests. These groups overlap; the
counts are not a claim of that many distinct tests. The warmed repository test
now asserts zero content reads and an actual lease hit, not merely clean output.

Before the cached-namespace change, a full repository library run passed
**1,504 tests**, with 3 manual drivers ignored. Release ordinary short status
then took 4.184 and 4.155 s in warm runs, and long status 4.202 s. The first
9.0-second run overlapped broader validation and is not a quiet warm figure.
Each status scan's V2 verification was down to roughly 35–53 ms with zero
content reads; namespace setup still cost about 1.4–1.6 s per scan. Edit and
revert probes passed, with snapshot capture contributing additional latency.

The first real chmod probe exposed an existing graph-only snapshot-capture
limitation: ordinary status refused with `attribute path 'research.md' has no
semantic trunk`. The **older, unchanged** binary at
`~/Projects/atomic/target/release/atomic` independently reproduced that exact
failure in 23.463 s on the disposable copy (`old-path-chmod.json`). This does
not identify the binary the user ran in their shell. The failure is preserved,
not counted as a passing chmod probe and not silently worked around by the
performance changes. Native mode/conflict unit tests pass; graph-only imported
attribute snapshot capture needs separate remediation.

## Remaining work, not a completed subsecond claim

The clean content-validation phase is now tens of milliseconds, but an ordinary
CLI command still pays workspace entry plus two namespace projections, including
historical absent-claim liveness checks. The snapshot protocol is deliberately
not skipped to obtain a faster number. Dirty checks can also pay snapshot
construction even when only one file needs content comparison.

An eventual cross-command topology/projection cache needs a proof keyed to the
**effective** graph/view closure (including ancestor changes), not just the
current view's own Merkle or Git HEAD. It also must remain honest about
conflicts, recorded deletions, reappearances and index repair. This work does
not introduce an unvalidated persistent cache to claim agent-ready latency.

## Final release measurement

Final release binary SHA-256:
`f10867f432068f0389df3ee9cb7830aeb13d270f9db3dd2846ab352d3e324966`.
Built with the configured release LTO profile. On the same disposable replica:

| Local CLI probe | Seconds | Outcome |
|---|---:|---|
| First clean short status | 7.756 | Clean; not a warm figure |
| Warm short status | 3.682 / 3.681 | Clean |
| Warm ordinary long status | 3.723 | Clean |
| Single edit / repeated edited status | 4.986 / 7.795 | Modification reported |
| Revert edited content | 4.529 | Clean |
| Chmod | 3.001 | Known pre-existing graph-only capture refusal |
| Restore mode | 3.752 | Clean |
| Delete file | 22.551 | Deletion reported; snapshot overhead remains |
| Restore deleted file | 13.125 | Clean |
| Separate shim verification | 13.091 | Every layer passes |

Warm short status is approximately **6.9× faster** than the 25.254-second prior
release run (about 85% less wall time). These are individual local runs; broader
regression validation also ran during this experiment, so they are not an
isolated multi-run statistical performance guarantee. The final script's
`all_cases_succeeded` is deliberately **false** for the known chmod refusal,
even though clean/edit/revert/delete/restore and final verification completed.
Initial timings/failure logs are retained under `first-release-probes/`; final
results and per-probe phase logs are in `after.json` and `after-*.{out,err}`.

This is a substantial safe reduction, **not** a subsecond or agent-ready
end-to-end result. Warm content checking now uses clean leases; namespace
projection, workspace entry and especially pending-edit snapshot construction
still need work. Native mode, policy, race and conflict regressions pass, but
that does not override the real graph-only chmod limitation above.

After the final full repository suite completed (**1,504 passed, 3 ignored**),
three additional unsampled, untraced short-status runs took **3.486, 3.466 and
3.455 seconds**, all clean. Evidence: `quiet-final.json`. The median is about
**7.3× faster** than the 25.254-second prior release observation (86% less time),
but remains multi-second. The final cache-reader tests also passed, including
snapshot isolation across a concurrent commit.

All ordinary-status, edit and mutation probes targeted only the disposable copy.
Read-only original-directory control-state fingerprints retained identical HEAD,
index, working-copy ID, current-view and checkpoint bytes/mtimes. The original
database's mtime changed during the later observation window, with unchanged
file size; no global claim that other processes left it unchanged is made.
See `source-observation-final.json`; do not treat that report's aggregate
`unchanged=false` as evidence of a status test running against the original.

## Verified import → V2 handoff

Intent `ATOM::continuouslee::16` closes a separate import integration gap:
the command's legacy `reindex_working_copy` writes `FILE_INDEX`, while modern
status consumes `FILE_INDEX_V2`. V2 rows observed later can have been populated
by native record/status calls; their presence did not prove import seeded V2.

The existing `refresh_checkpoint_after_verified_import` boundary already builds
the actual canonical `ProjectTree` after private `.atomicignore`/vault setup and
checks its Git projection against the imported tip. It now passes that project
and the resulting equivalence proof to `seed_verified_import_file_index_v2`
before publishing the checkpoint. The seeder does **not** retrieve graph content
again. Existing legacy indexing is retained for other legacy-index consumers;
no blind legacy-to-V2 backfill is used.

The handoff:

- Checks the supplied proof/manifest and the current effective graph SetId.
- Decodes canonical escaped identities into the raw V2/filesystem paths.
- Includes canonical tracked private files as well as Git-included paths;
  skips gitlinks, matching status's file-content verification set.
- Uses the existing complete canonical filesystem verifier with **cold input**:
  existing rows and raw worktree hashes cannot define canonical clean content.
  Worktree bytes (or symlink targets) must match the verified canonical bytes
  under the same raw/clean filter semantics as status.
- Re-observes metadata and policy after hashing. Unstable, dirty, missing,
  mode/type-mismatched or racy entries are left uncached; prior stale leases
  for those paths and paths no longer present are removed.
- Publishes one atomic V2 batch only after rechecking the registered working
  copy, view state and effective SetId in the write transaction. Parent changes
  are therefore detected even if the child's own Merkle is unchanged.

The ordinary and `--stream` command regressions inspect persisted V2 rows
**before status can backfill them**, including the newly recorded private
`.atomicignore`. First post-import status asserts complete cache hits and zero
unchanged-content reads. CRLF-filtered bytes retain the canonical LF hash, and
the literal-percent path is indexed under its correct physical identity.
Repository tests additionally exercise mutation/replacement during hashing,
policy drift, stale or mismatched proofs, effective parent-state changes,
future mtimes, missing/dirty paths and mode/kind changes.

This fixes the final-import clean-index initialization, not the remaining
namespace/reconciliation/snapshot latency or graph-only chmod-capture issue.
It performs worktree verification once at import finalization rather than
teaching the first status to trust arbitrary worktree bytes.

Validation for the handoff change:

- Final repository library suite: **1,510 passed**, 3 manual drivers ignored.
- Git-command unit suite: **202 passed**, 2 manual drivers ignored.
- Actual release/LTO CLI canaries (`ATOMIC_SERVICE=local`): stream import of
  130 Git files seeded **131** rows including `.atomicignore`; the **first**
  ordinary status logged two scans with `tracked=131, leases=131, hashed=0`
  and completed in **0.220 s**. Ordinary import of a CRLF-filtered literal `%`
  path, attributes and symlink seeded **4** rows; its first status logged
  `tracked=4, leases=4, hashed=0` in both scans and completed in **0.227 s**.
- Both canaries passed all shim-verification layers and retained identical
  tracked bytes/modes/mtimes and HEAD, with clean Git status.

These small fixtures establish a first-status cache-hit regression, **not** a
new latency claim for the larger Atomic or Hyperfine histories. The release
binary is `target/release/atomic`, SHA-256
`ab94ed80d522b11d0e22ed84c211544ad600e7c303ac51158cc49ccd855bf968`.
Evidence is preserved under `import-v2-seed-final-qcZ1l2/` (`result.json` and
per-command logs), with the reproducible `canary.py` under
`import-v2-seed-qHMVHe/`. The earlier canary there preserved a fixture-preparation
failure: manually changing LF to CRLF left Git's index stat baseline stale;
the final fixture renormalizes it and asserts Git-clean state before import.
No user's imported repository or installed binary was modified.

## Large-history namespace and startup follow-up

Intent `ATOM::continuouslee::17` follows the user's report that another fresh
import still took around three seconds for status. Read-only inspection of
that new import found **1,344 complete V2 rows**, all with matching physical
metadata, with no racy, missing or policy-mismatched entries. V2 seeding was not
the remaining failure. The 130-file handoff canary above was not representative
validation of whole-command latency on this larger history.

A new independent APFS replica contains the same 250-commit first-parent Git
tip, 1,343 Git files plus native `.atomicignore`, and 214 directories. It has
its own physical working-copy registration, with the original materialized
baseline/V2 rows preserved for fixture preparation. All ordinary-status and
mutating operations ran only in the replica.

### Remaining causes and equivalent fast paths

- Namespace queries rediscovered exact basename spans even though the visible
  `PATH_CLAIMS` records already contain those spans. The new core name query
  accepts those structural claims, using an exact existing vertex only when
  the match is unique and nonempty. Missing, ambiguous, wrong-inode or absent
  claims fall back to the original block lookup. Visible folder-edge linked /
  unlinked checks remain identical.
- More significantly, matching a historical basename invoked
  `ChangeStore::copy_content_span`, whose cold miss deserializes the **entire
  change including JSON provenance** just to copy a few filename bytes.
  Profiling located that call under namespace projection, despite every V2
  content lease hitting. Reduction now retains the canonical path-to-name
  bindings from `PATH_CLAIMS`, not TREE or working-copy guesses. For a known
  binding, the graph name-liveness check is sufficient; legacy/undescribed
  names retain the original content-byte fallback. This is request-local use
  of existing canonical namespace authority, not a new persistent cache.
- Absent-entry liveness now runs canonical early-exit retrieval through the
  existing `InodeScopedGraph` preload. It does not reconstruct bytes through a
  linear content reader merely to ask whether any live content span exists.
  Missing inode-index data transparently falls back to the graph reader.
- After eliminating payload loads, profiling exposed allocator startup:
  `Pristine::scan_ids` called `next_inode_id`, which iterated every
  `INODE_GRAPH` key on each open. Since its keys start with a **big-endian inode
  prefix**, the last key proves the same maximum. An end-key read now replaces
  that full graph-index sweep. The other high-water sources and exhaustion
  handling remain unchanged, including unbound TREE and orphaned inode rows.

There are no schema changes, skipped snapshot/reconciliation boundaries,
weakened view filters or unvalidated cross-command projection caches.

### Representative release results

The previous release executable was preserved before rebuilding. Unsampled,
untraced alternating runs on the **same** replica in local service mode:

| Command/build | Seconds |
|---|---:|
| Previous release, plain `status` | 3.928 / 3.463 |
| New release, plain `status` | 0.612 / 0.612 |
| Additional new release plain-status runs | 0.610 / 0.615 / 0.613 |
| New release warm short-status runs | 0.588 / 0.592 / 0.601 / 0.592 |
| First invocation after new release build | 1.948 |
| Separate full shim verification | 13.080, passes |

This is about a **5.7× reduction in warm whole-command latency** relative to
the 3.463-second old plain-status observation, measured on the large history,
not extrapolated from a small import. Both release status scans still report
**1,344 leases and zero content reads**. The tracked Git bytes, modes, mtimes,
HEAD and Git status remained unchanged. Cold-start latency is explicitly not
claimed subsecond; these are individual local measurements, not a statistical
guarantee. Dirty snapshot capture and the independent graph-only chmod problem
were not redesigned by this fix.

Final checks: **3,697 core tests**, **1,513 repository tests** (3 manual drivers
ignored), and **202 Git-command tests** (2 manual drivers ignored) pass. New
regressions prove that exact claims avoid constructing the whole-change span
index, fallback/deleted/visibility verdicts are retained, known bindings do
not need a change payload, scoped liveness matches canonical delete/restore /
truncate behavior and missing-index fallback, and the end key preserves an
orphaned inode maximum across byte-order boundaries and read/write reopens.

Evidence: `status-namespace-fast-3k90u9/` in approved OpenCode temporary storage:
`read-only-index.txt`, `before.json`, profiling reports, `tests.json` and suite
logs, `final.json`, `ab.json`, per-command logs and `benchmark.py`.
Release SHA-256:
`6187d99896eff1f9619b4ce3d064c55222b93792ec90bbc7fca0ff36ba7e37f9`.
Use the new `target/release/atomic` for comparison; existing compatible imports
do **not** need to be re-imported for this reader/startup fix. Timings use
`ATOMIC_SERVICE=local`, so they do not measure an older already-running daemon.

# Streaming Git tree/blob → Atomic graph import

Status: proposed architecture, investigated in `atomic-shim-2`. **The production
streaming importer is not implemented.** A limited publication-stage experiment
is implemented and measured below. `atomic-shim` is historical reference; its
experimental importer must not be copied wholesale into this clean tree.

## Recommendation

Use one import session with one open Atomic database, bounded in-memory Git
object reads, an ordered graph compiler, and one writer publishing many distinct
Git interpretations per transaction. Keep commit identity/history granularity;
batch the database publication, not the meaning of the history.

Here, “compiler” means orchestration of the **existing Git shim recording and
assembly capabilities**, not a new graph manipulation engine. Optimize input
ownership, scheduling and transaction boundaries around working functionality.
Core graph algorithms, edge semantics and native recording behavior are not
implementation targets for this migration optimization.

Do not substitute a single enormous transaction for per-commit transactions.
Keep the database handle alive for the entire import, but checkpoint bounded
transactions so interruption loses at most the current batch. Start with a
single-threaded pipeline to establish correctness; parallelize independent Git
reads only after profiling. redb still has one writer.

```text
pinned Git ref/tip + conversion policy
    ↓ compact history/claim preflight (no retained historical blob payloads)
Git ODB: parent/result tree deltas + changed blobs
    ↓ bounded byte-budgeted queue (optional read workers, ordered output)
existing shim conversion/assembly + bounded import-session state
    ↓ bounded prepared changes, separate hash and metadata for every commit
journal prepare → one staged graph transaction for the batch
    ↓ verify → commit → journal completion/recovery boundary
next batch, same Repository/database handle
    ↓
final graph audit → optional final checkout via existing materializer
    ↓
existing working-copy verification/index/checkpoint finalization once
```

## Hard boundary: no materialization during migration

Until the entire selected import has been published and its final graph state
verified, **never materialize tracked files**: no Git checkout/reset, Atomic
materialization, temporary/synthetic checkout or intermediate view switch that
writes a working tree. This applies to preflight, batches, verification,
fallbacks, retries and import recovery—not just the nominal fast path.

Historical content comes from the Git object database and Atomic's staged
graph/content store. Rendering graph bytes into an in-memory buffer for
verification is permitted; writing those bytes into a working directory is
not. Internal change files, journal/recovery metadata and a bounded internal
validation spool are storage artifacts, not permission to create a checkout.

At the very end, invoke the **existing** materialization path only if the
command actually needs a checkout. In a colocated Git repository whose working
tree already matches the imported tip, preserve it and use existing shim
observation/equivalence checks rather than rewriting identical files. Keep
dirty-worktree and expected-old leases intact. If import fails midway, retain
durable graph batches for retry but do not checkout that partial prefix.

## Reuse boundary: shim orchestration, unchanged core graph behavior

| Capability | Reuse rather than replace |
| --- | --- |
| Git history and trust | Existing deterministic sequencing, mainline/all-parent distinction, boundary detection, verified bindings and Atomic-claim validation in `parallel.rs` |
| Paths, kinds and bytes | Existing conversion policy, canonical path identities, Git tree/blob reading and mode/symlink/gitlink handling |
| Change construction | Existing shim recording and `assemble_change` path, including projected bindings, `with_git_directory_deletions` and `assembly_projected_paths` in `synthesis.rs` |
| Graph application | Existing `write_change_to_graph`, dependency checks and cached graph-write machinery; no new raw edge writer or replacement core algorithm |
| Verification | Existing prospective/staged equivalence and semantic checks, with in-memory rendering—not filesystem round trips |
| Publication and recovery | Existing operation locks, journal preparation, leases, receipts and recovery; batch their orchestration without inventing a parallel journal |
| Final checkout | Existing receipted materializer and shim working-copy/checkpoint finalization, only after complete import success |

Extract a transaction-scoped repository helper only where the current wrapper
unconditionally commits or reacquires locks. Keep its graph mutation body and
invariants unchanged. Do not duplicate working assembly/deletion logic inside
the import session to avoid that extraction. If existing helpers need narrower
transaction-scoped access, make that explicit at the repository boundary;
stop for a separate design decision if core algorithm changes appear necessary.

## What the clean implementation actually does

Source references below name existing functions, not proposed APIs.

| Boundary | Current behavior | Implication |
| --- | --- | --- |
| `parallel.rs::phase2_write` | Receives `&mut Repository` and opens its Git handle outside the commit loop | The central writer already reuses a repository handle; reopening the database per commit is not the principal explanation here |
| `repository/mod.rs::redb_change_store` | Uses `pristine.shared_database()` | Preserve shared ownership; do not introduce a second writable database handle |
| `parallel.rs::validate_branch_prospectively_for_tip` | Parses all selected commits, folds every prospective project, retains `Vec<VerifiedImportCommit>` | Historical payloads accumulate before phase 2 starts |
| `parallel.rs::ProspectiveProjectTree::apply_commit` | Rebuilds a manifest and project, caches projects by tree OID, returns a clone | Batching only the later writer does not make the reader bounded |
| `synthesis.rs::synthesize_git_change_locked` | Assembles, finalizes, computes metadata transitions, prepares a journal operation for each synthesis | Per-commit orchestration survives even with one open database |
| Same function's after-state calculation | Iterates the existing view log to rebuild its Merkle even when `supersede` is empty | An additional history-length-dependent cost; an append-only path can use the verified current Merkle, while real removals still need the existing alignment logic |
| `synthesis.rs::apply_synthesized_change_locked` | Registers, applies, projects, verifies and commits one interpretation | Several journal/metadata transactions surround each graph transaction; count them separately |
| `parallel.rs::write_commit_synthesized` | Calls `repo.index_git_sha` after synthesis | Move SHA publication into the same batch transaction as its interpretation, rather than leaving a separate success window |
| `synthesis.rs::verify_staged_full_tree` | Projects the namespace; retrieves graph bytes for every supplied expected entry; checks attributes and semantic state | Verification work must be measured separately from commit/fsync cost |
| `changes.rs::save_change_bytes_after_capability_declaration` | Writes serialized content-addressed change files | Graph writes are in redb, but this synthesis path does not put all change payload bytes there; a new writer must preserve change-store availability/durability |

The ordinary CLI already constructs touched-path expectations for eligible
commits and a complete expectation at the final commit; merges and internal
shim verification have stricter boundaries. This is not a claim that the clean
default importer always renders every full tree. Even touched verification
still builds visibility/projection state, and preflight retains projects.

`atomic-core/src/apply/graph_batch.rs` already provides `CachedWriteGraphTxn`.
Do not implement the old table-cache design document again as though that
wrapper did not exist. Profile the actual apply path before changing table
lifetimes, and route reads and writes through the same cached handles while
they are open.

## Lessons from the original workspace

Historical evidence: `atomic-shim/docs/git-import-graph-batches.md`, particularly
“Completed hyperfine validation”; memory `01m5x2ezsmxf9vtnqv8e0byjhv`.

- The old opt-in graph-batch path documented **1,097 first-parent SHAs, 39
  merges, 357 batch transactions, 3,277.1 seconds (54.6 minutes)**. It used an
  optimized dev build, not a new release benchmark. This demonstrates why
  amortizing commits alone is insufficient, not the clean tree's throughput.
- That path still preflighted historical projects ahead of the writer and
  performed expensive staged historical verification. It was not an
  end-to-end bounded tree/blob stream.
- Git tree equivalence does not detect leftover empty Atomic directories.
  Retire implicit imported directories when their last descendant disappears;
  do not retire explicit native empty directories. Test retirement/recreation
  and clean status, not only final blob equality.
- Rendered line order is not graph edge identity. Never guess an incoming
  edge from the previous displayed line. Reseed exact anchors or use existing
  canonical whole-file assembly when the fast compiler cannot prove them.
- Mainline merge projection and all-parent resolution are different contracts.
  Keep complete ordered Git parent facts without claiming PR-leg commits
  were individually imported by a first-parent walk.

Old test passes do not validate new code in this workspace.

## Session and payload ownership

Proposed boundaries (names illustrative, not new public API commitments):

- **ImportSession** borrows the one `Repository`, pins source tip/object
  format/policy and target view state, owns compiler state and batch budgets.
- **GitObjectReader** traverses parent/result trees by OID. Skip identical
  subtrees; load changed blob contents directly through libgit2, without
  checkout, temporary working files or per-commit Git subprocesses. Preserve
  existing rename/path/filter policy; an optimization must not silently change
  identity semantics.
- **CompiledCommit** carries graph operations, required semantic operations
  under the selected existing mode, exact foreign origin facts, dependencies,
  and a private verification expectation. It remains one separately hashed
  change. Compilation is ordered because graph anchors can refer to previous
  compiled changes before those changes are durable. Produce it through existing
  shim assembly, supplying staged transaction state or the minimum adapter it
  needs; do not build a second independent graph compiler.
- **BatchPublisher** owns the journal/transaction boundary. It accepts already
  finalized immutable inputs; it does not recursively call a per-commit
  repository method that acquires another lock, opens a writer or commits.

Cache blobs by `(object format, OID)` under an explicit byte budget; an OID
without object format is not a complete identity. Share immutable byte buffers
where practical rather than copying payloads into parsed files, manifests and
expectations. Evict decoded blobs and obsolete tree/anchor state. Do not retain
every historical `ProjectTree` or interpreted ancestry set in memory.

Memory target is **current compiler state + bounded queue/batch/cache**, not
constant memory regardless of repository size. A file/line anchor index grows
with current content. A compact all-history OID ordering may still grow with
commit count; measure it separately. All-parent mode needs a bounded parent
state cache with deterministic reseeding, not one mutable mainline baseline.

A count-only queue is not bounded for large blobs. Charge retained bytes,
compiled operations, expectations and serialization buffers; reserve capacity
before allocation. One oversized commit must explicitly refuse or use an
explicitly configured larger budget, not silently break the advertised limit.
Candidate starting knobs: 128 commits / 32 MiB per batch, with a separately
bounded read queue. These are tuning hypotheses, not measured defaults.

## Transaction and recovery contract

1. Compile a bounded batch against a verified durable parent plus a private
   in-memory overlay. Graph anchors must match the state the writer will see.
2. Under existing ordered operation locks, compare the target's expected-old
   state/head. Revalidate at publication; a pinned Git tip does not freeze the
   Atomic target or working-copy observation.
3. Prepare one immutable `SynthesizeGit` operation describing the batch's
   before/after states and every membership transition. Persist it before
   visible publication using existing immediate durability.
4. Make finalized change-store bytes safely available under the existing
   recovery contract. Audit file sync/atomic publication separately; fewer redb
   commits cannot justify referencing missing payload files after a crash.
5. Open one ordered write transaction. In dependency order, register each
   change, write dependency and closure rows, apply graph/semantic operations
   and lifecycle projection, and advance membership/Merkle and SHA index.
   Later entries read earlier entries in this transaction, not a new committed
   read snapshot. Preserve canonical GRAPH/INODE_GRAPH consistency.
6. Verify each required historical state before advancing past it, and prove
   the final effective batch boundary before commit. Commit once; complete
   metadata receipts using the existing journal protocol.
7. On failure, abort the entire current batch and recover its prepared
   operation. Earlier batches remain durable. Restore compiler/cache state
   from the last successful boundary; never reuse anchors from an aborted
   attempt. Counters and resume markers count committed batches only.

“One write” here means one **graph publication transaction per batch**. Journal
prepare/completion still require durable transactions; individual B-tree
inserts and content-addressed records do not disappear. Do not weaken immediate
durability or recovery to manufacture a benchmark win. Do not call a change-store
helper that opens another redb writer while the batch writer is held.

Current interpreted closures copy parent membership into every commit's row
(`apply_synthesized_change_locked`). That can itself grow quadratically with
linear history. First instrument bytes/rows and eliminate repeated in-memory
closure copies where possible; a persistent compact closure representation is
a separate compatibility/schema decision, not a hidden part of batching.

## Verification is a separate optimization, with a proof obligation

First implement streaming/batched publication **without weakening existing
proofs**. Then measure whether verification still dominates. The old experiment
is strong reason to expect it might.

Prefer existing shim verification scopes and caches before adding anything new.
The incremental-verifier discussion below is a **deferred research option**,
not part of the initial migration implementation or authorization to change
clean core graph traversal/manipulation. It requires a separate scoped decision
and evidence that reuse-level optimizations are insufficient.

A subsequent incremental verifier can maintain a verified path/mode/kind/blob
Merkle structure. Derive its updates from **actual staged graph effects**, not
only the compiler's Git delta; otherwise the same compiler bug could corrupt
the graph and its supposed proof together. Retrieve affected graph bytes,
check removed paths and directory lifecycle, update ancestors, and compare the
result with Git's policy-filtered tree identity. Keep semantic validity under
the existing mode as an independent requirement.

Reusing untouched proofs requires proving that staged operations cannot affect
those paths, including cross-inode edges, name conflicts, attributes and
visibility/dependency changes. Until that affected-set proof exists, use the
existing complete verifier at that boundary. A final full check alone cannot
prove all historical commits: two bad intermediate states can cancel. A fast
in-memory Git manifest matching Git is likewise not proof of the written graph.

Do not drop CRDT correctness to meet a throughput target. Preserve the existing
import mode and native record semantics; graph-only persistence versus eager
semantic construction remains explicit, not a silent new default.

## Preflight compatibility

The current preflight rejects bad claims/capabilities before publication. A
streaming writer must not quietly turn that into a partial-import contract.
Use a compact metadata/claim pass, and either a bounded second validation pass
or a bounded-memory disk spool for payload-dependent validation that must
complete before mutation. Re-read pinned immutable Git objects for publication
when this avoids retaining historical projects. Account for both passes in
end-to-end time. A spool is not a synthetic checkout and needs explicit cleanup
and recovery rules.

If rejecting a late corrupt/missing blob before **any** target mutation cannot
be preserved cheaply, expose that as a separate opt-in resumable-prefix contract
before shipping, not as an undocumented change to normal import. Missing Git
objects, shallow/promisor boundaries, required filters and forged Atomic claims
must still fail closed. Optional worktree indexing/checkpoint refresh runs once
after graph success, with existing observation leases; it is not import input.

## Implementation sequence and promotion gates

1. **Baseline instrumentation/tests:** count physical DB opens, read/write
   transactions by role, journal operations, table opens on the real apply
   path, decoded blob bytes, graph operations and verification bytes/time.
   Time source/preflight, compile, serialize/save, apply, verify, commit/fsync,
   finalization separately. Existing broad `commit_ms` includes verification.
   Add a no-materialization guard before changing orchestration; exercise the
   current shim capabilities and identify transaction-owning wrappers to adapt.
2. **Transaction-owned publisher:** extract the per-change staged apply body
   from `synthesis.rs` without its commit; make a new batch coordinator own
   journaling, SHA/closure publication and one commit. Test synthetic prepared
   changes first; preserve the old entry point as a one-entry adapter where
   appropriate. Verify all commits with current proofs initially.
3. **Bounded reader/compiler:** replace all-history parsed/project payload
   retention in the new opt-in session with ordered Git ODB deltas and an
   overlay or staged transaction access. Reuse existing shim recording,
   directory/path/attribute handling and graph assembly helpers; do not
   transplant the old workspace's mixed importer files or replace core graph
   functionality. Route any fallback through the same no-materialization gate.
4. **Verification reuse/profile:** retain the existing shim verifiers and
   measure their costs after batching/streaming. Any new incremental verifier
   is a separately scoped follow-up, requiring the affected-set proof and
   corruption tests, cross-checked against the complete verifier.
5. **Broaden and promote:** keep all-parent, bindings, rewrites, complex paths,
   native semantics and worktree preservation on their established contracts
   until their explicit session tests pass. Unsupported fast-path cases must
   select the old route before mutation or flush safely and report boundaries;
   never silently skip commits.

Test matrix: batch sizes 1/16/128 yield identical per-commit hashes and rendered
states; failure at entry k publishes none of that batch; retry on the same
handle and after reopen is idempotent; process death at prepare/save/apply/
commit/finalize recovers; another view/working copy is untouched. Include hot
files, independent additions, empty/binary/large blobs, chmod-back, rename
chains, symlinks/gitlinks, percent/escape-lookalike paths, directory retirement
and recreation, merges and no-op incremental repeats. Corrupt an untouched
path, an attribute and an incoming edge to test verifier independence. Check
final clean status as well as exact Git content and history coverage.

Enforce the materialization boundary with an invocation guard/spy, not only a
final file-content comparison (an intermediate rewrite could restore identical
bytes). Assert zero materializer/checkout calls through all import stages,
including failed batches, fallback and recovery; zero calls for a successful
preserved colocated checkout; and a single final checkout phase, when requested,
only after all commits and final graph verification succeed. Snapshot tracked
bytes, file kinds, modes and modification times to catch direct filesystem
writes bypassing the materializer. Reject fallback paths that cannot satisfy
this contract rather than silently using a disk-based import.

Benchmark a pinned local source in isolated disposable targets using the
explicit `atomic-shim-2/target/release/atomic`, never installed PATH atomic.
Compare the same executable's old/new routes, same options, same history
scope and same verification guarantees. Run repeated cold/warm cases and
report median/spread, commits/s, changed MiB/s, peak RSS, transaction counts
and stage timings. Use 1k/10k synthetic histories plus a pinned real repository;
do not run a long real import until short runs show the expected scaling.

Structural success: one session DB handle; graph commits scale with batches,
not commits; no retained historical project payloads; no hidden per-commit
repository publication calls. Runtime success must then be measured. No speedup
number or “bounded memory” claim is established by this document alone.

## Small publication-stage experiment

Implemented under intent `ATOM::continuouslee::7`, separately from the design
investigation. No `atomic-core` files or algorithms changed. The only production
refactor separates `apply_synthesized_change_in_txn` from its existing one-change
commit wrapper in `synthesis.rs`; graph application, lifecycle projection and
staged verification retain their existing implementations. The new coordinator
is **test-only**, in `synthesis/streaming_sample.rs`, not a supported batch API.

The sample creates 16 real, deterministic Git commits through libgit2 in a
**bare Git ODB**, each adding a text file under a shared directory. It reads
their blobs directly, records through the existing in-memory recording workflow,
and compiles through `Repository::synthesize_git_change`. It then replays the
same finalized changes into fresh disposable Atomic repositories at batch sizes
1, 4 and 16, rotating run order over three rounds.

Each replay uses one initialized repository handle, the existing ordered locks,
one prepared `SynthesizeGit` operation per batch, the extracted production staged
apply, **full graph/attribute/semantic verification for every commit**, and the
existing metadata finalization/receipts. SHA-index and interpreted-closure writes
share the graph transaction. Graph application validation is not skipped.

Measured on Darwin arm64, `rustc 1.99.1`, Cargo's **unoptimized test profile**:

| Batch size | Graph transactions / synthesis operations | Three replay times (ms) | Median (ms) | Relative to size 1 |
| --- | --- | --- | --- | --- |
| 1 | 16 / 16 | 1412.714, 1352.370, 1481.773 | 1412.714 | 1.00× |
| 4 | 4 / 4 | 692.580, 701.093, 722.354 | 701.093 | 2.02× faster |
| 16 | 1 / 1 | 514.722, 537.076, 533.970 | 533.970 | 2.65× faster |

These are the final repeat's results with the explicit Merkle and full closure
membership assertions included. The initial run also passed, with medians
1435.122 / 719.057 / 521.103 ms respectively (2.00× / 2.75×).

Preparation of the 16 Git commits and their source synthesis took 1578.124 ms
and is **excluded** from replay times. Target initialization, post-replay audits,
reopen and final checkout are also outside the measured interval; change-file
saves, graph application, full staged verification and journal preparation/
completion are inside. Journal transactions still exist in addition to the
listed graph transactions. Runs are short local warm-process observations, not
cold-cache or release measurements.

Correctness checks cover exact ordered change hashes/Merkle, SHA mappings,
interpreted closure membership, exact final graph bytes, verified operation
receipts, reopen and final materialization. Sample paths are asserted absent
after each staged entry and before final materialization, and the working copy's
materialized-state record remains unchanged during import. The only explicit
materialization call is after complete replay and graph checks. This sample
does **not** yet install the planned global materialization-invocation spy.

A separate four-commit regression commits an initial batch, deliberately corrupts
the second batch's staged expectation, verifies that neither SHA/membership of
that batch landed while the first batch survived, retries successfully on the
same handle, reopens and only then materializes. Content-addressed payload files
can remain after abort, as under the existing recovery contract.

Commands:

```sh
cargo test --locked -p atomic-repository --lib streaming_sample -- --nocapture
cargo test --locked -p atomic-repository --lib \
  compare_sixteen_git_commits_in_journaled_batches -- --ignored --nocapture
cargo test --locked -p atomic-repository --lib \
  repository::tests::synthesis_tests -- --nocapture
# Run both sample tests serially, including the normally ignored timing test:
cargo test --locked -p atomic-repository --lib streaming_sample \
  -- --include-ignored --test-threads=1 --nocapture
```

Observed: the rollback/retry regression passed, the explicitly selected timing
experiment passed, and all **24 existing synthesis tests passed** after the
transaction-ownership extraction, both normally and with the
`adoption-test-injection` feature enabled. Scoped `rustfmt --check` and
`cargo check --locked -p atomic-cli` also passed.
The timing experiment is ignored by default;
normal test success alone does not mean it was measured.

Conclusion: the sample supports **amortizing publication/journal overhead with
unchanged graph logic and per-commit proofs**. It does not measure end-to-end CLI
import, bounded-memory streaming, hot-file edits, deletion/rename batches,
merges, bindings, process-death recovery or broad workload performance. The sample
intentionally retains a small prepared payload set. The next sample should add
hot-file edits and directory deletion/recreation through existing shim assembly
before implementing the full streaming reader or promoting a production batch
route. These numbers must not be compared directly to the historical hyperfine
import or described as a universal import speedup.

## Expanded mixed-history sample (16 → 32 commits)

Intent `ATOM::continuouslee::8` extends only the test fixture/coordinator and
this report; no further production or core graph changes. The 32-commit fixture
repeats the same 16-step lifecycle against accumulated history. Its first 16
change hashes must exactly match the separate 16-commit fixture.

Each lifecycle covers:

- multi-file addition/update and a localized hot-file edit;
- nested empty-file creation and binary addition/replacement/deletion;
- executable chmod and chmod-back, including on an empty file;
- rename, edit-after-rename, truncation, and rename-with-content-change;
- deletion of the last nested descendant, directory retirement, recreation,
  then retirement again;
- an empty Git commit retained as a separate interpretation.

Edits, moves and deletions use `record_foreign_modified_file`,
`record_foreign_moved_file` and `record_foreign_deleted_file`; normal synthesis
adds directory lifecycle operations. The fixture asserts actual `SetAttr`,
`FileMove`, `DirDel` and empty-change shapes rather than merely labelling
commits. These are fixture-directed canonical moves, not a test of Git's rename
similarity detection or its provenance policy. Mixed fixture expectations are
checked against real bare-Git tree/blob OIDs and bytes before source synthesis.

The coordinator now forwards each prepared change's deletion leases. All
replays retain full per-commit graph/attribute/semantic verification and add an
independent complete namespace assertion **including implicit directories** at
every staged entry, so leftover empty directories cannot hide behind Git tree
equality. Final audits also check mode/kind and clean status after the single
final checkout. The additional namespace guard runs inside the measured interval
for all batch sizes. Post-replay audits and final materialization remain outside.

### Measurements

Final serial run, same Darwin arm64/unoptimized Cargo test profile as the small
experiment; three rotated trials per row, with identical prepared payloads
across batch sizes. Times include change-file saves, staging, full per-commit
verification, namespace guards and journal preparation/completion. They exclude
fixture/source synthesis, target initialization, final audits/reopen/checkout.

| Mixed commits | Batch size | Graph transactions | Median (ms) | Min–max (ms) | Speedup vs same-size batch 1 |
| --- | --- | --- | --- | --- | --- |
| 16 | 1 | 16 | 1146.224 | 1104.553–1156.377 | 1.00× |
| 16 | 4 | 4 | 435.259 | 428.615–443.744 | 2.63× |
| 16 | 16 | 1 | 263.059 | 260.615–273.782 | 4.36× |
| 32 | 1 | 32 | 2377.961 | 2346.577–2398.881 | 1.00× |
| 32 | 4 | 8 | 1013.379 | 1008.814–1039.761 | 2.35× |
| 32 | 16 | 2 | 658.878 | 657.592–662.491 | 3.61× |
| 32 | 32 | 1 | 589.119 | 586.736–593.498 | 4.04× |

Source preparation took 1270.076 ms for 16 mixed commits and 2711.574 ms for
32, outside these timings. Synthesis-operation counts equal the listed graph
transaction counts; additional journal durability transactions still exist.
The retained 16-add fixture also passed again, with median publication times
1392.648 / 695.169 / 513.599 ms at sizes 1 / 4 / 16 respectively.

**Batching remains faster on the larger mixed workload, but doubling history
does not improve throughput.** At fixed batch sizes 1 / 4 / 16, total time grew
2.07× / 2.33× / 2.50× respectively. Size-16 mixed throughput declined from
60.82 to 48.57 commits/s; raising the larger fixture's batch size to 32 recovered
54.32 commits/s. On that 32-commit history, size 32 is only about 12% faster than
size 16, demonstrating diminishing returns. Remaining history-dependent apply/
verification work is a profiling target, not a reason to weaken existing checks.
The second cycle edits existing hot/stable identities where the first cycle
created them, so these growth ratios describe this lifecycle, not a controlled
complexity measurement with identical operation counts per half.

The full serial sample run passed **all four sample tests**, explicitly including
the two normally ignored timing tests: 21 mixed replays, 9 add-only replays, and
the two negative-test scenarios. These are local warm-process microbenchmarks,
not randomized release benchmarks or a general complexity claim.
After the final materialized-state assertion was added, the normal synthesis
selection passed **26 tests with 2 timing tests ignored**, both with default
features and with `adoption-test-injection`. Scoped `rustfmt --check` passed.
The earlier explicit serial command above is the evidence that the ignored
timing cases were actually executed, not the normal selection's pass count.

### Recovery finding: prepared semantic identities are not portable

The expanded negative test initially failed when it blindly retried the same
cross-repository prepared payloads after an aborted batch. Investigation confirmed
the distinction: aborted registrations advance the handle's live NodeId allocator,
while these precompiled edit FileOps retain source-repository semantic IDs.
In the diagnostic run, source commit 16 expected `NodeId(17)` but the retry
registered it as `NodeId(25)`; semantic verification correctly refused an alive
line rendering of 556 bytes against the expected 276 bytes. Fresh replays at
every tested batch size passed; **stale same-handle mixed-payload retry did not**.

The regression now deliberately checks that refusal, preserving the durable
four-commit prefix and publishing no SHA from either rejected batch. Reopening
this disposable target reconstructs its allocator from that prefix; the sample
then replays successfully and explicitly checks every source/target NodeId
mapping before the final checkout. This is a narrow test-fixture recovery, **not
a general production resume strategy or permission to rewrite core IDs**.

A production streaming session must invalidate and **reassemble** prepared
semantic payloads against the actual current repository identities after an
abort, using existing shim recording/assembly. Blindly replaying these fixture
objects is insufficient. No verifier was disabled, no semantic layer was
discarded and no core allocation/graph behavior was changed to get a pass.

Run the mixed timing experiment serially:

```sh
cargo test --locked -p atomic-repository --lib \
  compare_mixed_sixteen_and_thirty_two_git_commits \
  -- --ignored --test-threads=1 --nocapture
# Both negative tests plus both timing fixtures, including the original adds:
cargo test --locked -p atomic-repository --lib streaming_sample \
  -- --include-ignored --test-threads=1 --nocapture
```

As before, this measures prepared publication, not a production streaming
importer's end-to-end runtime or memory bound. Merge DAGs, bindings, symlinks,
gitlinks, nontrivial path identities and process-death recovery are not added
to the fixture by this expansion.

## Pinned Hyperfine, batch size 100: real-data canary

Intent `ATOM::continuouslee::9` tests the next hypothesis using the local Git
objects of Hyperfine at `c5161524980522320c7d3425e714cc0965a869a5`. The selected
first-parent history contains **1,097 commits**, so batch size 100 means **11
graph publication batches**, not ten. No old imported Atomic state is reused.

An under-ten-second import is a hypothesis, not a consequence of selecting a
batch size. Batching amortizes durable publication overhead; Git reads,
recording/assembly, per-commit graph application, historical verification and
the final checkout still cost time. The earlier measurements excluded source
compilation and did not establish a fixed per-batch cost for arbitrary commits.

The opt-in source driver in
`atomic-cli/src/commands/git/real_import_sample.rs` uses the **existing clean
shim**: `validate_branch_prospectively_for_tip`, `write_commit`, foreign metadata,
recording and synthesis. It reads the pinned source ODB and imports into a new
disposable Atomic directory on its own view, without a checkout or materialized
source files. It spools the shim's complete prospective expectations as JSONL.
This preparation deliberately still includes normal per-commit publication;
its time is reported separately and must not be called pure compilation time.

The repository's existing test-only coordinator then reads those freshly
prepared change files and expectations and replays them at size 100 into
another fresh directory. It retains full per-commit graph/attribute/semantic
verification, directory namespace checks, ordered locks, journaling and SHA/
closure publication. Target root entries must remain limited to `.atomic`
throughout publication. Only a successful complete selected history may reach
the final materialization/audit. The sample's regular-file-only final audit and
sequential semantic-ID assumptions are checked explicitly, not silently waived.

Experiments have explicit soft budgets checked between commits/batches; a
single operation may exceed a budget before returning. Preparation writes a
completion/error summary and preserves its JSONL and graph prefix on a budget
stop. Opt-in resume uses the existing shim's incremental SHA handling only after
checking that every persisted spool row matches that exact durable prefix.
Attempt summaries are retained. Incomplete preparation cannot be replayed as a
silently successful full source. A 512-MiB expectation-spool cap and 256-MiB
prepared-payload cap prevent this test from becoming an unbounded experiment;
they do not make the production preflight a streaming reader.

The first **100-commit canary** selects tip
`ab4dbe998e6d748e7629239461dd818fa9ce0232` (22 regular/executable files at that
tip). Its first preparation attempt reached **72/100 commits in 122.053 s**
before the 120-second soft budget stopped it; preflight was **245.609 ms**.
This was a budget failure, not a passing import or a measurement of 100-commit
batch publication. It establishes that real source preparation is materially
different from the tiny synthetic fixture, but does not by itself predict the
runtime of a future integrated streaming/batch implementation.

Preparation subsequently completed the selected 100 commits using the existing
incremental path in a second attempt: **82.514 s additional work, 204.566 s summed
preparation time**, **780.384 ms summed preflight**, and a **20,049,862-byte JSONL
spool**. The source stayed unmaterialized. The selected prefix has no merges;
this is not coverage of Hyperfine's later 39 mainline merges. Both the initial
budget failure and successful continuation are retained as separate attempt
summaries, not averaged or relabelled as a single passing first attempt.

These runs use the unoptimized test profile and full historical graph/semantic
verification. They are stricter than the ordinary CLI's eligible touched-path
verification scopes, so comparisons to default CLI or release performance must
state that difference. This experiment does not silently change proof scope to
reach the proposed timing target.

### Completed 100-commit replay

The canary successfully replayed **all 100 selected commits in one graph
transaction and one `SynthesizeGit` operation**, then passed exact hash/Merkle/
SHA/closure, complete namespace, graph bytes/modes/kinds, verified journal,
reopen, final checkout and clean-status checks. No working files appeared during
source preparation or batch publication. Retained prepared payload was
**8,736,915 bytes**, excluding Rust/container/cache overhead.

One local unoptimized trial (not a median or release benchmark):

| Phase | Time |
| --- | ---: |
| Prepared payload load | 1.139 s |
| Fresh target setup | 0.075 s |
| **Batch publication, 100 commits / 1 transaction** | **140.526 s** |
| Audit + reopen | 3.530 s |
| Final checkout/status helper, including its graph pre-check | 6.075 s |

The successful batch's diagnostic phase counters:

| Inside publication | Time |
| --- | ---: |
| Journal preparation / before-state setup | 0.041 s |
| Change-file saves | 0.015 s |
| Graph/semantic apply | 19.927 s |
| Tree projection + staged verification | **119.123 s (~85%)** |
| Extra test-only namespace guards | 1.009 s |
| Graph transaction disk commit | **0.122 s** |
| Journal completion | 0.023 s |

Counters use existing rounded per-change timings and do not sum exactly to the
full wall time. The 120-second replay guard is checked **between batches**;
this single batch exceeded it before returning. This is not a hard 120-second
timeout or evidence that large batches have bounded writer hold times.

**Result: 100-commit batching alone does not validate an under-ten-second
1,097-commit import.** In this strict debug canary, computation/projection/
verification—not the final disk commit—dominates. Source preparation includes
an additional ordinary publication pass, so adding its time to replay measures
the duplicate-work experiment, not a future integrated importer. No fair
per-commit-vs-batch speedup ratio for Hyperfine is claimed from those two
different workloads.

The full 1,097-commit run was **not attempted** after this gate. Nor was an
optimized/default-CLI verification-policy comparison run. Those are separate
measurements needed before judging the proposed production timing target; the
next useful experiment is an optimized 100-commit canary that explicitly reuses
the shim's existing verification scopes, not a core graph rewrite or an
unbounded full-history run.

Post-experiment regression checks passed **26 repository synthesis tests**
(3 opt-in timing/real-data tests ignored in that normal selection), **49 CLI
parallel-import unit tests**, and scoped `rustfmt --check`. The real source
driver and replay were separately invoked with `--ignored`; the source's
initial budget failure is preserved rather than counted as a pass.

Retained canary evidence:
`/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/hyperfine-100-canary-nXnIdP/`.

Reproduction (use a fresh artifact directory and an explicit local source):

```sh
export ATOMIC_REAL_SAMPLE_GIT=/path/to/local/hyperfine
export ATOMIC_REAL_SAMPLE_ARTIFACT=/path/to/fresh/disposable/artifact
export ATOMIC_REAL_SAMPLE_COMMITS=100
export ATOMIC_REAL_SAMPLE_BUDGET_SECONDS=120
cargo test --locked -p atomic-cli --bin atomic \
  prepare_hyperfine_publication_sample -- --ignored --test-threads=1 --nocapture
# Only after a clean budget stop, to resume this same disposable preparation:
ATOMIC_REAL_SAMPLE_RESUME=1 cargo test --locked -p atomic-cli --bin atomic \
  prepare_hyperfine_publication_sample -- --ignored --test-threads=1 --nocapture
# Only after preparation.json reports complete:
ATOMIC_REAL_SAMPLE_BATCH=100 \
ATOMIC_REAL_SAMPLE_TIMINGS="$ATOMIC_REAL_SAMPLE_ARTIFACT/phases.jsonl" \
cargo test --locked -p atomic-repository --lib \
  replay_hyperfine_in_hundred_commit_batches -- --ignored --test-threads=1 --nocapture
```

### Measured final-tip result

One final-tip trial on the **same prepared 100 Hyperfine commits**, same
unoptimized test profile, one graph transaction and normal journal durability:

| Publication policy | Publication wall time |
| --- | ---: |
| Verify every historical commit (prior strict baseline) | 140.526 s |
| Verify only the final tip before commit | **24.969 s** |

That is an observed **5.6× reduction**, not a statistical or release benchmark.
Source preparation was reused and excluded from both replay timings. The
final-tip phase counters were:

| Final-tip publication component | Time |
| --- | ---: |
| Graph/semantic apply across 100 commits | **19.838 s** |
| Per-change tree bookkeeping/projection and structural verification | 1.783 s |
| One complete final graph/semantic equivalence check | **2.873 s** |
| One extra namespace guard | 0.030 s |
| Disk commit | 0.121 s |

Apply time is essentially unchanged from the strict baseline's 19.927 s. The
experiment therefore isolates a large repeated-verification cost; it does not
show that raw graph application is now cheap enough for a ten-second full import.
The source's complete final expectation and all post-publication hash/Merkle/
SHA/closure, graph bytes/modes/kinds, namespace, verified operation and reopen
checks passed. Working files stayed absent until the final checkout. Separate
audit/reopen and final checkout/status-helper phases took 3.489 s and 6.048 s;
these are not included in the 24.969 s publication figure.

Evidence beside the strict baseline:
`hyperfine-100-canary-nXnIdP/phases-final-tip.jsonl` and
`hyperfine-100-canary-nXnIdP/replay-final-tip-batch-100.json`. The reports
explicitly declare final-tip proof and no independent historical-state proof.
The full 1,097-commit/multi-batch final-only route and optimized-build timing
remain unimplemented/unmeasured.

The replay emits per-batch save/apply/projection-verification, namespace-guard,
disk-commit and journal timings using existing write timings (some rounded to
milliseconds). Its final report separates load/setup/publication/audit/checkout.
Source preparation plus replay is an experiment workflow with duplicated graph
publication, not the runtime of a finished integrated importer. No under-ten-
second, full-1,097-commit or release-build result is implied by a prefix canary.

## Final-tip equivalence instead of per-commit reconstruction

Intent `ATOM::continuouslee::10` tests the user's proposal to replace repeated
historical-state verification with a final no-op comparison against Git HEAD.
The comparison must read the **actual final Atomic graph** and compare complete
policy-filtered paths, bytes, modes/kinds and required semantic state against
the pinned Git tree. It is not an index/stat-only clean-status check, a new
corrective record, or an intermediate checkout. If an existing Git checkout
already represents HEAD, a production integration need not rewrite those files
to perform this comparison.

This changes the guarantee: **the final imported state is verified; individual
historical Git states are not independently certified by that final check**.
Cheap dependency, causal-frontier, canonical-deletion and graph-application
validation remain enabled. A middle-state mismatch can be invisible at the tip
if later changes cancel it; final success must not be labelled full historical
verification.

The test-only coordinator adds explicit `each-commit` and `final-tip` policies.
Existing tests keep `each-commit`. The opt-in final policy calls the same staged
apply helper with empty content expectations for intermediate entries, retaining
its structural validation and normal tree/semantic writes. It then calls the
existing complete `verify_staged_projection` and namespace check **once before
the graph transaction commits**, against the final Git-derived expectation.
Tree bookkeeping/projection still needed by subsequent changes is not removed.
No production verification default or core graph algorithm changes.

Two boundary regressions demonstrate the contract:

- A deliberately wrong intermediate expectation is rejected by per-commit
  verification but is outside the final-only policy's guarantee. A wrong final
  expectation aborts the whole transaction, including SHA/view publication.
- Final-only publication refuses a prior imported prefix or a nonempty target
  view. The real runner also refuses a selection larger than its batch size,
  so this experiment cannot publish unchecked intermediate batches into dev.

For the 100-commit canary, all changes fit in one transaction and a failed final
proof can roll them all back. For a future 1,097-commit import with 100-commit
batches, **private staging visibility plus final verified promotion** (or an
equivalent existing operation boundary) needs a separate design. Ordinary dev
must not be presented as a verified imported state while unchecked batch
prefixes are being published. Reuse view filters, journal/lease APIs and the
existing graph engine rather than changing core mutation semantics.

The final-tip run reuses the exact completed source corpus; it does not repeat
the expensive ordinary source-preparation pass. A separate target and report
stem preserve the strict baseline. Reports explicitly set
`full_per_commit_verification=false` and `historical_states_verified=false`.

```sh
ATOMIC_REAL_SAMPLE_ARTIFACT=/path/to/completed/hyperfine-100-canary \
ATOMIC_REAL_SAMPLE_BATCH=100 \
ATOMIC_REAL_SAMPLE_VERIFICATION=final-tip \
ATOMIC_REAL_SAMPLE_TIMINGS=/path/to/completed/hyperfine-100-canary/phases-final-tip.jsonl \
cargo test --locked -p atomic-repository --lib \
  replay_hyperfine_in_hundred_commit_batches -- --ignored --test-threads=1 --nocapture
```

The two policy-boundary regressions and all existing synthesis regressions pass:
**28 passed, 3 opt-in tests ignored** in the normal selection, with the same
result under `adoption-test-injection`. The real final-tip canary was separately
executed with `--ignored`. Scoped `rustfmt --check` passed.

## Full-history private staging experiment

Intent `ATOM::continuouslee::11` extends the test-only harness to the entire
pinned **1,097-commit first-parent history**, with **39 mainline merges**, without
changing production graph algorithms or import defaults. This is still not a
production streaming CLI and not an all-parent/PR-leg history import.

The full experiment uses `CARGO_PROFILE_TEST_OPT_LEVEL=2` and
`CARGO_PROFILE_TEST_DEBUG=1`, recorded as an **optimized test build, not a
release/LTO build**. Build time is separate from import-stage timings. Earlier
100-commit canaries used unoptimized test builds; do not attribute all timing
differences solely to batching or verification policy.

### Isolation and final promotion

- Private graph batches target a new Draft view `git-import-private`, parented
  to the disposable target's empty `dev`. They apply through the existing
  repository graph/semantic engine with dependency checks and ordered operation
  locks. Existing preserve-working-copy/deferred projection support keeps
  current-view bindings untouched.
- Every 100-commit batch journals its metadata transition and retains recovery
  state. The batch's receipt proves its private mutation completed; it is not
  a claim that its historical content matches Git. After each batch, the test
  checks that `dev` still has zero members/state, no visible files and its
  original working-copy desired state. No working files may appear.
- After the complete exact source hash sequence is staged, a promotion
  transaction references those existing graph changes into `dev`, using the
  normal membership primitives; it does not apply/copy graph edges again.
  Existing tree realignment and the complete final graph/semantic equivalence
  and namespace checks run against **the actual staged dev view before this
  transaction commits**. Ordered locks and expected-old leases protect the
  target boundary.
- A failed final check aborts dev membership publication and retains the
  private stage for inspection. A regression exercises this failure, proves
  dev remains empty, then retries final promotion successfully. Only successful
  promotion may proceed to reopen/audit and the one final checkout/status phase.

The tests do not create or switch project/session views. These application-level
views exist only inside new disposable experiment repositories. The same private
policy is rejected if directed at the ordinary current view or a nonempty dev
parent, rather than exposing an unchecked prefix.

### Avoiding a repeated hour-long preparation pass

The private source-preparation driver can opt into
`ATOMIC_REAL_SAMPLE_SOURCE_VERIFICATION=defer-final`. A scoped, thread-local,
`cfg(test)`-only seam supplies empty intermediate content expectations to the
existing shim writer; ordinary/shipping imports remain unchanged. The existing
Git prospective-tree checks and graph/dependency validation still run. Native
semantic operations are still generated and applied. Scope restoration is
tested so the override cannot leak into later ordinary checks.

Deferred source artifacts explicitly declare their pending final content proof
and cannot be used as evidence of per-commit verification. Their JSONL contains
full expected bytes only for the final tip; intermediate graph changes, origin
facts, hashes and deletion leases remain intact. This avoids retaining a full
verification snapshot per commit in the spool, but **does not fix the existing
all-history prospective preflight memory behavior**.

The experiment still performs two graph-publication passes: existing shim
preparation (per-commit durable writes in its private scratch repository), then
100-commit batches in the separate target. Both times must be reported; their
sum is a two-pass experiment workflow, not the runtime of an integrated
single-pass streaming importer. The old 3,277.1-second historical run also used
a different proof/semantic mode (zero imported FileOps); it is context, not a
controlled like-for-like speedup baseline.

### Full-run configuration and evidence

Fresh artifact directory:
`/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/hyperfine-full-final-tip-opt-IkAIxc/`.

The driver records platform/compiler/build flags, source HEAD/ref/index/Atomic
database metadata before and after, command logs, per-stage wall times, prepared
source metadata, private publication phases and final replay report in
`run.json`, `prepare.log`, `replay.log`, `preparation.json`,
`phases-private-stage.jsonl` and `replay-private-stage-batch-100.json`.
It uses 1,200-second soft source/replay budgets and a separate 1,320-second
process timeout per phase; timeout or failure retains all artifacts and is not
reported as a completed import.

```sh
export CARGO_PROFILE_TEST_OPT_LEVEL=2 CARGO_PROFILE_TEST_DEBUG=1
export ATOMIC_REAL_SAMPLE_BUILD_LABEL='Cargo test opt-level=2 debug=1; not release/LTO'
export ATOMIC_REAL_SAMPLE_GIT=/path/to/read-only/local/hyperfine
export ATOMIC_REAL_SAMPLE_ARTIFACT=/path/to/fresh/disposable/full-artifact
export ATOMIC_REAL_SAMPLE_COMMITS=1097 ATOMIC_REAL_SAMPLE_BATCH=100
export ATOMIC_REAL_SAMPLE_BUDGET_SECONDS=1200 ATOMIC_REAL_SAMPLE_REPLAY_BUDGET_SECONDS=1200
ATOMIC_REAL_SAMPLE_SOURCE_VERIFICATION=defer-final \
cargo test --locked -p atomic-cli --bin atomic prepare_hyperfine_publication_sample \
  -- --ignored --test-threads=1 --nocapture
ATOMIC_REAL_SAMPLE_VERIFICATION=private-stage \
ATOMIC_REAL_SAMPLE_TIMINGS="$ATOMIC_REAL_SAMPLE_ARTIFACT/phases-private-stage.jsonl" \
cargo test --locked -p atomic-repository --lib replay_hyperfine_in_hundred_commit_batches \
  -- --ignored --test-threads=1 --nocapture
```

### Preparation scaling diagnosis during the full run

The optimized, deferred-content-proof preparation still slowed substantially:
100 commits took 19.107 s, 200 took 68.714 s, 300 took 189.508 s and 390 took
450.191 s. These are progress checkpoints from the same full attempt, not
independent prefix benchmarks. The private replay stage had not started at
those checkpoints.

A single resource snapshot at 7:18 process elapsed reported **1,993,168 KiB
RSS**. A three-second macOS `sample` at about nine minutes reported a 1.7-GiB
physical footprint and 1.9-GiB peak-so-far. Neither is a full-run peak/RSS bound
or proof that all memory belongs to the preflight cache.

The sampled worker's 1,593 stack observations were in:

```text
ParallelImporter::write_commit_synthesized
  → Repository::record_foreign_modified_file_on_view_excluding
    → alive_content_vertex_count
      → collect_sorted_content_vertices
        → retrieve_graph → replacement_roots → redb adjacency iteration
```

This is an existing recording/baseline-divergence graph read, **not the deferred
per-commit content verifier and not a filesystem checkout**. It is a concrete
additional scaling hotspot; the short profile is not a percentage attribution
for the whole preparation run. The source files are preserved in
`source-profile.txt` and `resource-snapshot.json` beside the run logs. No core
traversal or recording algorithm was modified to conceal this cost.

### Full-run outcome: budget failure during preparation

The full attempt **did not complete**. The source driver stopped cleanly at
**487 / 1,097 commits after 1,210.167 s (20 minutes 10 seconds)** when its
1,200-second soft budget was checked after the active commit returned. The
outer command took 1,211.941 s and exited 101; `run.json` records `success=false`.
Preflight itself took only **886.264 ms**. The compact JSONL spool was 101,465
bytes; no intermediate working files were materialized.

**Private batched replay, final dev promotion/proof, final checkout and the
runner's scheduled post-import regression stages were not reached.** There is
no full-history publication timing or successful 1,097-commit equivalence result
to compare with the old nearly-hour import. The private-stage mechanism passed
its separate 16-commit lifecycle/isolation/final-rejection regression; that is
not a substitute for full-history coverage.

The source's HEAD, refs, Git index size/mtime and existing Atomic database
size/mtime matched their before-run snapshots (`source_unchanged=true`). The
failed preparation repository, spool, logs, summaries and profile remain intact.
The target replay repository was never created; no target dev promotion occurred.

Late log examples put the bottleneck in perspective: the last dependency bump
took **24.376 s total**, but reported **0 ms assembly**, **0 ms save**, **305 ms
apply** and **114 ms projection/commit**. Most time is outside those write
phases, consistent with the sampled preparation graph scan. This does not
establish that every late commit spends exactly that amount in the sampled
function, but it rules out attributing the total delay to its final disk commit.

The current two-pass harness still invokes the ordinary graph-backed recorder
to prepare changes. Therefore, despite final-only proof, it is **not yet the
intended in-memory Git-parent/delta → graph-write streaming importer**. The next
implementation work must address import-side preparation state/anchor reuse
through existing shim capabilities before another expensive full run; merely
resuming this source or increasing batch size does not address this measured
path. Core graph manipulation was deliberately left unchanged.

After the failed run, regression checks were executed separately (not by the
short-circuited full-run driver): optimized repository synthesis selection
**29 passed / 3 opt-in tests ignored**, optimized CLI parallel-import selection
**49 passed**, and scoped `rustfmt --check` passed. The private-stage isolation/
failed-final-proof regression is included in those 29 tests. These passes do
not turn the incomplete real-history attempt into a success.

## Wired direct producer and `--stream` command

Intent `ATOM::continuouslee::12` replaces recorder-backed preparation with an
actual direct path. It is wired into the real command, not only a replay test:

```sh
# In a fresh, checked-out Git repository with no .atomic directory:
/path/to/atomic-shim-2/target/debug/atomic git import --stream --no-vault
```

`Import::import_branch` selects `import_graph_stream_prevalidated` under the
explicit experimental flag. The existing command-boundary preflight, workspace
adoption, vault policy, final indexing/tags/checkpoint behavior are retained.
Its prevalidated historical payloads are dropped before direct compilation.
The default command and native/eager semantic recorder are not rerouted.

The actual data path is:

```text
pinned first-parent OIDs and existing Git capability/claim checks
  → one commit's old/new Git blobs and metadata (existing parser)
  → existing build_graph_first_change + import-owned live line/edge cache
  → finalized Git-classified change/hash (once)
  → at most 100 changes / 128 MiB of compiled private batch payload
  → existing graph apply / journal / SHA / interpretation closure writes
  → complete final Git-derived graph/namespace/mode/byte proof
  → one metadata-only promotion into the current target view
  → existing command finalization; adopt the existing Git checkout unchanged
```

`GitGraphImportSession` in `atomic-repository/src/repository/git_stream.rs`
borrows the one open repository. It does not invoke `record_*`, create a source
repository or read a prepared-change spool. Its new helpers own orchestration,
not core graph algorithms. Failed private writes or final proof retain the
stage; the current view is never populated by unchecked prefixes. Library
regressions cover failed final promotion, recovery/reopen, failed dependency
publication and a graph-only retry despite allocator gaps.

### Compiler/cache corrections and proof scope

- The existing builder now emits canonical structural `FileDel`, not only
  content edits, and the direct adapter retires implicit directories using the
  same exact `DirDel` shape as existing globalization. Retired anchors leave the
  cache before recreation. No native explicit empty directories are affected:
  the initial target must contain no recorded history.
- Cached content lines now retain the **actual incoming edge source position**
  as well as its introducer. After a pure deletion, a surviving successor may
  still be physically anchored to a deleted predecessor; guessing its incoming
  edge from the previous displayed line is incorrect. New successor ownership
  is recorded only when a new insertion actually creates that edge.
- The graph-only producer reuses the existing in-memory prefix/suffix middle
  replacement builder, checks its cached parent bytes against Git's actual
  parent blob, and never reconstructs the parent from the growing database.
  Its previous Atomic interpretation is a conservative dependency because that
  complete parent context includes deletions behind surviving anchors.
- Modes/kinds use normal `SetAttr` operations, with dependency on the prior
  attribute writer, including chmod-back. Git-detected renames are explicitly
  lowered to delete+add with fresh inode identity and loss evidence in
  `git.stream.rename_identity_loss`; Git similarity is not stable inode
  authority. Copies also create independent additions.
- Git metadata reuses `build_git_metadata` and `git_synthesis_metadata` rather
  than a new namespace. The prepared boundary enforces the origin's canonical
  `git.sha`, so checked SHA lookups remain valid after reopen.
- The ordinary parser includes side-parent union paths for all-parent
  resolution. Direct first-parent import now requests its first-parent scope
  explicitly; complete ordered Git parent facts are still retained. It does
  not interpret side-only changes as parentless modifications or index side-leg
  SHAs. A merge regression covers this distinction.

The existing bare-repository Git-diff fallback now supplies the actual
`--git-dir` rather than assuming that its parent directory is a checkout.
Normal rename/path parsing tests remain applicable. Test builds trap any entry
to the ordinary historical writer during direct import; a fallback cannot
silently turn another full experiment into recorder-backed preparation.

The semantic mode is explicit: **existing graph-only Git-import behavior**, with
interpretation derived from graph content and Git metadata, not imported native
CRDT identities or token provenance. This is different from the earlier eager
CRDT preparation/replay samples. `--with-crdt` is refused with `--stream`, not
silently ignored. No `atomic-core` graph manipulation was changed.

### Initial restrictions and memory limits

The route accepts fresh repositories and the checked-out branch's first-parent
history. It shares the ordinary importer's mode/kind validation: regular files,
executables, symlinks and gitlinks are supported. Git tree names are opaque bytes;
the shared parser carries them through the reversible `%XX` String encoding and
the tree writer decodes back to the exact original name bytes. Spaces, Unicode,
non-UTF-8 bytes, control characters, backslashes and literal percent-escape
lookalikes do not require a stream-specific filename restriction. Symlink target
bytes are retained as blobs; gitlinks retain their foreign commit ID without
requiring that commit's object in the parent repository.

The route still explicitly refuses eager CRDT, incremental/all-parent,
reactive/detached, existing Atomic state, shallow/promisor/replacement/graft and
Atomic provenance/binding claims. Unsupported modes and inputs do not trigger an
ordinary-recorder fallback; the normal import path remains available explicitly.

The direct API reads one parsed commit at a time. Git ODB headers bound the
old/new input blobs before decoding, and count/serialized-byte bounds limit
compiled batches. The CLI defaults to 100 changes and 128 MiB; the byte bound
also applies to an individual commit's combined old/new input blob sizes.
Compiler memory grows with the **current** files/line anchors;
blob/cache/container overhead and pristine/libgit2 caches are not an RSS bound.
The public command still retains its existing whole-history prospective
preflight before releasing that plan, so **the entire CLI has not acquired a
constant-memory preflight guarantee**. There is no recorder/source-publication
pass in either the direct API or the flag's write path.

The direct importer never materializes files itself. The checked-out command
uses the already correct Git working copy without rewriting its tracked files.
The API's optional generic materializer is outside import: a fresh-directory
file→directory→file test exposed existing `ENOTDIR` handling in historical
filesystem tombstones. The supported existing-checkout/no-op typechange test
passes; that separate fresh-materializer case was not hidden by changing core.

### Direct validation so far

- Mixed 32-commit regular/binary/empty/edit/truncate/mode/rename/directory
  retirement/recreation history passes at batch sizes 1, 4, 16 and 100.
- Real `Import { stream: true }` command integration passes with two Git commits,
  zero imported FileOps and unchanged tracked-file mtime; ordinary shared
  finalization/checkpoint code still runs.
- Bare ODB regressions reconstruct exact source tree OIDs for spaces, Unicode
  normalization pairs, non-UTF-8 components, percent lookalikes, control bytes,
  backslashes and a component containing every legal non-NUL/non-slash byte.
  Edits, renames, directory retirement/recreation, symlink kind changes and
  gitlink add/bump/rename/typechange/delete pass at batch sizes 1 and 100, with
  identical resulting state hashes. No host checkout is needed for these tests.
- The real command imports binary files with spaces/Unicode/percent names,
  `CLAUDE.md -> AGENTS.md` and an unpopulated gitlink without rewriting tracked
  bytes or mtimes; status is clean afterward.
- Flag conflicts and shallow-boundary refusal before Atomic/exclude writes
  pass. A bare ODB merge test passes and leaves its side-leg SHA
  unindexed while retaining both Git parents.
- The corrected real 100-commit Hyperfine canary passes in **3.494 s on an
  unoptimized test build**, including direct metadata/Git reads, compilation,
  private publication and final proof. Reopen, checked SHA lookups, zero imported
  FileOps, final optional checkout and clean status pass. Source preparation
  pass: **none**. Evidence: `hyperfine-direct-100-fixed-HY86mN/`.

Two earlier failures remain preserved: the first direct 100 run passed graph
proof but its post-audit found missing `git.sha` metadata; the standard metadata
builder/prepared-boundary correction above fixes it. The first direct full run
staged 700 changes and stopped at commit `bc86938052cf...` (index 782) because
the all-parent parser supplied a side-only `.github/dependabot.yml` modification
with no first-parent blob. It returned an error after **65.367 s**, did not
promote or materialize, and left the source unchanged. Evidence:
`hyperfine-direct-100-tJCe3F/` and `hyperfine-direct-full-ETAS06/`. The explicit
first-parent parser scope and merge regression correct that wiring error;
neither failure is counted as full-history success.

### Completed full-history measurements

The corrected direct API completed all **1,097 first-parent commits** (39
merges) at pinned Hyperfine tip
`c5161524980522320c7d3425e714cc0965a869a5`, in 11 private batches:

| Measurement | Seconds |
|---|---:|
| Direct import, including final proof/promotion | 32.129 |
| Private publication within that total | 22.720 |
| Final proof/promotion within that total | 8.423 |
| Whole test command, including extra audits and fresh checkout/status | 106.948 |

This used Cargo test `opt-level=2`, `debug=1`, not release/LTO. All reopened
SHA lookups, graph-only checks and final materialization/status passed. Source
refs, index metadata and Atomic database metadata were unchanged. Evidence:
`hyperfine-direct-full-opt-ceLn7J/{run.json,direct-result.json,full.log}` under
the approved OpenCode temporary directory.

The actual production CLI (no `cfg(test)`) also completed on a fresh shared
local Git clone checked out on `import-bench` at the pin:

```sh
ATOMIC_SERVICE=local /path/to/target/debug/atomic git import \
  --stream --no-vault --branch import-bench
```

With dev `opt-level=2`, `debug=1` (not release/LTO), the complete command took
**41.851 seconds**. All 75 Git-tracked files retained identical bytes, modes
and mtimes; Git HEAD was unchanged and Git status stayed clean. Separate
forensic status (4.426 s) and `git shim verify` (13.334 s) passed: operation
heads, durable state, manifest, index, worktree, snapshot, change census and
mapped ref all agreed. The census has 1,098 changes: 1,097 Git interpretations
plus the ordinary native `.atomicignore` initialization. Evidence:
`hyperfine-stream-cli-VHLyNi/{cli-result.json,import.log,status.log,verify.log}`.

These establish a real direct route, not an under-ten-second result. Final-tip
proof does not certify every intermediate historical state; all-parent history
and eager CRDT remain outside this experiment. Subsequent hardening adds
binding-transport refusal, deterministic deepest-first/lexical sibling directory
retirement, and refusal to materialize if Git changes branches during import.
The command still retains the prospective-preflight memory limitation above.

Final hardened-binary repeat: **42.053 s** import, **13.793 s** separate shim
verification, again with all 75 tracked files' bytes/modes/mtimes and HEAD
unchanged and Git status clean. Evidence:
`hyperfine-stream-final-6tu66eow/{cli-result.json,import.log,verify.log}`.
All **201 Git-command unit tests** passed (2 manual drivers ignored), including
the sibling-retirement determinism regression; the final local CLI rebuild and
format checks passed. Repository stream and synthesis regressions additionally
passed 2 and 29 tests respectively.

### Release-build repeat

Intent `ATOM::continuouslee::13` measures the same pinned first-parent history
with the actual release binary, built without profile/environment overrides:

```sh
cargo build --release --locked -p atomic-cli --bin atomic
```

The configured release profile uses default `opt-level=3`, `lto=true`,
`codegen-units=1` and `panic="abort"`. Compiler: Rust 1.99.1
(`58cf8f9c3`, Homebrew). The initial tool invocation timed out during compilation;
the resumed build completed successfully in 3m03s (incremental continuation, not
the total clean build time). Build time is excluded from import timing.

The disposable clone and reproducible benchmark harness are preserved under
`hyperfine-stream-release-IJIOeF/` in the approved OpenCode temporary directory.
The harness times the full `target/release/atomic git import --stream --no-vault
--branch import-bench` command separately from `git shim verify`, records the
binary SHA-256 and compares all tracked bytes, modes, mtimes, HEAD and Git status.

**Result: 32.547 seconds for the complete release import**, with 1,097
first-parent commits published in 11 batches. Separate shim verification took
**10.270 seconds** and passed every layer (operations, durable state, manifest,
index, worktree, snapshot, change census and mapped ref). All 75 tracked files
retained identical bytes/modes/mtimes, HEAD was unchanged and Git status clean.
Private staging reached 1,097 commits at 18.61 seconds on its internal timer;
that is not the complete command timing. The recorded census is 1,098 changes,
including native `.atomicignore` initialization.

Compared with the final optimized-dev run of 42.053 seconds, release took
approximately **22.6% less time (1.29× speedup)**. These are individual local
measurements, not a multi-run statistical benchmark. Build/clone time and the
separate verification command are excluded from the 32.547-second import timing.
The same graph-only, first-parent, final-tip proof scope applies; this is not
all-parent/all-branch migration or eager CRDT. Git tag import still warns and
refuses tags without verified Git state bindings; those warnings occurred in
both dev and release runs, so successful history import does not imply all tags
were imported. The under-ten-second target remains unmet.

Evidence: `hyperfine-stream-release-IJIOeF/{build.txt,benchmark.py,cli-result.json,
import.log,verify.log,tracked-before.json,tracked-after.json}`. Release binary
SHA-256: `18e2e6f75cf21b76b7de87d39ff9b1b6eb05e3c51023f67b0c1db5e281ce5423`.

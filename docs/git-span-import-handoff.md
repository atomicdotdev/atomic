# Git span import: working handoff

This is an **unfinished experiment**, not a production-ready import path.
Work is in `/Users/leefaus/Projects/atomic-span-clean` on
`checkpoint/span-native-integration`, based on Git `33a60686`. The working
copy contains substantial uncommitted harness and source work; in particular,
`atomic-cli/src/commands/git/span_compiler.rs` is a **new untracked file**.
Do not discard or overwrite the existing changes. The original
`/Users/leefaus/Projects/atomic` checkout was inspected read-only and remains
on `fix/insert-session-diff` at `c7e6a68a`.

## What was ported

The earlier branch's `atomic-cli/src/commands/git/span_compiler.rs` and its
private streaming approach informed a narrow port. A CLI built with
`--features experimental-span-import` exposes `atomic git import --span` for
a **fresh, first-parent** import. It lowers Git content changes to schema-3
spans while retaining graph namespace/attribute operations, Git facts,
private batches and final-tip verification. Git similarity renames remain
delete/add, not invented inode continuity. This feature is **not enabled by
default** and does not migrate an existing repository.

The old branch's work log reports a previous Hyperfine experiment at 6.30 MB
compacted span redb versus 54.27 MB linewise for 1,097 first-parent commits.
Those are historical experimental measurements, **not** measurements of this
port. The historical run skipped native bootstrap and shim checkpointing, so
the two experiments are not identical.

## Paired storage measurements

One release run using the same feature-enabled binary for both paths imported
1,097 first-parent commits at Hyperfine tip
`c5161524980522320c7d3425e714cc0965a869a5` and checked all 75 selected files
per clone. The binary SHA-256 was
`a28fc216c3c5cffdec3192c152434102f6aae76b5349b9a873df211b330b7c99`;
it predates the latest import and shim preflight changes. After compaction:

| Measure | Span | Linewise |
|---|---:|---:|
| redb logical bytes | 35,524,608 | 80,285,696 |
| entire `.atomic` bytes | 44,270,109 | 90,872,593 |
| import time, one run | 49.78 s | 46.07 s |

Span dirty restore failed separately; it is not a successful benchmark case.
These are individual observations, not a timing distribution or a full
equivalence proof.

Read-only redb table statistics taken after the restore probes showed that
`git_commit_closures` alone has 1,097 rows, 19,320,364 stored bytes, 45,827
metadata bytes and 8,068,817 fragmented bytes in each database. The span
tables have 2,362 span nodes and 4,278 edits; the graph and inode-graph tables
have 732 and 688 edges, respectively. The linewise graph has approximately
500,000 edges in each index. Both database SHA-256 hashes were unchanged by
the table inspection. The restore probes mean these table snapshots are not
identical to the preceding immediate post-compaction size snapshots.

Closure rows persist exact interpreted Git parent visibility and reject a
conflicting reimport. The repeated full hash lists identified a missing port:
the documented historical run used `git-closure-sharing`, a content-addressed
trie sharing unchanged subtrees. That implementation and its capability fence
have now been ported, retaining legacy-row reads, immutable conflict checks,
and fail-closed validation of referenced nodes.
The fresh stream passes the hashes of all preceding streamed commits to
`apply_synthesized_change_in_txn`, which persists the complete closure for
each Git SHA in the same transaction as its change. These rows are therefore
written during the initial import, not by an incremental import. They also
support later parent interpretation, including across runs and views; the
  the earlier refusal of incremental import into span repositories did not
  remove that persistence requirement.

A second one-run release comparison after porting closure sharing used the
same Git tip, 1,097 first-parent commits, and 75 checked files per clone. The
binary SHA-256 was
`a1e57c769e7121976c7e8adc0d76cf9cacb6d62f1eaf7c59543b0a66507d45f6`:

| Measure | Span | Linewise |
|---|---:|---:|
| Compacted redb logical bytes | 9,592,832 | 54,460,416 |
| Entire `.atomic` bytes | 18,338,385 | 65,047,366 |
| Import time, one run | 40.95 s | 32.24 s |

Span dirty restore still failed separately. Read-only inspection after that
probe showed 10,960 shared closure nodes (1,052,160 stored bytes) and 1,097
closure roots (92,148 stored bytes), with the database hash unchanged by
inspection. The result is **not the 6.30 MB target**: unlike the historical
experiment, this run retained normal `.atomicignore` bootstrap and shim
checkpointing. KG tables also hold substantial data in the current run; their
contribution to the difference has not been isolated with an otherwise matched
experiment. In particular, native bootstrap's `kg_enrich_change` calls
`project_view_kg`, which creates change nodes and direct membership edges for
every member of every view, including the private import stage. Do not omit
these rows from a normal import to claim the historical size. A later source
change only fixes a Clippy lint and strengthens the 16-commit test; the measured
binary predates those changes. A separate debug benchmark was invalidated when
its binary was replaced during the run and must not be used as evidence.

## Verified so far

- The port's 16-commit real-Git unit fixture passed: all selected files at
  every imported commit matched the Git trees; current materialization,
  clean status and reopen also passed. Every tested Git change had a typed
  span section and no fabricated historical FileOps. Exact Git interpretation
  closures were also verified for every commit before and after reopen.
- Nine closure-sharing tests passed, covering arbitrary and legacy vectors,
  shared histories, corruption, idempotence, rollback, and capability fencing.
- `SpanRecordState::authored_ranges` and `authored_anchor` preserve actual
  change owners across rendered span boundaries and reject invalid mappings.
  Native span recording now uses these checked mappings. A real two-commit Git
  fixture verifies that the resulting bytes come from multiple original Git
  changes, both before and after reopening. This is a building block, not a
  historical semantic-trunk implementation; the projected native bridge below
  uses these mappings without fabricating historical CRDT rows.
- A five-commit compiler replay test verifies that recompiling the first two
  imported commits after reopen reproduces their exact persisted hashes, and
  the remaining three hashes match a fresh five-commit import.
- `Repository::validate_span_git_prefix` checks a replayed prefix against the
  selected view, Git SHA index, content-addressed changes, original Git parents,
  dependency index and persisted interpretation closures. The replay test
  covers wrong hashes, order, missing commits and a view that cannot see the
  prefix, and verifies database bytes remain unchanged. This is a read-only
  check, not a tree proof, a cleanliness check, or a lease; callers must repeat
  it under operation locks before publication.
- An internal, restricted `GitGraphImportSession::begin_span_incremental`
  reuses the existing private stage only when both target and stage contain
  the verified Git prefix in order, the target tree matches the pinned baseline,
  and no inherited members or unrelated native changes are present. A single
  structurally verified native `.atomicignore` creation is retained and checked
  by bytes, mode and kind through promotion. It leases the
  unchanged target, journals private batches, rejects unrelated existing SHA
  identities and reuses a matching identity only after a verified inverse
  recovery operation,
  and promotes only after final tree verification. Focused real-Git tests cover
  two successive updates, wrong baseline, SHA collision, wrong final tree,
  native-change refusal and reopening. A real CLI-initialized import preserves
  the recorded `.atomicignore`, its unrecorded working edit, Git HEAD and index
  while the internal continuation publishes later commits. An abandoned
  **verified** private stage can resume only after exact replay and verification
  of its reachable batch operations in both journal scopes; a wrong staged hash
  refuses without journal changes. Other native changes and additional
  vault/private files remain unsupported.
- The feature-enabled CLI accepts `atomic git import --span --incremental`
  for an attached, clean, compatible first-parent checkout. It pins and
  repeatedly rechecks Git HEAD and index, privately stages verified span changes,
  and refreshes the verified shim checkpoint after promotion. Real process
  probes passed two updates including deletion, binary content, an executable
  and a symlink; status and diff succeeded afterward, while untracked bytes,
  Git HEAD and index were preserved. Dry-run remains explicitly unsupported.
- A real child-process kill regression exercises before-commit, after-commit,
  after-metadata and after-verification windows in a two-change private batch;
  verified recovery and an exact retry preserve prior and untracked bytes.
  Another child kill after promotion but before checkpoint refresh verifies an
  idempotent retry restores checkpoint coherence. These are targeted macOS
  fixtures, not complete CLI or cross-platform crash coverage.
- The span semantic projection is now tested against two independently edited
  views in both insertion orders: line/token fragments retain their original
  owners and reconstruct the recorded bytes. A separate corruption test removes
  a visible edit-index row, reopens the database, and confirms projection refuses
  instead of silently attributing incomplete content.
- Projected lines, tokens and complete semantic baselines have domain-separated
  identities derived from the portable file and exact authored byte fragments,
  not repository-local node IDs or invented single-author CRDT identities. A
  test reindexes the same immutable changes into a second pristine with a
  different inode and change IDs, compares identities in both view orders, and
  checks both repositories after reopening. Projected native edits now store a
  baseline identity in a typed semantic FileOp, checked against their exact
  dependency closure. Equal tokens keep their original authored spans; large
  token matrices fail closed rather than rewriting attribution. The separate
  repository transfer regression copies exact change bytes, inserts dependency
  closure, materializes and reopens; it does not exercise network transport.
- Incremental span import refuses `GIT_INDEX_FILE` rather than observing a
  different index from the caller's Git process. The observer rechecks HEAD,
  index bytes, the index lock and Git operation state after collecting status;
  a real CLI regression verifies the refusal preserves both index files, HEAD,
  working bytes and the database. It also refuses index entries marked
  `assume-unchanged` or `skip-worktree`, which can hide tracked edits from Git
  status; a process test exercises both flags and preserves the hidden bytes.
  These observations do not lock Git itself:
  a Git writer can still race after the final check, so this is not a complete
  concurrent-mutation guarantee.
- Explicit span/graph format fencing and refusal of an already-staged path
  before capability installation passed focused tests.
- Focused native-edit tests now exercise two successive edits, independent
  divergent views, overlap refusal, wrong-baseline/path refusal, empty files,
  binary/text transitions, CRLF, Unicode, and mixed projected/CRDT files in
  one record. Real CLI tests cover record, blame, recorded diff, and dirty
  restore. A macOS child-process test kills recording after capability fencing,
  object save, graph apply and verification; recovery and retries preserve the
  working bytes and capability. Undo retains the capability. A pre-bridge span
  binary refused normal access without changing database or Git bytes.
- One broad rerun timed out during an undo test; 60 subsequent isolated repeats
  passed. The cause of the timeout is not established, so it is not a full pass.
- A two-commit, one-file *debug* benchmark smoke test checked the Git blobs,
  modes, schema headers, import and compaction. It demonstrated that dirty
  `restore --force` succeeds on the graph control and refuses on the span
  candidate. This tiny fixture is not evidence of a storage improvement.
- Default workspace `cargo check` and Clippy passed on Rust 1.98; core and
  repository checked on Rust 1.90. Feature-enabled CLI Clippy passed for the
  binary target. Feature-enabled `--all-targets` Clippy did **not** pass: it
  reported `manual_is_multiple_of` in the existing real-import sample and
  `items_after_test_module` in the previously modified service module.
- `git diff --check` passed at the stopping point.
- Ordinary full, incremental, dry-run and detached shim import into an existing
  span repository still refuse at capability preflight. The explicit span-aware
  incremental command uses a separate capability-checked path. Focused tests preserve raw
  database and Git state; CLI probes also compared both reachable operation
  DAGs and receipts. A linked-worktree regression exercises import and shim
  refusal without creating local Atomic state. The preflight retains the
  common lock through the operation; a concurrent initialization test and
  an ordinary incremental-import regression passed. Focused synthetic tests
  also refused a legacy-named span database and an unreadable database without
  migration or repair; these do not simulate a redb crash-recovery condition.

The local `dev` release CLI used for read-only project context was built from
Git commit `54efd7452f546b22796c479e5496d77b3651400a` in an isolated source
archive. With `ATOMIC_SERVICE=local` and a read-only sandbox, it can read goals,
memories, intents and indexed code from `/Users/leefaus/Projects/atomic`.
The versioned binary at
`/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/atomic-dev-release-jXW6jjFC/target/release/atomic`
reports `atomic 0.20.0 (dev 54efd7452f54)` and has SHA-256
`82def01327c96273e7b5200a85afe4ec6d3704f27b05ac1199f969217327a391`.
The adjacent `atomic-context` wrapper runs it from the original checkout with
local mode and a sandbox denying writes outside the approved temporary root.
The linked worktree still requires working-copy identity migration; no writable
open or migration was performed to bypass that gate. The original repository's
index describes its own recorded view, not the uncommitted clean-worktree
source, so current code must still be checked directly.

## Later safety and regression work

- Installing projected semantics permanently raises the repository schema to
  2 and requires `span-projected-semantics`; ordinary repositories remain on
  schema 1. Both directions of the schema/capability consistency check fail
  closed, and legacy merge preserves the source fence. Undo cannot remove it;
  projected capability leases reject version zero and downgrades.
- The schema registry now includes all five span index tables. Legacy merge
  copies and digests their rows, and detects later mutation. This fixes an
  earlier fail-closed refusal of real span databases: the previous registry
  test did not scan the file defining those tables.
- An older span-capable client refused status and compaction on schema 2. Its
  compactor nevertheless opened redb writable before refusing: in a disposable
  test the raw database bytes changed. The checked operation histories, change
  objects, Git state and working bytes survived. After registering span tables,
  the fixture's 80 populated registered tables and 384 rows also matched by
  digest before and after the refusal. This is still not a no-mutation pass or
  a general guarantee for other databases. Current compaction preflights
  read-only, checks operation state and capabilities, and holds the common lock.
- Projected change objects are staged, synced and published without overwrite;
  collision, concurrent publication and process-kill tests exercised refusal and
  recovery. Projected redb changes survive reopen and export/reimport; selective
  span reads remain fenced. Non-Unix projected publication is unsupported.
- A corrected harness run through suite 42 on the candidate identified below
  produced 30 passed, 16 failed and two partial suites; suite 15 needs a
  separate PostgreSQL-backed server and was not run. Suite 03 passed after an
  earlier timeout. The same 23 suite 17 merge failures and the relevant suite
  22, 28 and 40 failures also occur on an independently built pre-bridge
  control. Suite 18 exceeded its 30-second budget while other builds ran.
  The partial suites skip shared-child coverage and unproven contention,
  respectively; none of these failures or skips are passes.
- Suite 05's missing semantic counts came from graph-edit statistics being
  accumulated only for span edits. That accounting bug is now fixed, and the
  Atomic-only suite passed 209/209 on a subsequent debug build. Its no-final-
  newline case still deliberately uses the existing whole-file replacement
  safety fallback; this fix does not change provenance or graph operations.
- A later candidate (SHA-256
  `648449396d72a488b7c60d85ac0ce44038fa5e20381b5766aa8a67c694c6b7fa`)
  measured 9,515,008 compacted span bytes versus 54,435,840 graph bytes for
  the same 1,097 commits and 75 files. Both single-file dirty-restore probes
  passed with Git index unchanged. A second run measured 9,576,448 versus
  54,456,320 bytes and preserved exact post-compaction snapshots: both had
  25,345 KG rows and 3,119,615 stored KG bytes. Runs overlapped other work, so
  timings are not reliable. These results predate the latest registry and
  capability-lease fixes; no final-source matched benchmark or full final
  harness pass has been established.

## Blockers and safety gates

1. The new projected semantic bridge is experimental and needs independent
   review and broader crash, remote transport, recovery and adversarial tests.
   Git history still has no fabricated CRDT rows. Generic CRDT-only consumers
   must refuse projected operations rather than silently discard them.
2. The explicit incremental span command remains experimental and constrained;
   native changes beyond the `.atomicignore` bootstrap, vault/private files,
   detached/other views, dry-run, concurrent Git mutation and broader recovery
   require further validation. Ordinary Git import and shim reconciliation still
   refuse span repositories before writable effects.
3. The imported graph-only span changes need transport, cross-view insertion,
   clone, repair, crash and older-client fencing tests before promotion.
   The 16-commit test is not a complete history or recovery proof.
4. Check the port's private batch atomicity, capability/lock ordering, staged
   verification and identity-loss metadata against the older branch's tests.
   Do not weaken operation leases, receipts, recovery or final-tip proof.
5. Repeat the paired storage comparison with a fingerprinted final binary
   after source changes stabilize; the historical dirty-restore failure is not
   evidence about the new bridge.
6. Rerun the affected harness and workspace checks on the final source. The
   previous 01–14 passes were from different builds; 10–13 exercise Git
   import and do not prove native span history. Later overlap/name-conflict
   and Git-shadow failures remain open.

## Scratch artifacts

`/private/var/folders/hl/fwbg_jc93tvgz43rj7t25x240000gn/T/opencode/span-harness01-1gFvn7/`
contains `test-git-span-port.sh`, `probe-git-span-cli.sh`,
`build-git-span-release.sh`, `measure-git-span-storage.py`, the debug candidate
`atomic-debug-git-span`, fixture logs and earlier harness results. This is
temporary storage and may be cleaned by the system. The measured feature-enabled
release build completed with `rust-objcopy` missing-`libLLVM.dylib` warnings;
it is not a final release candidate.

No remote push or OpenCode hook-pin change was made. No commit or checkpoint
was created for this unfinished work.

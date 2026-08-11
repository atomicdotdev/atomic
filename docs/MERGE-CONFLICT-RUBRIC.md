# Merge & Duplication Scenario Rubric + Conflict Surfacing Design

Status: research / proposal
Related: `docs/CROSS_VIEW_MERGE_RCA.md`, `docs/SEMANTIC-MERGE.md`,
`tests/harness/17_cross_view_merge.sh`, `tests/harness/22_switch_conflict_markers.sh`,
`tests/harness/24_concurrent_insert_conflict.sh`, `tests/harness/27_merge_ordering_duplication.sh`

## Why this document exists

The nine defects in the cross-view merge RCA were found reactively — a user
hit corruption, we fixed the specific shape, and a new shape appeared. The
harness now has four suites (17, 22, 24, 27) that each cover the shapes we
happened to hit. This document replaces reactive discovery with an
**enumerated scenario space**: every way duplication can occur, every way a
conflict can occur, and which harness case covers each cell. Anything
uncovered is a known gap, not an unknown unknown.

It also specifies how conflict state should be **surfaced** (persisted and
shown by `atomic status`) instead of living only in conflict markers buried
in files, and records our assessment of four-way merge.

---

## 1 · Where duplication and conflicts can actually originate

In Atomic, "merge" is not one operation. Content passes through four stages,
and each stage can independently corrupt or conflict:

| Stage | Code | Failure modes |
|-------|------|---------------|
| **Record** (working copy → hunks) | `atomic-repository/src/repository/record.rs`, `record/workflow/` | Wrong base ⇒ full-file rewrite hunks (self-inflicted duplication later); wrong up/down context positions |
| **Apply/Insert** (hunks → GRAPH edges) | `atomic-core/src/apply/`, `repository/insert.rs` | Zombie edges, missing-context, duplicate edge writes on re-insert |
| **Retrieve/Order** (GRAPH → linear order) | `output/alive/{retrieve,order}.rs`, Tarjan SCC | False conflicts between commuting changes; wrong SCC membership; tail emitted per side |
| **Output** (order → bytes on disk) | `output/repo/file.rs`, `merge/engine.rs`, `ResolvedConflicts` | Marker placement, skip-set misses ⇒ duplicated vertices, auto-merge writing both sides |

**Key insight from harness 27:** cases 1–3 there are not merge-theory
failures — patch theory says those changes commute. They are
**linearization failures** (stages 3–4). This matters for the four-way-merge
question in §5: a better merge algorithm cannot fix a broken serializer.

---

## 2 · The rubric

Two orthogonal axes. Axis A enumerates the *relationship between the two
edits* (this is what patch theory sees). Axis B enumerates the *pathway* the
edits took to converge (this is what the implementation sees). A scenario is
a cell (A, B). Duplication and false conflicts are **always wrong** in every
cell; true conflicts are correct only in the cells marked ⚠.

### Axis A — edit relationship (per file)

| # | Relationship | Correct outcome |
|---|--------------|-----------------|
| A1 | Disjoint regions (prepend vs append, different functions) | Clean merge, both edits, no markers |
| A2 | Adjacent regions (insert at line N vs line N+1) | Clean merge, deterministic order |
| A3 | Same position, **identical content** (both sides made the same edit) | Deduplicate — content appears once |
| A4 | Same position, different content (concurrent insert) | ⚠ Order conflict (or CRDT auto-merge if different tokens) |
| A5 | Same line, different tokens | Token-level auto-merge (`SemanticMergeEngine`) |
| A6 | Same token, different content | ⚠ True conflict |
| A7 | Delete vs modify (one side deletes what the other edits) | ⚠ Zombie conflict |
| A8 | Delete vs delete (both delete same region) | Clean — deletion happens once |
| A9 | Delete region vs insert *inside* that region | ⚠ Zombie conflict |
| A10 | Move/rename vs edit | Clean (inode survives rename) |
| A11 | Rename vs rename (different targets) | ⚠ Name conflict |
| A12 | Create same path independently on both sides | ⚠ Name conflict (two inodes, one path) |
| A13 | Edit vs full-file rewrite (recorded against stale base) | Legitimately both copies OR conflict — never silent interleave |
| A14 | Empty file / EOF-no-newline / trailing-newline boundary edits | Clean, byte-exact |
| A15 | Binary or non-UTF-8 content, same-position edits | ⚠ Whole-file conflict (no token merge) |

### Axis B — convergence pathway

| # | Pathway | Notes |
|---|---------|-------|
| B1 | `insert <hash>` draft → current view | harness 27's shape |
| B2 | `insert from-view` (bulk, with dep closure) | harness 17's shape |
| B3 | Insert into a **parent** view while a child draft watches (live perspective) | draft may legitimately show markers (harness 22 invariant) |
| B4 | View **switch** after divergent edits | must NEVER invent markers on a shared view (harness 22) |
| B5 | `pull` from remote (same graph ops, remote change source) | how the 236→454-line plugin corruption arrived |
| B6 | Repeated/idempotent insert (same change inserted twice) | must be a no-op, not duplicate edges |
| B7 | Three or more concurrent sides (N-way fork) | SCC with >2 vertices; markers must nest correctly |
| B8 | Conflict left unresolved, then **another change** lands on top | zombie-on-zombie; supersession (RCA §5.8) |
| B9 | Conflict resolved by recording over markers, then re-materialize | resolution must stick; markers must not resurrect |
| B10 | `unrecord` / `reinsert` of a change that participated in a merge | orphaned edges must not re-linearize as content |
| B11 | Sequential chain on one side vs single change on other (dep depth ≥ 2) | harness 17 case 6 |
| B12 | Same logical edit arriving via two paths (diamond: draft→dev and draft→release→dev) | dedup by change hash, not by content |

### Cell classification

For every cell (A×B) the invariants are the same four:

1. **No silent duplication** — every logical line appears exactly once
   (except inside conflict markers, where each *side* appears exactly once).
2. **No false conflict** — commuting changes never produce markers.
3. **No lost edit** — both sides' content is present (in body or in a marker).
4. **Honest exit state** — if the file contains markers, the repo must
   *know* it (see §4); `status` must not report clean; `record` must not
   silently bake markers in.

### Current harness coverage map

| Cell | Covered by |
|------|-----------|
| A1×B1 | 27 case 1 |
| A4×B1 (+ tail scaling) | 27 cases 2–3 |
| A1/A4/A7×B2 | 17 cases 1–5 |
| A3×B2 | 24 case 1 (identical concurrent edits dedup) |
| A4×B2 well-formed markers | 24 case 2 |
| stability across switch | 24 case 3, 27 case 4 |
| supersession (B8) | 24 case 4 |
| B3/B4 no-marker invariants | 22 |
| A?×B11 | 17 case 6 |
| A1×B1, A2×B2, A3×B2, A4×B1 (+ honesty) | **28** (matrix) |
| B6 idempotence, B4 switch round-trip | **28** |
| A7×B2, A8×B2 | **28** (real assertions since ATOM::25) |
| A9×B2 delete-region vs insert-inside (reattach, lossless) | **28** (ATOM::29) |
| A14×B1 no-trailing-newline byte-exact | **28** (ATOM::29) |
| B9 resolution sticks across switch round-trip | **28** (ATOM::29) |
| B12 diamond dedup by change hash | **28** (ATOM::29) |
| A15 binary conflict: surfaced + no base residue | **28** (ATOM::31, real assertion) + `conflict_surface_tests.rs` |
| A12 same-path independent create → name conflict surfaced | **28** (ATOM::30, real assertion) + `conflict_surface_tests.rs` |
| B7 N-way fork (three concurrent sides nest correctly) | **28** (ATOM::32, real assertion) |
| A10 rename vs edit (inode survives; edit rides along) | **28** (ATOM::37, real assertion) + `rename_tests.rs` |
| honesty invariant across pathways | **28**, Phase-5 property `prop_conflict_state_is_honest` |
| **Tracked bugs (xfail_correct)** | A11 rename-vs-rename → name conflict not surfaced (last-writer-wins; ATOM::37, §6.7) |
| **Remaining gaps** | A5/A6 (token level, end-to-end), A11 (rename-vs-rename name conflict — tracked xfail, §6.7), A13, B5 (pull), B10 (unrecord-by-hash unimplemented, §6.6) |

Suite `28_merge_rubric.sh` (Phase 6) is the consolidated matrix: one fresh
repo per cell, a parameterized `build_case`, and the four invariants asserted
uniformly — including the honest-exit-state invariant now that Phases 1–3
surface conflicts. It fails cleanly against a pre-fix binary (the old release
shows tail duplication + broken honesty on A4), so it genuinely distinguishes
fixed from unfixed. Suites 17/24/27 keep their anecdotal regression value; 28
is the systematic grid. The Rust property suite (Phase 5,
`merge_property_tests.rs`) covers Axis A generatively below the CLI; 28 covers
the pathways (insert / bulk / switch / repeat) the property tests cannot reach
through the binary.

---

## 3 · The surfacing gap (verified in code)

Today the conflict information **exists and is then discarded**:

- The output layer produces rich per-conflict data: `Conflict { conflict_type,
  path, inode_vertex, line, changes, id }` (`atomic-core/src/output/mod.rs`)
  and `FileOutputResult.conflicts` → `MaterializeResult.conflicts`
  (`output/repo/repository/mod.rs`).
- `insert`/`insert_from_view` reduce all of that to a single boolean
  `has_conflicts` (`atomic-repository/src/repository/insert.rs`).
- The CLI prints *"Conflicts detected. Run 'atomic status' to see details."*
  (`atomic-cli/src/commands/insert.rs`) — **but `Repository::status()` never
  produces `FileStatus::Conflicted`** (`atomic-repository/src/repository/status.rs`
  has no conflict path). The CLI's `C` code, `conflicted()` iterator, and
  `is_recordable(Conflicted) == false` logic are all dead code today.
- Nothing is persisted. After the process exits, the only trace of a
  conflict is inverted markers (`>>>>>>>`/`=======`/`<<<<<<<`) in file bytes.

So the two failure modes reinforce each other: linearization bugs corrupt
files *silently* (harness 27's "data-integrity bug"), and even *correct*
conflicts are invisible to `status`, letting a later `record` capture
markers as content.

---

## 4 · Design: persistent conflict state surfaced by `atomic status`

### 4.1 Storage

New pristine table (follows existing patterns in `pristine/tables.rs`):

```
CONFLICTS
  Key:   (view_id: u64, inode: u64)          [u8; 16]
  Value: ConflictRecord (bincode, versioned):
    - path: String            (at detection time; inode survives renames)
    - kind: ConflictType      (Order | Zombie | Cyclic | Name)
    - line: u64               (first marker line, 1-based)
    - sides: Vec<Hash>        (changes involved)
    - detected_by: Hash       (the inserted change that triggered it)
    - state_at_detection: Merkle
```

Multimap semantics (one file can hold several conflicts), same as GRAPH.

### 4.2 Write path

- `materialize_*` already returns `MaterializeResult.conflicts`. The callers
  that mutate view state (`insert_change`, `insert_change_rec`,
  `insert_from_view`, `switch_view`, `pull`) write each `FileConflict` into
  `CONFLICTS` in the same transaction that records the insert.
- Re-materialization **replaces** the entries for the inodes it touched
  (conflicts are a function of graph state, so they are recomputed, not
  accumulated).

### 4.3 Clear path (resolution lifecycle)

A conflict is resolved when a `record` on that view captures the file
*without* markers, or a superseding insert removes the fork (RCA §5.8):

- `record()`: for each recorded file whose inode has `CONFLICTS` entries,
  scan the recorded content for marker lines. Markers absent ⇒ delete the
  entries (the recorded change *is* the resolution — this matches the
  existing supersession model). Markers present ⇒ **refuse to record that
  file** unless `--allow-conflict-markers` is passed. This closes the "bake
  the damage into history" hole directly.
- `del_view` cleanup deletes `CONFLICTS` by view prefix (same as
  VIEW_CHANGES).

### 4.4 Read path

- `Repository::status()`: after the existing tree walk, iterate
  `CONFLICTS[view_id]`; emit `FileStatusEntry` with
  `FileStatus::Conflicted`, details = `"order conflict with <hash12> at
  line N"`. The CLI already renders this (long format prints
  `conflict: (...)`, short format prints `C `).
- New `atomic conflicts` subcommand (or `atomic status --conflicts`) for the
  detail view: per-conflict sides, kind, line, triggering change — the data
  is already in the record.
- `print_insert_outcome` upgrades from the boolean to a real list:
  `"2 conflicts in src/plugin.ts (order, lines 14, 88) — atomic conflicts for details"`.

### 4.5 Safety property this buys

Even if a *new* linearization bug silently duplicates content, invariant 4
of the rubric becomes checkable at zero cost: the fuzz/matrix suite can
assert `status`-clean ⇔ markers-absent ⇔ line-counts-exact. Any divergence
between the three is a caught bug instead of a silent one.

---

## 5 · Four-way merge: assessment

"Four-way merge" in the literature extends three-way merge (base, ours,
theirs) with a fourth input — typically either the *previous merge
resolution* (so re-merging after new changes reuses your earlier conflict
resolutions) or the *target/working state*. The adjacent research worth
knowing (verified available):

- **Khanna, Kunal & Pierce, "A Formal Investigation of Diff3"** — proves
  textual 3-way merge is inherently unstable (adding an *unrelated* change
  can flip a clean merge into a conflict). This is the theoretical reason
  git-style merges need help at all.
- **"Evaluation of Version Control Merge Tools" (ASE 2024,
  arXiv:2410.09934)** — benchmarks line/token/AST merge tools; finds hybrid
  token-level approaches handle most real scenarios; a useful evaluation
  harness model for us.
- Structured-merge tools (Spork, Mergiraf, IntelliMerge) — AST-based 3-way.

**Assessment for Atomic: four-way merge solves a problem we don't have, at
the stage where we don't have it.**

1. Three/four-way merge algorithms exist to *infer* what changed from three
   or four snapshots, because snapshot-based VCSs (git) throw the operations
   away. Atomic never loses the operations: every edit is a recorded change
   with exact positions, dependencies, and provenance. Our merge is a
   pushout in patch theory — strictly more information than any n-way
   snapshot merge can recover. Adopting 4-way would be a downgrade in
   theory and an architectural bolt-on in practice.
2. Our observed corruption (harness 27, RCA defects 5.1–5.9) occurs in
   **graph linearization**, after the merge is already correct in the graph.
   A four-way merger sitting in front of a broken serializer would emit the
   same duplicated tails.
3. The one genuinely valuable idea in 4-way — **reusing prior conflict
   resolutions when re-merging** — we already get structurally: a recorded
   resolution is a change that supersedes the fork (RCA §5.8), and §4.3
   makes that lifecycle explicit. If re-resolution churn shows up as a real
   pain point, the answer is a resolution-memory table (git rerere
   equivalent) keyed by conflict side-hashes, not a different merge
   algorithm.

Recommendation: **do not pursue four-way merge.** Invest in (a) the rubric
matrix suite so linearization is exhaustively pinned, (b) conflict
persistence + status surfacing, (c) finishing the token-level CRDT merge
path (`docs/SEMANTIC-MERGE.md`), which is our equivalent of — and superior
to — the structured-merge research direction.

If the specific paper that prompted this differs from the above framing,
link it and we'll re-evaluate against these criteria: does it help at the
linearization stage, and does it use information we don't already have?

---

## 6 · Gaps outside the harness

The harness is the slowest, coarsest layer. Survey of what exists below it
(verified 2026-08):

| Layer | State |
|-------|-------|
| Rust integration | `cross_view_merge_tests.rs` (8 tests, mirrors harness 17), `record_duplication_tests.rs`, `status_tests.rs` |
| Unit tests | `output/alive/order.rs` 40, `output/repo/file.rs` 46, `merge/three_way.rs` 22, `merge/engine.rs` 10 — pieces in isolation |
| Property tests | **None.** AGENTS.md documents QuickCheck; zero `quickcheck`/`proptest` usage exists in atomic-core or atomic-repository |
| Runtime invariants | None in the output path |

The RCA already concluded ("Test End-to-End") that isolated unit tests miss
the interaction bugs. Missing layers, ranked by leverage:

### 6.1 Property-based tests (highest leverage)

Patch theory correctness is defined by algebraic laws — the ideal domain for
generative testing. A generator producing random (base, edit-A, edit-B)
triples, classified into rubric cells, covers Axis A by construction:

- **Commutation**: independent changes in either order ⇒ byte-identical output
- **Convergence**: any dep-respecting permutation of a change set ⇒ same output
- **Idempotence**: double-insert ⇒ no-op (B6)
- **Round-trip**: record → apply → materialize ⇒ original bytes (incl. empty,
  no-trailing-newline, non-UTF-8 — A14/A15)
- **Honesty**: `has_conflicts` ⇔ markers on disk ⇔ `status` Conflicted

The honesty property converts every future silent-corruption bug into a loud
CI failure. Pair with a ~100-line in-memory reference model (list-of-lines
with op replay) as a differential oracle.

### 6.2 Runtime invariant assertions

The output stage knows the alive vertex set before writing. Assert **each
alive vertex is emitted exactly once** (and no skipped vertex is emitted).
This would have turned all three harness-27 cases into panics at the
faulting line instead of silent file growth, and it guards cells nobody
enumerated. Cheap enough to keep always-on behind a config flag;
`debug_assert` at minimum.

### 6.3 Verifier command (`atomic doctor check`) — DONE (ATOM::27)

`atomic doctor check` (read-only; `Repository::verify_working_copy`)
recomputes each visible file's content from the graph and reports two classes
of problem, exiting non-zero when any are found:

1. **Materialization drift** — a file `status` considers *clean* whose
   on-disk bytes differ from what the graph would materialize (silent
   corruption, modulo uncommitted edits which are skipped).
2. **Conflict honesty** — the rubric invariant: on-disk markers ⇔ `status`
   Conflicted ⇔ `list_conflicts` includes the file. Any disagreement is a
   caught bug.

Regression tests in `verify_tests.rs`: clean repo healthy; a real conflict is
honest (not a problem); and injected same-length drift (FILE_INDEX pointed at
the old hash to defeat the status fast-path) is caught.

Not yet covered: CRDT-table ↔ graph consistency (RCA §11 showed CRDT tables
silently go stale) — needs pristine introspection beyond the byte-graph and
is left for a follow-up.

### 6.4 Pathways no current layer reaches

- **Pull-driven merge (B5)** — the original corruption arrived via `pull`;
  harness 07/15 cover push/storage but not merge-under-pull; no Rust test
- **Concurrent access** — two processes, redb lock hand-off, pull racing record
- **Crash mid-materialize** — partial writes vs the mtime/FILE_INDEX cache:
  does re-run heal, or does the cache mask damage?
- **unrecord/reinsert of a merge participant (B10)** — 19_unrecord exists,
  merge interplay untested

### 6.5 FIXED — whole-file delete via insert (A7/A8; ATOM::25)

**Symptom.** Inserting a whole-file deletion into a view where the other side
is unchanged silently removes only the FIRST line and leaves the rest on
disk, with `atomic status` reporting a clean tree and no conflict markers.
Repro (fixed 5-line file):

```
# feature deletes f.txt entirely; dev is unchanged
atomic insert <feature-delete-hash>     # on dev
cat f.txt        # => beta,gamma,delta,epsilon  (alpha silently dropped!)
atomic status    # => working tree clean
```

Three manifestations, all the same root cause:
- delete vs unchanged (insert): tail survives, only line 1 dropped.
- delete vs modify (A7): unmodified first line silently lost, no conflict.
- delete vs delete (A8) via **bulk** `insert from-view`: the file is
  RESURRECTED (a single-change `atomic insert` of one delete leaves it
  absent; the bulk re-materialize brings it back).

**Root cause — deeper than first thought (ATOM::24 investigation).** The first
hypothesis was the `deleted_lines = vec![0]` placeholder in
`record_deleted_file` (atomic-core/src/record/workflow/record/mod.rs): with a
non-empty `deleted_lines`, `globalize_delete` takes its *targeted* branch and
deletes only `sorted[0]`. That is real, but fixing it (empty `deleted_lines`
→ fall through to `delete_all_content`) does **not** fix the observable bug.
Two layers are implicated, established by trace:

1. **Record representation.** `atomic record` correctly *detects* the deletion
   (`status` shows `D f.txt`), but the recorded change renders as
   `+1 vertices, ~0 edges` / `~ f.txt (+1 span: new content)` — an *add-span*
   op, not deletion edges. So the deleted-file record path is not emitting the
   deletion the way `globalize_delete` assumes; routing it through `sorted` or
   `delete_all_content` did not change the emitted change (`~1 edge`),
   implying the active path bypasses the edit made.
2. **Cross-view propagation.** Inserting a whole-file-delete change into a
   view where the other side is unchanged does not propagate the deletion at
   all under the change filter (with the empty-`deleted_lines` variant the
   file was left fully intact). Delete-vs-delete only works because *each*
   view carries its own delete.

**Fix (ATOM::25).** Op-level Rust diagnostics
(`delete_propagation_tests.rs`) — not CLI renderer output, which had
misrendered the deletion as "+1 span: new content" — established the true
two-part cause:

1. **Record:** `record_deleted_file` passed `deleted_lines = vec![0]`, so
   `globalize_delete` took its targeted branch and emitted ONE deletion edge
   (`sorted[0]`). Fixed: whole-file deletes carry an empty `deleted_lines`
   and `globalize_delete` emits **`GraphOp::FileDel`** with deletion edges
   for every content vertex via `delete_all_content`. Using `FileDel` (not a
   bare `Edit`) makes the delete-intent travel with the change — the tree
   layers (`collect_tree_ops`, `insert_change`, `write_recorded`) already
   understand it; the old `Edit` form only worked at record time via the
   out-of-band `deleted_files()` list, which does not exist on insert.
   Truncate-to-empty still records as `Edit`, keeping the two intrinsically
   distinguishable.
2. **Insert:** materialize only writes or skips — it never removes. Fixed:
   after applying a change into the current view, `insert_change` re-checks
   each `FileDel` path's visible content and, when it is truly gone, removes
   the stale working-copy file (+ FILE_INDEX entry). A delete-vs-modify merge
   where a modified line survives yields content and is rewritten instead —
   patch-theory semantics (each line's fate independent) preserved.

The three harness cells xpassed and were promoted to real assertions
(28: 36/36). Regression guards: `delete_propagation_tests.rs` (op-level edge
count, insert-removes-file, delete-vs-modify survivor, delete-vs-delete
no-resurrection); full core (3383) + repository (803) suites and harness
01/02/04/08/17/19/21/22/24 all green.

Earlier investigation notes (ATOM::24) preserved for the record: the CLI
`atomic change` renderer mislabels a FileDel/EdgeUpdate as "+1 span: new
content" — a display bug worth fixing separately.

**Coverage.** `28_merge_rubric.sh` asserts the CORRECT behavior for these
cells via an `xfail_correct` helper: while unfixed they print a loud
`KNOWN BUG` line without failing the suite; when fixed the predicate
xpasses and hard-fails, forcing promotion to a real assertion.

### 6.6 Final audit pass (ATOM::29)

A read-only probe sweep over the §2 remaining cells (fresh repo per probe,
on-disk bytes + `atomic doctor check` for graph truth, never trusting the CLI
renderer for diagnosis). Results split into verified-correct (now real
assertions in 28) and two characterized bugs (now tracked `xfail_correct`).

**Verified correct — promoted to real assertions.**

- **A9×B2 (delete-region vs insert-inside).** Feature deletes lines 2–4; base
  inserts INSIDE between them. Atomic reattaches the orphaned insertion to the
  surviving neighbours (`line1`/`line5`) rather than raising a zombie
  conflict. This is a deliberate CRDT reattachment policy: INSIDE is preserved
  exactly once, the deleted lines are legitimately gone, status is clean, and
  all three honesty signals agree. The rubric's ⚠ "zombie conflict" label
  (A9) is an editorial preference — all four invariants hold, so it is not a
  bug. (If we later decide zombies MUST surface, this assertion flips.)
- **A14×B1 (no trailing newline).** Base has no trailing newline; disjoint
  first-line/last-line edits merge byte-exact and do NOT invent a trailing
  newline.
- **B9 (resolution stickiness).** A genuine conflict resolved by recording
  clean content over the markers stays resolved across a view-switch
  round-trip — markers do not resurrect, status stays clean.
- **B12 (diamond dedup).** The same change arriving via two pathways
  (draft→release→stage and draft→stage) dedups by change hash: the second path
  is a no-op, the edit appears exactly once.
- **B7 (N-way), now a real assertion (ATOM::32).** A 3-way concurrent insert
  nests markers correctly (one START, N-1 separators, one END for N sides),
  keeps each side and each shared line exactly once, and stays honest. The
  audit probe was promoted to a standing cell in 28.

**BUG A12 — same-path independent create was silently shadowed (HIGH: silent).
FIXED (ATOM::30).** When two views each independently CREATE the same path as
separate inodes, inserting one side's create into the other reported success
but the graph silently materialized only the target view's content. Both
create-changes appeared in the view's log, yet the inserted side's content was
orphaned, no conflict surfaced, and `status`/`atomic doctor check` both
reported clean — violating invariants 3 (no lost edit) and 4 (honest exit
state). Confirmed root cause: `TREE` is a single-valued path→inode index, so
the later recorder overwrote the first (the first inode survived only in
`REV_TREE`/`INODES`, orphaned); insert skips tree-population when the change's
edges are already ambient in `GRAPH`; and materialize enumerates via
`iter_tree()` (the `TREE` table), so the shadow inode was never even seen.

*Fix.* Materialize now recovers the hidden inodes: a new read-only
`ReadTxn::iter_rev_tree()` enumerates every `(inode, path)` pair, and
`materialize_parallel` groups inodes by path. For any path with ≥ 2 candidate
inodes it confirms each is visible (creating change in the view filter) and
alive (`is_file_alive_via_retrieval`, promoted to `pub(crate)`); when ≥ 2
survive it renders a name conflict wrapping every side's body in markers
(`>>>>>>>` / `=======` / `<<<<<<<`). Both bodies are preserved, and the
existing marker-driven pipeline (`first_conflict_marker_line` →
`conflicts_by_path` → `persist_view_conflicts`) surfaces it — `status` shows
the file Conflicted, `atomic conflicts` lists it, honesty holds. The aliveness
probe runs ONLY for paths with ≥ 2 candidate inodes, so ordinary single-inode
files are byte-identical to before. No new graph representation is emitted
(no `SolveNameConflict`); resolution is working-copy-based like content
conflicts. The 28 cell is a real assertion; `conflict_surface_tests.rs` adds
the name-conflict surfacing test plus a single-create false-positive guard.
*Out of scope:* binary-body name conflicts render with the same textual
markers (both bodies preserved, imperfect for binary), and a path that is a
file on one side and a directory on the other (type conflict) is not
specifically handled.

**BUG A15 — binary conflict leaked base bytes outside the markers (MEDIUM).
FIXED (ATOM::31).** A same-position edit to binary/non-UTF-8 content is
surfaced as a whole-file conflict (markers + `status C` + listed by
`conflicts`). But unlike the text path — where replace-vs-replace deletes the
base line cleanly — the binary path left the ORIGINAL base bytes trailing
OUTSIDE the conflict block. Confirmed root cause (NOT the renderer): the
defect was at the record-representation layer. `record_modified_file`'s binary
branch built its whole-file Replace hunk with `deleted_lines = Vec::new()`,
which routes through `globalize_replace`'s PURE-INSERTION branch — it inserts
the new content but never deletes the base vertex. So every binary "replace"
left the old content alive; invisible for a single edit (the new bytes shadow
the old on that view) but leaked as residue under a concurrent merge.

*Fix.* The binary branch now uses the same whole-file-replace sentinel as the
`force_whole_file_replace` path — `deleted_lines = vec![usize::MAX]` — which
forces `globalize_replace_whole_file` → `delete_all_content`, deleting every
base content vertex before inserting the new bytes. Because the deletion is
carried by the change (a `Replacement`/`FileDel`-style op), it travels on
insert into another view, not just at record time. Single-view binary edits
round-trip byte-exact with the base gone; concurrent binary edits merge to a
conflict body containing only the two edited versions. The 28 cell is a real
assertion (`grep -qF BASE` false); `conflict_surface_tests.rs` adds a
round-trip test and a no-residue merge test.

**Feature-gated, not silently omitted.**

- **A10/A11 (rename / rename-vs-rename)** could not be exercised. The initial
  audit note ("needs a rename command first") was WRONG and is corrected in
  §6.7: the command exists; the real blocker is that the record pipeline never
  classifies a rename as a move, so it records delete+add and merges break.
- **B10 (unrecord of a merge participant)** is blocked: `atomic unrecord
  <hash>` returns "not yet supported" (only bare last-change unrecord exists),
  so unrecord-by-hash of a specific merge participant cannot be probed yet.

### 6.7 Rename support scope (ATOM::33) — A10/A11 are NOT a "quick unlock"

The earlier estimate that A10/A11 just needed an `atomic mv` command was wrong.
Investigation found rename support is ~70% wired but broken end-to-end.

**What already exists:**
- CLI `atomic mv`/`move` (`atomic-cli/src/commands/mv.rs`, `Move`): renames on
  disk, then calls `Repository::move_file`.
- `Repository::move_file` → `move_tracked` (`repository/tracking.rs`): eagerly
  updates `TREE` (drop old path, add new path → same inode).
- `GraphOp::FileMove` and its apply/insert handling (`insert_change`,
  `write_recorded`, `write_import_*`).
- Core `DetectionKind::Moved` and a `GraphOp::FileMove` emission in
  `record/workflow/globalize/pipeline.rs`, gated on
  `recorded.kind() == DetectionKind::Moved` with a resolvable `old_path`.

**The decisive gap (grounded in source):**
- The atomic-repository record pipeline has **no Moved classification**.
  `record/assemble.rs::filter_files` recordable set is exactly
  `FileStatus::{Modified, Added, Deleted}` — there is no `Moved`. There are
  **zero** `detect_moves` / `DetectionKind::Moved` references anywhere in
  atomic-repository. So `record` never sets `DetectionKind::Moved`, and the
  FileMove-emitting branch in globalize is **unreachable from record**.
- Consequence: a rename records as **delete-old + add-new** (new inode), losing
  inode/history. And the two mechanisms conflict — `atomic mv` eagerly rewrites
  `TREE`, so even the core content-hash move detection (which is off by
  default, `detect_moves=false`) could not reconstruct the move at record time
  (`get_inode(old_path)` is already `None`).

**Probes (throwaway, current binary) confirm the breakage:**
- `atomic mv f.txt g.txt` + record, then insert into another view that edited
  `f.txt`: left `f.txt` **untracked** with a **stale** `g.txt` (edit lost).
- rename-vs-rename insert failed outright.

**What completing A10/A11 actually requires (multi-layer):**
1. A `Moved` classification in the repository record pipeline (a `FileStatus`
  /`filter_files` concept) that connects a rename — either `atomic mv`'s intent
  (e.g. a pending-move record it stashes) or content-hash detection — to core
  `DetectionKind::Moved`, so globalize emits `GraphOp::FileMove`.
2. Resolve the eager-`TREE`-update vs. detect-at-record tension so record can
  still reconstruct the move after `atomic mv`.
3. Verify/fix `FileMove` **merge** semantics across views (apply handling
  exists but the probe shows cross-view insert of a rename is incorrect today).
4. Only then add A10 (rename vs edit — inode survives, edit preserved) and A11
  (rename vs rename — name conflict) cells to suite 28 + Rust regression tests.

This is a genuine feature-completion + bug-fix spanning detect → record
classification → status → globalize → insert/merge, not a one-command add.

**Staged implementation (green-lit):**
- **Stage 1 — DONE (ATOM::34).** Record now classifies a git-style raw rename
  (`fs::rename(old, new)` on disk, new path left untracked) as a move: a
  Deleted tracked path whose byte-identical content reappears at an untracked
  path is paired into a synthesized `Moved` `RecordedFile`, and globalize's
  existing FileMove emitter produces a single `GraphOp::FileMove` reusing the
  ORIGINAL inode (old path is still in TREE for a raw rename, so
  `get_inode(old_path)` resolves). Single-view round-trip is byte-exact with
  the inode preserved; op-level tests in `rename_tests.rs`
  (`test_raw_rename_records_as_filemove`,
  `..._roundtrip_preserves_inode_and_content`, `test_plain_delete_is_not_a_rename`).
- **Stage 2 — DONE (ATOM::35).** The `atomic mv` command no longer eagerly
  rewrites TREE (it dropped the `Repository::move_file` call); it performs the
  on-disk rename only, leaving the working copy in the raw-rename shape (old
  Deleted, new Untracked) so Stage-1 detection captures it as a FileMove on the
  next `record`. This unifies both rename UXes through one detection path and
  matches the git importer (record a FileMove, let apply update TREE).
  `Repository::move_file` is retained as a low-level primitive but is no longer
  used by the CLI. End-to-end guard: `tests/harness/29_rename.sh` (mv + record
  round-trip, rename-back, subdir move, `doctor` consistent); op-level lock:
  `rename_tests.rs::test_atomic_mv_equivalent_records_as_filemove`. Between
  `atomic mv` and `record`, `atomic status` shows the old path Deleted and the
  new path Untracked — expected (there is no `Moved` status yet). Still open:
  move+edit (content change during a rename — content-hash pairing can't detect
  it; needs similarity-based detection), multi-hop already works via re-pairing,
  and directory renames.
- **Stage 3 — DONE (ATOM::36).** Inserting a `FileMove` into the current view
  now actually applies the rename. Root cause: the insert only JOURNALED the
  TREE move for switch-replay (`append_deferred_tree_ops`) and the eager
  `!already_in_graph` TREE block was skipped (a draft-recorded rename is always
  already-ambient in GRAPH), so materialize — which enumerates via TREE — kept
  the old path (and `doctor` was blind for the same reason). Fix: `insert_change`
  repoints TREE old→new for every `FileMove` targeting the current view
  regardless of `already_in_graph`, and removes the stale old file from disk
  (guarded against an A12-style shared path) so the caller's materialize
  produces the new path; the deferred journal append is retained so a later
  view switch replays idempotently. A10 (rename vs edit) now merges cleanly —
  the concurrent edit rides along because the FileMove reuses the inode. Guards:
  `rename_tests.rs::test_cross_view_rename_applies_on_insert` (incl. switch
  round-trip idempotency) and `..._rename_vs_edit_preserves_edit`.
- **Stage 4 — DONE (ATOM::37).** A10 (rename vs edit) is a real assertion in
  suite 28 (draft renames f→g, base edits f; insert yields g.txt with the
  edited line, no markers, honest). A11 (rename vs rename → different targets)
  is a tracked `xfail_correct`: two views rename the SAME inode to different
  names and the insert silently resolves last-writer-wins (one name kept, the
  other dropped, `status` clean) instead of surfacing a name conflict. The
  correct outcome is a surfaced name conflict; fixing it needs (a) graph-level
  detection of one inode carrying ≥2 alive FOLDER name-edges under the view
  filter, and (b) a name-conflict honesty signal INDEPENDENT of in-file markers
  (the current honesty invariant is marker-in-file, which does not fit a
  filename conflict — unlike A12, where the conflict lives in one path's
  content). That is a dedicated follow-up; A11 is now a LOUD tracked gap, not a
  silent one.

#### A11 design (ATOM::38) — why it is a multi-locus feature, not a patch

Investigation to find the smallest correct A11 fix established that any PARTIAL
fix introduces a NEW bug, so it must be done across all loci together:

- **Blocker — a partial fix is silently reverted.** The deferred-tree resolver
  `desired_tree_paths` (`repository/deferred_tree.rs`) collapses each inode's
  visible `Set` ops to a SINGLE latest-by-journal-order path. So even if
  `insert_change` kept both competing names in TREE, the next `view switch`
  (which runs `apply_deferred_tree_ops_in_txn`) would recompute one desired
  path and delete the other — reverting the conflict to last-writer-wins. The
  resolver and the insert-eager path must agree.
- **Locus 1 — detection (concurrency-aware).** Distinguish a *concurrent*
  rename-vs-rename (surface a conflict) from a *sequential* rename chain a→b→c
  (pick the latest, no conflict). Journal order is only a proxy for "latest"
  and is arbitrary for concurrent changes. Correct detection is either
  dependency-based (are the competing `Set` ops' changes in a dependency chain,
  via DEPS?) or graph-truth (does the inode carry ≥2 alive `FOLDER` name-edges
  under the view filter?). NOTE: there is no existing helper that reads an
  inode's names from the graph — folder-edge traversal + name-byte reads from
  the change store would be new code.
- **Locus 2 — keep all competing names, in BOTH paths.** The deferred resolver
  (`desired_tree_paths`/`apply_deferred_tree_ops_in_txn`) and the insert-eager
  FileMove handler (`insert_change`) must both `put_tree` every competing name
  for the inode so materialize (TREE-driven) writes the file under each name.
  Manage the `REV_TREE` single-valued asymmetry (inode→one path).
- **Locus 3 — a non-marker `Name` honesty channel + clear lifecycle.**
  `StoredConflictKind::Name` already exists but is unused. `status` and
  `list_conflicts` currently drop any CONFLICTS entry unless the file carries an
  in-file `>>>>>>>` marker (`still_conflicted = first_conflict_marker_line(..)`),
  and `verify_working_copy` (doctor) enforces `markers == status_conflicted ==
  listed`. A filename conflict has no in-file marker, so all three need a
  `Name`-kind path whose "still conflicted" test is "the inode still has ≥2
  live names" (cleared when the user removes one name and records), and doctor
  must treat `Name` conflicts as honest without a marker.

**Decision:** implement as a focused arc (A11-a detection + keep-both across
resolver & insert; A11-b `Name` honesty channel + lifecycle; A11-c promote the
suite-28 xfail to a real assertion), each stage validated like the rename
stages — NOT as a single unvalidated change to the merge/honesty core. Both
names present is the canonical Pijul/Atomic representation of an unresolved
name conflict (cf. the existing `SolveNameConflict`/`UnsolveNameConflict`
GraphOps), so "materialize under every competing name + surface it" is the
target behavior.

---

## 7 · Proposed implementation order

| Phase | Work | Status |
|-------|------|--------|
| 1 | `CONFLICTS` table + write in insert/switch txn | **done** (ATOM::19) |
| 2 | `status` emits `Conflicted`; `record` refuses marker-laden files (with override) | **done** (ATOM::19) |
| 3 | `atomic conflicts` detail command; richer insert output | **done** (ATOM::20) |
| 4 | Output-stage invariant assertions (§6.2) | **done** (ATOM::18) |
| 5 | Property suite (§6.1) — `merge_property_tests.rs` | **done** (ATOM::21) |
| 6 | Matrix harness `28_merge_rubric.sh`: parameterized A×B cells, four uniform invariants | **done** (ATOM::22) |
| 7 | `atomic doctor check` verifier (§6.3) | **done** (ATOM::27) |
| 8 | (If needed) resolution-memory (`rerere`-style) keyed on conflict side hashes | pending, only if churn observed |
| 9 | **Fix A12** — surface a name conflict (both bodies) when two live inodes claim one path (§6.6) | **done** (ATOM::30) |
| 10 | **Fix A15** — record binary replace as a whole-file replace that deletes the base, so no residue leaks (§6.6) | **done** (ATOM::31) |

Phases 1–10 are complete, plus the ATOM::29 audit (§6.6, promoted A9/A14/B9/B12
to real assertions), B7 N-way (ATOM::32), and the staged rename effort
(ATOM::33–37, §6.7): renames now record as a FileMove reusing the inode
(Stages 1–2), cross-view insert applies the move including rename-vs-edit
(Stage 3, A10), and A10 is a real assertion in 28 (Stage 4). Suite 28 has ONE
tracked xfail: **A11** (rename-vs-rename → name conflict, silently
last-writer-wins today — needs graph name-conflict detection + a non-marker
honesty channel, §6.7). Still genuinely uncovered: token-level A5/A6, the B5
pull pathway, and unrecord-by-hash (B10). Phase 4's invariant assertions and
Phase 1–3's persistence were what made Phase 6's A4 duplication + honesty cells
pass — the old pre-fix binary still fails them.

**Status of the "pre-existing prepend/append linearization bug" (resolved,
ATOM::26).** Harness 27 case 1's persistent failure during this effort turned
out to be a TEST ARTIFACT, not a live bug: the merge output is byte-identical
to the expectation in current code (the Aug-7 release binary genuinely fails
it — the defect was real and was fixed before/during this effort). The
assertion compared via process substitution (`diff <(printf …)`), which
sandboxed environments block, and treated the resulting diff TOOL ERROR (exit
2) as a content mismatch — with an empty diff body as the only clue. Fixed:
`assert_file_equals` now diffs two real files and distinguishes exit 1
(mismatch) from exit >= 2 (tool error); suite 27 passes 13/13. The Phase-5
commutation property was widened accordingly
(`prop_boundary_and_interior_edits_commute`): prepend/append boundary edits
now commute and match the oracle across seeds. One nuance discovered while
widening: disjointness is an ANCHOR property, not a line property —
Prepend-vs-Replace(first) (and Append-vs-Replace(last)) share an insertion
anchor and CORRECTLY conflict (A4 class), so the generator pairs boundary
edits only with replacements away from their boundary. Note for other suites:
11/12 (git-parity) still use process substitution and will show the same
artifact in sandboxed runs; they are network-gated so they currently skip.

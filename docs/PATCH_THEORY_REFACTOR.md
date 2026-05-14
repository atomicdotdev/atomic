# Patch-Theory Refactor — Handoff Document

## Context

Atomic is a CRDT-based graph database for code edits, built on patch theory and DAGs. The original codebase was written with **filesystem thinking** — recording changes as whole-file diffs, physically deleting edges, materializing by writing files, etc. This session reframed the system to follow proper patch theory:

- Changes are **algebraic deltas** on a graph (vertex/edge operations).
- Views are **change-set filters** on a single canonical GRAPH (the "ambient graph" model).
- Edges are **never deleted** — a deletion is a new edge with the `DELETED` flag added alongside the original. Views with the deleting change in their filter see the vertex as dead; views without it still see it as alive.
- Materialization is a **pure read**: walk the graph with the view's filter, emit alive vertices in order.

The original bug that started this work:

> Inserting a draft view's changes into the shared `dev` view produces **content duplication** in the materialized file. Both the original and the inserted version of every line appear.

The work in this session diagnosed the bug as a filesystem-thinking implementation of a graph database. The graph was being mutated incorrectly during record, and views weren't being filtered properly during read.

---

## What Was Fixed

### 1. Additive-Only Edge Model (`atomic-core/src/apply/edge.rs`)

**Before**: `write_new_edge` called `del_edge_with_reverse` to physically remove the original `BLOCK` edge from the B-tree before adding the new `BLOCK|DELETED` edge.

**After**: The original edge stays in the B-tree forever. Only the new edge is added. The view's change filter + `is_vertex_alive` determine effective visibility.

```rust
// In write_new_edge:
//   add_edge_with_reverse(...);  // <-- only this
//   // del_edge_with_reverse was removed
```

`del_edge_with_reverse` is now `#[allow(dead_code)]` and never called.

### 2. Aliveness Check Honors Additive Edges (`atomic-core/src/output/alive/retrieve/classify.rs`)

**Before**: `is_vertex_alive` only looked at non-deleted parents (`include_deleted=false`).

**After**: Looks at ALL parents (including DELETED ones). A vertex with any `BLOCK|DELETED` parent is dead, regardless of whether the original `BLOCK` parent is still in the B-tree.

The filtered version in `options.rs` already had the correct logic — it checks whether the deletion's `introduced_by` is in the filter to decide whether the deletion "has happened" from this view's perspective.

### 3. Per-Line Vertex Granularity (`atomic-core/src/record/workflow/globalize/`)

**Before**: A file was stored as a single content vertex. Every edit produced a whole-file replacement.

**After**: Each line is its own vertex, chained via BLOCK edges. Edits target specific line vertices.

Changes:
- `vertex.rs`: Added `create_content_vertices_per_line` that splits content on newlines and produces a chain of `Insertion`s.
- `pipeline.rs` FileAdd path: First line goes into `GraphOp::FileAdd.contents`, remaining lines emitted as standalone `GraphOp::Edit` ops chained via self-reference.
- `hunk.rs` `globalize_replace`: Uses `built.deleted_lines` to target only the specific vertices being replaced. Per-line vertices for the replacement content.
- `hunk.rs` `globalize_delete`: Same — targets only the deleted lines, not the whole file.
- `hunk.rs` `classify_insert`: Added `Middle` variant for inserts at arbitrary positions (not just Prepend/Append).
- `hunk.rs` `collect_sorted_content_vertices`: Walks the graph in BLOCK-edge order (was sorting by `start` position, which is wrong with multiple changes).

### 4. Removed Nuclear Hunk Consolidation (`atomic-core/src/record/workflow/record/mod.rs`)

**Before**: When any hunk was a `Replace` or `Delete`, or an `Insert` in the middle of a file, ALL hunks were consolidated into a single whole-file `Replace`. This was the "fall-back to filesystem-style" path.

**After**: Each hunk is processed independently using proper patch theory. The consolidation block was removed entirely.

### 5. Minimal Diff (No User-Friendly Coercion) (`atomic-core/src/diff/mod.rs`)

**Before**: `diff()` post-processed ops via `rewrite_positional_shifts` to convert "positional shifts" into Replace ops (user-friendly display). And `common_affixes` reduced the suffix when one side was empty to "force Replace detection."

**After**: Added `diff_raw()` and `common_affixes_strict()`. The record path uses `diff_raw` so pure insertions stay as `Insert` ops and aren't mangled into `Replace + Delete` pairs.

### 6. Always Filter (No Fast Path for Shared Roots) (`atomic-repository/src/repository/content.rs`)

**Before**: `get_file_content` had a "fast path" for shared root views that skipped the filter entirely (assuming they could see all of GRAPH).

**After**: Always use the change filter. Drafts also write to GRAPH, so shared roots can no longer see "everything" — they only see their own VIEW_CHANGES.

### 7. Walk Through Dead Vertices (`atomic-core/src/output/alive/retrieve/mod.rs`)

**New**: Added `walk_through_dead` helper. When the traversal hits a dead vertex (or a dead edge to an alive vertex), it walks through the dead chain to find live successors. This is needed because the additive model has no PSEUDO reconnection edges — when V_X is dead between V_predecessor and V_successor, the traversal must skip V_X but still reach V_successor.

The helper also checks `has_alive_alt_parent` to avoid duplicating successors that are reachable through other alive paths.

### 8. Fork Conflict Markers (`atomic-core/src/output/repo/content.rs` + `merge/resolved.rs`)

**New**: When the `SemanticMergeEngine` can't auto-merge a fork conflict, the children are now wrapped in `>>>>>>>` / `=======` / `<<<<<<<` markers (like git). Previously they were silently concatenated.

Added `ResolvedConflicts::insert_unresolved_fork` / `unresolved_forks` / `fork_group_for` and wired them into `output_graph_content_resolved`.

### 9. Cross-View Insert Cleaned Up (`atomic-repository/src/repository/insert.rs`)

The `insert_from_view` method went through a filesystem-style detour (snapshot content, 3-way merge, auto-record) which was reverted. It's now a pure metadata operation:

```rust
// Add change_id to VIEW_CHANGES[target_view]
// That's it. The graph is already current.
```

---

## Test State

After the work in this session:

| Suite | Passing | Failing | Notes |
|-------|---------|---------|-------|
| `atomic-core` (lib) | 3338 | 0 | All pass |
| `atomic-repository` (lib, excluding cross_view_merge) | 756 | 3 | 3 regressions explained below |
| `atomic-repository` cross_view_merge_tests | 4 | 4 | Foundational tests pass, edge cases fail |
| Harness 17 (`tests/harness/17_cross_view_merge.sh`) | 58/72 | 14 | (was 0/72 at start) |

**Three pre-existing tests now regress** (all in `repository/tests/integration_tests.rs`):

- `test_modify_first_line_content_retrieval`
- `test_status_clean_after_view_switch_with_sibling_changes`
- `test_switch_view_shows_view_content`

These regressions stem from the same root cause as the remaining cross-view failures (see below).

**Four cross-view merge tests still fail:**

- `test_cross_view_merge_non_overlapping_edits`
- `test_cross_view_merge_reverse_direction`
- `test_cross_view_merge_sequential_draft_changes`
- `test_cross_view_merge_two_files_mixed_collision`

---

## Where We Need To Go

### The Remaining Bug

The graph is now structurally correct. The remaining failures all share a single root cause:

**A vertex with multiple alive incoming edges (from different changes) gets its content emitted multiple times during output.**

Specifically: when change C2 modifies content recorded by C1, and a view's filter contains both C1 and C2, the graph correctly has:

- `V[line2]` alive with TWO incoming alive `BLOCK` parent edges:
  - From `V[line1_old]` (introduced by C1, never deleted as an edge)
  - From `V[line1_new]` (introduced by C2 — C2 wired its replacement to point at V[line2] as a successor)

Both are alive. `is_vertex_alive(V[line2])` correctly returns `true`. But the output walker visits V[line2] **once per alive parent**, emitting its content twice.

This is **filesystem thinking at the output layer**: it's iterating edges and emitting content per-edge, instead of asking "what's the linear sequence of leaves in this view?"

### The Right Fix

The output layer should use the **semantic CRDT layer** (Trunk → Branch → Leaf) for sequencing, not the byte-range graph directly.

#### Current Architecture

```
Materialize call
    └─→ retrieve_graph (walks byte-range vertices, follows BLOCK edges)
        └─→ compute_order (Tarjan SCCs on the byte-range graph)
            └─→ resolve_conflicts_semantically (calls SemanticMergeEngine per SCC)
                └─→ output_graph_content_resolved (emits vertices in SCC order)
```

The current pipeline produces byte-level vertices and tries to linearize them. When a vertex has multiple alive parents, the SCC analysis groups them together, and the output emits the byte content for each path.

#### Proposed Architecture

The CRDT semantic layer (already implemented in `atomic-core/src/crdt/` and `atomic-core/src/merge/`) tracks lines and tokens as `Branch` and `Leaf` nodes. Each branch has a stable ID (`(change_id, branch_idx)`). When two changes both produce the same logical line (e.g. via shared insertion context), the CRDT layer recognizes them as a single semantic line — not two competing vertices.

The fix is to make `output_file_with_filter` ask the CRDT layer:

```
For this file, in this view's filter, what is the canonical
sequence of (line, content) pairs?
```

The CRDT layer would:
1. Walk `crdt_branches` filtered by the view's change set.
2. For each alive branch, return its content (assembled from alive leaves).
3. Skip branches that are deleted (have a superseding delete op in the view).

This sidesteps the byte-range graph entirely for the *output* step. The byte-range graph remains the source of truth for storage; the CRDT layer is the source of truth for ordering and rendering.

### Concrete Task Breakdown

#### Task 1: Output via CRDT Layer

**File**: `atomic-core/src/output/repo/file.rs` (the `output_file_with_filter` function)

**Change**: Before falling back to `retrieve_graph` + SCC + write, check if the file has CRDT data:

```rust
if file_has_crdt_data(txn, inode) {
    // Walk crdt_branches for this file in this view's filter.
    // For each alive branch, emit its content.
    return output_file_via_crdt(txn, inode, change_filter, writer);
}
// Fallback to byte-range graph walk.
```

**Reference**:
- CRDT tables: `atomic-core/src/pristine/tables.rs` (`crdt_trunks`, `crdt_branches`, `crdt_leaves`)
- CRDT types: `atomic-core/src/crdt/mod.rs` (`Trunk`, `Branch`, `Leaf`)
- Existing query helpers: search for `iter_branches_for_trunk`, `get_leaf`, etc.

The hard part is the **ordering** of branches. Branches don't have a global order — each is an insertion with a predecessor/successor context. The right approach is probably to walk the branch DAG starting from the trunk, following alive branches in their preferred order.

#### Task 2: Verify CRDT Data Is Built During Record

**File**: `atomic-repository/src/repository/record.rs` (the `record` method)

**Check**: When per-line vertices are created (via `create_content_vertices_per_line`), is corresponding CRDT data (`Trunk`/`Branch`/`Leaf` ops) also being produced?

Currently, the record workflow has a CRDT subsystem (`atomic-core/src/record/workflow/crdt/`) that produces `FileOps`. These get serialized into the change file and applied to the CRDT tables on insert (`atomic_core::apply::apply_file_ops`).

Per-line vertex creation (recently added) may or may not be producing matching CRDT ops. If not, the CRDT tables are out of sync with the per-line graph — Task 1 would fail because the CRDT layer wouldn't know about the new lines.

#### Task 3: Update Tests for Markered Output

Several test failures (in cross_view_merge_tests) expect single-copy output when there's a genuine fork (e.g. both sides modify the same line). The CRDT-based output should produce conflict markers in that case. The tests need to be updated to either:

- Accept conflict markers as valid output (and assert the markers are correctly placed), or
- Specifically assert "no markers expected because edits don't overlap semantically"

The tests where both sides edit DIFFERENT lines should pass cleanly (no markers). The tests where both sides edit the SAME line should produce a marked conflict.

#### Task 4: Re-evaluate the Three Regressed Tests

The three regressed tests in `integration_tests.rs` should pass once the output layer uses CRDT properly. They're failing because:

- `test_modify_first_line_content_retrieval`: V[line2] is being emitted twice (once for each alive parent). The CRDT branch for "Line 2 - unchanged" exists exactly once in the trunk; the CRDT output would emit it once.

- `test_status_clean_after_view_switch_with_sibling_changes`: probably similar — status compares disk content vs graph content, and if graph content is duplicated, status reports phantom modifications.

- `test_switch_view_shows_view_content`: switching views writes the materialized file to disk. If materialization produces duplicated content, the disk content is wrong.

All three should resolve once the output is CRDT-driven.

#### Task 5: Re-test the Original User Bug

The original bug was:
> Inserting a draft view into dev produces content duplication.

The harness test `tests/harness/17_cross_view_merge.sh` should go from 58/72 to 72/72 once Task 1 lands. The harness tests use the CLI directly — they exercise the full record → insert → materialize pipeline.

---

## Architectural Lessons

For the new context window — important things to keep in mind:

1. **Records are graph operations, not file writes.** When a user types new bytes, the record path's job is to compute the algebraic delta on the graph (insert vertex here, delete vertex there). Don't think of it as "snapshot the file, store the bytes."

2. **Views are filters, not branches.** A view is a set of change IDs. The same graph is "filtered" by each view's set. There are no per-view graph copies, no snapshots, no merge bases. The parent chain is evaluated at read time — propagation is automatic.

3. **Inserts are metadata.** `insert_from_view` adds a change ID to `VIEW_CHANGES[target]`. That's it. The graph is already current because the change was already applied when it was recorded on its source view.

4. **Edges are additive forever.** Never call `del_graph` on an alive edge. Mark it deleted by adding a new edge with the DELETED flag. The view filter determines which is effective.

5. **Materialize is a pure read.** Walk the graph with the view's filter. Emit alive content. No diffing, no comparison with disk, no recording back to the graph.

6. **Use the semantic layer for sequencing.** When you need to render content for humans, ask the CRDT (Trunk/Branch/Leaf) layer, not the byte-range graph. The byte-range graph is for storage and merge logic; the CRDT layer is for ordering and display.

---

## File Map of Changes

Key files modified in this session:

```
atomic-core/src/apply/edge.rs                              # Additive-only edges
atomic-core/src/output/alive/retrieve/classify.rs          # is_vertex_alive honors superseding deletes
atomic-core/src/output/alive/retrieve/mod.rs               # walk_through_dead + dest_alive checks
atomic-core/src/output/repo/content.rs                     # Unresolved fork emission
atomic-core/src/output/repo/fork.rs                        # Fork detection refinement
atomic-core/src/merge/resolved.rs                          # Track unresolved forks
atomic-core/src/diff/mod.rs                                # diff_raw + common_affixes_strict
atomic-core/src/record/workflow/compare.rs                 # Use diff_raw for record path
atomic-core/src/record/workflow/globalize/hunk.rs          # Targeted globalize_replace/delete, Middle insert
atomic-core/src/record/workflow/globalize/pipeline.rs      # Per-line FileAdd
atomic-core/src/record/workflow/globalize/vertex.rs        # create_content_vertices_per_line
atomic-core/src/record/workflow/globalize/helpers.rs       # split_into_lines
atomic-core/src/record/workflow/globalize/mod.rs           # Export Local, split_into_lines
atomic-core/src/record/workflow/record/mod.rs              # Removed nuclear consolidation

atomic-repository/src/repository/content.rs                # Always filter (no fast path)
atomic-repository/src/repository/insert.rs                 # Reverted to pure metadata
atomic-repository/src/apply/cross_view.rs                  # Original CrossViewInsertOutcome
atomic-repository/src/repository/tests/cross_view_merge_tests.rs  # NEW test suite
atomic-repository/src/repository/tests/mod.rs              # Register new tests
atomic-repository/src/repository/tests/edit_tests.rs       # Updated assertion for per-line vertices

tests/harness/17_cross_view_merge.sh                       # NEW harness suite
```

---

## How To Continue

In the new context window, suggested opening prompt:

> I'm continuing work on the Atomic VCS patch-theory refactor.
> Read `atomic/docs/PATCH_THEORY_REFACTOR.md` for context.
> The remaining work is in the "Where We Need To Go" section.
> Start with Task 1: make the output layer use the CRDT semantic layer.

The fresh context should help approach the remaining bug with clear architectural thinking, rather than the accumulated filesystem-thinking patterns that have been driving us into corners.

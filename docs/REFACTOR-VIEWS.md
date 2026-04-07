# REFACTOR-VIEWS.md — Ambient Graph + View Filters

> **Status**: In Progress — Phase 2 Complete  
> **Created**: 2025-07-13  
> **Last Updated**: 2025-07-13  
> **Tracking Issue**: N/A  

## Table of Contents

- [Motivation](#motivation)
- [Vocabulary](#vocabulary)
- [Architectural Change](#architectural-change)
- [Complete Rename Mapping](#complete-rename-mapping)
- [Phase 1: Core Types, Traits, Tables, Errors](#phase-1-core-types-traits-tables-errors)
- [Phase 2: Eliminate ApplyTarget — All Edges to GRAPH](#phase-2-eliminate-applytarget--all-edges-to-graph)
- [Phase 3: Materialize Rename](#phase-3-materialize-rename)
- [Phase 4: Repository Layer](#phase-4-repository-layer)
- [Phase 4.5: Agent Crate](#phase-45-agent-crate)
- [Phase 4.6: Remote Client Crate](#phase-46-remote-client-crate)
- [Phase 5: CLI Layer](#phase-5-cli-layer)
- [Phase 6: Test Harness](#phase-6-test-harness)
- [Phase 7: Rust Test Suite](#phase-7-rust-test-suite)
- [Phase 8: Documentation and AGENTS.md](#phase-8-documentation-and-agentsmd)
- [Deleted Code](#deleted-code)
- [Risk Register](#risk-register)
- [Verification Strategy](#verification-strategy)

---

## Motivation

The current two-tier edge storage model (`GRAPH` for shared stacks, `STACK_GRAPH`
for local stacks) creates **isolation between agents that is structurally identical
to Git branches**. An agent working on a local stack writes edges to
`STACK_GRAPH[stack_id]`, which is invisible to every other agent until an explicit
`apply from-stack` promotes those edges into the canonical `GRAPH`. This is a
snapshot-and-merge workflow — the opposite of real-time collaboration.

The original design intent was for stacks to be **views** on a single, always-current
graph. This refactoring makes that intent real:

1. **All edges always go to the canonical `GRAPH`** — no more `STACK_GRAPH`.
2. **Views are change-set filters**, not edge-storage partitions.
3. **"Insert" replaces "apply"** — an O(1) metadata operation that adds change
   references to a view's filter, not an expensive edge-copying merge.
4. **Real-time collaboration** — agents can see each other's work immediately
   because all edges live in the same graph.

## Vocabulary

Three terms, consistent database metaphor throughout:

| Term | Definition | Replaces |
|------|-----------|----------|
| **View** | A filtered projection over the graph. Defined by its change set in `VIEW_CHANGES`. Analogous to a database view — a logical lens, not a physical copy. | Stack |
| **Insert** | Add change references from one view into another. An O(1) metadata operation — adds entries to `VIEW_CHANGES`, no edge copying. Analogous to a database `INSERT INTO`. | Apply (user-facing) |
| **Materialize** | Write the graph state to disk as actual files. Takes a logical view and produces a physical working copy. Analogous to a materialized view in a database. | `output_working_copy`, `output_repository` |

### CLI Vocabulary

```text
# View lifecycle (was: stack)
atomic view create feature-auth --from dev
atomic view list
atomic view switch dev
atomic view delete feature-auth
atomic view info dev

# Insert changes between views (was: apply)
atomic insert from-view feature-auth --to-view dev
atomic insert ABC123 --view dev
atomic insert preview feature-auth --to-view dev
atomic insert pick ABC123 DEF456 --to-view dev

# Real-time collaboration (future)
atomic view watch feature-auth
atomic view unwatch feature-auth

# Unchanged
atomic record -m "Add auth module"
atomic status
atomic diff
atomic log
```

### Internal Vocabulary

| Layer | Term | Meaning |
|-------|------|---------|
| CLI | `view`, `insert` | User-facing commands |
| Repository | `insert_change()`, `insert_from_view()` | Public API matching CLI |
| Repository | `write_recorded()` | Internal: record workflow writes to graph |
| Core engine | `write_change_to_graph()`, `write_new_vertex()` | Graph mutation operations |
| Core engine | `materialize_view()`, `materialize_prefix()` | Graph → disk |
| Core engine | `ViewState`, `ViewScope`, `ViewTxnT` | View types and traits |
| CRDT | `apply_branch_op()`, `apply_leaf_op()` | **Unchanged** — genuinely applying ops to a CRDT |

---

## Architectural Change

### Before: Two-Tier Edge Storage (Isolated)

```text
Agent A records on feature-auth (Local):
  → edges written to STACK_GRAPH[feature-auth]    ← INVISIBLE to Agent B

Agent B records on feature-payments (Local):
  → edges written to STACK_GRAPH[feature-payments] ← INVISIBLE to Agent A

To make visible:
  atomic apply from-stack feature-auth --to-stack dev
  → re-creates ALL edges in GRAPH                  ← snapshot-and-merge!
```

### After: Ambient Graph + View Filters (Collaborative)

```text
Agent A records on feature-auth:
  → edges written to GRAPH immediately             ← VISIBLE to everyone
  → change hash added to VIEW_CHANGES[feature-auth]

Agent B records on feature-payments:
  → edges written to GRAPH immediately             ← VISIBLE to everyone
  → change hash added to VIEW_CHANGES[feature-payments]

Agent B wants to see Agent A's work:
  → atomic view watch feature-auth
  → view = own changes ∪ feature-auth changes ∪ shared base
  → NO insert step. Just expand the filter.

"Insert" to dev:
  → add change hash to VIEW_CHANGES[dev]           ← O(1) metadata op
  → NO edge copying. Edges are already in GRAPH.
```

### What Changes vs What Stays

**Stays the same (most of the codebase):**

- The graph model (vertices, edges, `GraphNode`, `SerializedGraphEdge`)
- The `GRAPH` table — becomes the *only* edge store
- The `INODE_GRAPH` secondary index — still needed for file-local performance
- The `change_filter` / `RetrieveOptions` system — this IS the view mechanism
- The `introduced_by` field on every edge — this is how filters work
- The diff, record, and CRDT pipelines
- `TREE`, `INODES`, `DEPS`, `REV_DEPS`, `STATES`, `TAGS` — unchanged

**What is eliminated:**

- `STACK_GRAPH` table — all edges go to `GRAPH`
- `ApplyTarget` enum — always `Global`, so the enum is unnecessary
- `OverlayTxn` — views use change filters on the canonical `GRAPH`
- `should_apply_hunks` logic — always write hunks (they go to `GRAPH`)
- `put_stack_graph` / `del_stack_graph` / `del_stack_graph_prefix` — dead code
- `resolve_overlay_chain` stack-graph walking — replaced by view-chain filter building
- `iter_stack_graph_adjacent` — dead code

### ViewScope Replaces StackKind

The distinction is no longer about *where edges are stored* but about
*visibility semantics* and *lifecycle*:

```rust
pub enum ViewScope {
    /// Personal workspace. Changes are recorded to GRAPH immediately but
    /// only visible through this view's filter (and subscribers).
    /// Can be deleted freely — just removes VIEW_CHANGES entries.
    /// Edges remain in GRAPH for GC later.
    Draft,

    /// Collaborative view visible to all (dev, release, main).
    /// Changes inserted here become part of the base filter
    /// that all child views inherit.
    /// Deletion is restricted (permanent history).
    Shared,
}
```

### How Views Compute Their Filter

A view's effective change set is the union of its own changes and all ancestor
changes. This replaces the `OverlayTxn` stack-chain walk:

```rust
fn compute_view_filter(txn: &ReadTxn, view: &ViewState) -> HashSet<NodeId> {
    let mut filter = HashSet::new();

    // 1. This view's own changes
    filter.extend(collect_view_change_ids(txn, view));

    // 2. Walk parent chain, collecting all ancestor changes
    let mut cursor = view.parent;
    while let Some(parent_id) = cursor {
        if let Some(parent) = txn.get_view_by_id(parent_id) {
            filter.extend(collect_view_change_ids(txn, &parent));
            cursor = parent.parent;
        } else {
            break;
        }
    }

    // 3. Expand dependency closures
    expand_dependency_closure(txn, &mut filter);

    filter
}
```

This is nearly identical to what `get_file_content_via_overlay` in `content.rs`
already does at lines 88-148. The difference is it no longer needs `OverlayTxn`
for edge visibility — the canonical `GRAPH` has all edges, and the filter
determines which ones are "alive" in this view.

### The Insert Operation (Was: Apply)

Cross-view insertion becomes a metadata-only operation:

```rust
pub fn insert_from_view(
    &self,
    from_view: &str,
    to_view: &str,
) -> Result<InsertOutcome, RepositoryError> {
    let mut txn = self.pristine.write_txn()?;
    let source = txn.get_view(from_view)?;
    let mut target = txn.get_view(to_view)?;

    let source_changes = collect_view_change_ids(&txn, &source);
    let target_changes = collect_view_change_ids(&txn, &target);
    let missing: Vec<_> = source_changes.difference(&target_changes).collect();

    for &change_id in &missing {
        let hash = txn.get_external(change_id)?;
        txn.put_change(&mut target, change_id, &hash)?;  // O(1) per change
    }

    txn.update_view(&target)?;
    txn.commit()?;
    // That's it. No edge copying. No hunk re-application.
}
```

### View Deletion and Garbage Collection

Deleting a Draft view removes its `VIEW_CHANGES` entries but leaves edges in
`GRAPH`. Orphaned edges (whose `introduced_by` change is not in ANY view's
change set) are cleaned up by periodic GC:

```rust
fn gc_orphaned_edges(txn: &mut WriteTxn) -> Result<usize> {
    let all_referenced = collect_all_view_change_ids(txn);
    let mut removed = 0;
    for (vertex, edge) in txn.iter_graph() {
        if !all_referenced.contains(&edge.introduced_by()) {
            txn.del_graph(vertex, edge)?;
            removed += 1;
        }
    }
    Ok(removed)
}
```

---

## Complete Rename Mapping

### Types and Structs

| Current | Proposed | File |
|---------|----------|------|
| `StackKind` | `ViewScope` | `atomic-core/src/pristine/traits.rs` |
| `StackKind::Local` | `ViewScope::Draft` | `atomic-core/src/pristine/traits.rs` |
| `StackKind::Shared` | `ViewScope::Shared` | `atomic-core/src/pristine/traits.rs` |
| `StackState` | `ViewState` | `atomic-core/src/pristine/traits.rs` |
| `StackTxnT` | `ViewTxnT` | `atomic-core/src/pristine/traits.rs` |
| `StackInfo` | `ViewInfo` | `atomic-repository/src/repository/mod.rs` |
| `ApplyTarget` | **ELIMINATED** | `atomic-core/src/apply/mod.rs` |
| `OverlayTxn` | **ELIMINATED** | `atomic-core/src/pristine/overlay.rs` |
| `ApplyOptions` | `InsertOptions` | `atomic-repository/src/apply.rs` |
| `ApplyOutcome` | `InsertOutcome` | `atomic-repository/src/apply.rs` |
| `ApplyStats` | `InsertStats` | `atomic-repository/src/apply.rs` |
| `ApplyError` | `InsertError` | `atomic-repository/src/apply.rs` |
| `ApplyResult` | `InsertResult` | `atomic-repository/src/apply.rs` |
| `CrossStackApplyOptions` | `CrossViewInsertOptions` | `atomic-repository/src/apply.rs` |
| `CrossStackApplyOutcome` | `CrossViewInsertOutcome` | `atomic-repository/src/apply.rs` |
| `RepositoryOutputOptions` | `MaterializeOptions` | `atomic-core/src/output/repo/` |
| `RepositoryOutputResult` | `MaterializeResult` | `atomic-core/src/output/repo/` |
| `RepositoryOutputError` | `MaterializeError` | `atomic-core/src/output/repo/` |

### Trait Methods

| Current | Proposed | Trait |
|---------|----------|-------|
| `get_stack()` | `get_view()` | `ViewTxnT` |
| `get_stack_by_id()` | `get_view_by_id()` | `ViewTxnT` |
| `list_stacks()` | `list_views()` | `ViewTxnT` |
| `stack_state()` | `view_state()` | `ViewTxnT` |
| `get_children_stacks()` | `get_children_views()` | `ViewTxnT` |
| `resolve_overlay_chain()` | `resolve_view_chain()` | `ViewTxnT` |
| `iter_stack_graph_adjacent()` | **ELIMINATED** | — |
| `iter_stack_graph_vertices_for_change()` | **ELIMINATED** | — |
| `create_stack()` | `create_view()` | `MutTxnT` |
| `open_or_create_stack()` | `open_or_create_view()` | `MutTxnT` |
| `update_stack()` | `update_view()` | `MutTxnT` |
| `del_stack()` | `del_view()` | `MutTxnT` |
| `put_stack_graph()` | **ELIMINATED** | — |
| `del_stack_graph()` | **ELIMINATED** | — |
| `del_stack_graph_prefix()` | **ELIMINATED** | — |

### Repository Methods

| Current | Proposed | File |
|---------|----------|------|
| `current_stack()` | `current_view()` | `repository/mod.rs` |
| `set_current_stack()` | `set_current_view()` | `repository/mod.rs` |
| `switch_stack()` | `switch_view()` | `repository/mod.rs` |
| `create_stack()` | `create_view()` | `repository/mod.rs` |
| `create_stack_from()` | `create_view_from()` | `repository/mod.rs` |
| `list_stacks()` | `list_views()` | `repository/mod.rs` |
| `stack_exists()` | `view_exists()` | `repository/mod.rs` |
| `delete_stack()` | `delete_view()` | `repository/mod.rs` |
| `get_stack_info()` | `get_view_info()` | `repository/mod.rs` |
| `read_current_stack()` | `read_current_view()` | `repository/mod.rs` |
| `write_current_stack()` | `write_current_view()` | `repository/mod.rs` |
| `nearest_shared_ancestor()` | `nearest_shared_ancestor()` | unchanged |
| `collect_stack_change_ids()` | `collect_view_change_ids()` | `repository/mod.rs` |
| `apply_change()` | `insert_change()` | `repository/apply.rs` → `repository/insert.rs` |
| `apply_change_rec()` | `insert_change_rec()` | `repository/apply.rs` → `repository/insert.rs` |
| `apply_recorded()` | `write_recorded()` | `repository/apply.rs` → `repository/insert.rs` |
| `apply_from_stack()` | `insert_from_view()` | `repository/apply.rs` → `repository/insert.rs` |
| `apply_tag_to_stack()` | `insert_tag_to_view()` | `repository/apply.rs` → `repository/insert.rs` |
| `cherry_pick()` | `cherry_pick()` | unchanged (it's a user concept) |
| `output_working_copy()` | `materialize()` | `repository/mod.rs` |
| `output_working_copy_prefix()` | `materialize_prefix()` | `repository/mod.rs` |
| `get_file_content_via_overlay()` | `get_file_content()` | `repository/content.rs` |
| `get_file_content_on_stack()` | `get_file_content_on_view()` | `repository/content.rs` |
| `diff_stacks()` | `diff_views()` | `repository/content.rs` |

### Core Engine Functions

| Current | Proposed | File |
|---------|----------|------|
| `apply_change_to_graph()` | `write_change_to_graph()` | `atomic-repository/src/apply.rs` |
| `apply_hunk()` | `write_hunk()` | `atomic-repository/src/apply.rs` |
| `apply_new_vertex()` | `write_new_vertex()` | `atomic-core/src/apply/insertion.rs` |
| `apply_edge_map()` | `write_edge_map()` | `atomic-core/src/apply/edge.rs` |
| `apply_new_edge()` | `write_new_edge()` | `atomic-core/src/apply/edge.rs` |
| `apply_file_ops()` | `write_file_ops()` | `atomic-core/src/apply/file_ops.rs` |
| `add_edge_with_reverse()` | `add_edge_with_reverse()` | unchanged (edge.rs, insertion.rs) |
| `del_edge_with_reverse()` | `del_edge_with_reverse()` | unchanged (edge.rs) |
| `resolve_vertex_for_target()` | **ELIMINATED** | was target-aware |
| `resolve_context_vertex_for_target()` | **ELIMINATED** | was target-aware |
| `output_repository()` | `materialize_view()` | `atomic-core/src/output/repo/repository.rs` |
| `output_repository_prefix()` | `materialize_prefix()` | `atomic-core/src/output/repo/repository.rs` |

### Tables (redb)

| Current | Proposed | Notes |
|---------|----------|-------|
| `STACKS` (`"stacks"`) | `VIEWS` (`"views"`) | View metadata |
| `STACK_CHANGES` (`"stack_changes"`) | `VIEW_CHANGES` (`"view_changes"`) | Change log per view |
| `REV_STACK_CHANGES` (`"rev_stack_changes"`) | `REV_VIEW_CHANGES` (`"rev_view_changes"`) | Reverse change log |
| `STACK_GRAPH` (`"stack_graph"`) | **ELIMINATED** | All edges go to `GRAPH` |

### Error Variants

| Current | Proposed | Files |
|---------|----------|-------|
| `StackNotFound` | `ViewNotFound` | `pristine/error.rs`, `repository/error.rs`, `cli/error.rs` |
| `StackAlreadyExists` | `ViewAlreadyExists` | `pristine/error.rs` |
| `CannotDeleteSharedStack` | `CannotDeleteSharedView` | `pristine/error.rs` |
| `StackHasChildren` | `ViewHasChildren` | `pristine/error.rs` |
| `StackCycleDetected` | `ViewCycleDetected` | `pristine/error.rs` |
| `CannotDeleteCurrentStack` | `CannotDeleteCurrentView` | `cli/error.rs` |
| `StackAlreadyExists` | `ViewAlreadyExists` | `cli/error.rs` |

### Files on Disk

| Current | Proposed |
|---------|----------|
| `.atomic/current_stack` | `.atomic/current_view` |
| `.atomic/workspaces/<name>/` | `.atomic/workspaces/<name>/` (unchanged — these are artifact shelves, not stack metadata) |

### File Renames

| Current | Proposed |
|---------|----------|
| `atomic-cli/src/commands/stack/` | `atomic-cli/src/commands/view/` |
| `atomic-cli/src/commands/apply.rs` | `atomic-cli/src/commands/insert.rs` |
| `atomic-repository/src/repository/apply.rs` | `atomic-repository/src/repository/insert.rs` |
| `atomic-repository/src/apply.rs` | `atomic-repository/src/insert.rs` |
| `atomic-core/tests/stack_graph_test.rs` | `atomic-core/tests/view_test.rs` (rewrite) |

Note: `atomic-core/src/apply/` directory is **not** renamed. These are internal
graph write operations, not the user-facing "apply" concept. They are renamed
from `apply_*` to `write_*` at the function level but the directory stays as
a module organizational boundary.

---

## Phase 1: Core Types, Traits, Tables, Errors ✅ COMPLETE

**Goal**: Rename all foundational types that everything else depends on.  
**Depends on**: Nothing — this is the foundation.  
**Risk**: High — every downstream file imports these types.  
**Result**: `cargo test -p atomic-core` — 442 passed, 0 failed, 179 ignored.

### Files to Modify

| File | Changes |
|------|---------|
| `atomic-core/src/pristine/traits.rs` | `StackKind` → `ViewScope` (with `Draft`/`Shared`), `StackState` → `ViewState`, `StackTxnT` → `ViewTxnT`, all method renames, ~400 lines |
| `atomic-core/src/pristine/tables.rs` | `STACKS` → `VIEWS`, `STACK_CHANGES` → `VIEW_CHANGES`, `REV_STACK_CHANGES` → `REV_VIEW_CHANGES`, eliminate `STACK_GRAPH`, ~50 lines |
| `atomic-core/src/pristine/error.rs` | All error variant renames, ~40 lines |
| `atomic-core/src/pristine/mod.rs` | Update re-exports, ~20 lines |
| `atomic-core/src/pristine/txn/read.rs` | `impl ViewTxnT for ReadTxn`, eliminate `STACK_GRAPH` reads, ~150 lines |
| `atomic-core/src/pristine/txn/write/stack.rs` | Rename to `view.rs`, `impl ViewTxnT for WriteTxn`, ~100 lines |
| `atomic-core/src/pristine/txn/write/mod.rs` | `del_stack` → `del_view` (remove `STACK_GRAPH` cascade), `create_stack` → `create_view`, ~200 lines |
| `atomic-core/src/pristine/txn/helpers.rs` | Any stack helper renames, ~30 lines |
| `atomic-core/src/pristine/txn/pristine.rs` | Table opening — remove `STACK_GRAPH`, rename table constants, ~20 lines |
| `atomic-core/src/lib.rs` | Update re-exports, ~10 lines |

### Checklist

- [x] Rename `StackKind` → `ViewScope` with variants `Draft` (was `Local`) and `Shared`
- [x] Rename `StackState` → `ViewState` (update all fields, impls, tests)
- [x] Rename `StackTxnT` → `ViewTxnT` (update all method names)
- [x] Rename all `MutTxnT` stack methods → view methods
- [x] Update table constants: `STACKS` → `VIEWS`, `STACK_CHANGES` → `VIEW_CHANGES`, `REV_STACK_CHANGES` → `REV_VIEW_CHANGES`
- [x] Remove `STACK_GRAPH` table definition and all references
- [x] Rename error variants in `pristine/error.rs`
- [x] Update `ReadTxn` impl — remove all `STACK_GRAPH` read methods
- [x] Update `WriteTxn` impl — remove all `STACK_GRAPH` write methods
- [x] Update `del_view()` — remove `STACK_GRAPH` cascade, just delete `VIEW_CHANGES` entries
- [x] Update table opening in `pristine.rs` — remove `STACK_GRAPH`
- [x] Update re-exports in `mod.rs` and `lib.rs`
- [x] Run `cargo check -p atomic-core` — passes clean
- [x] Fix downstream import breakages in `apply/` and `record/` (StackState→ViewState, StackTxnT→ViewTxnT, overlay imports)
- [x] Fix `PristineError::StackNotFound` → `ViewNotFound` in 17 test-code references across 7 files
- [x] Run `cargo test -p atomic-core` — 442 passed, 0 failed

### Deleted

- [x] `atomic-core/src/pristine/overlay.rs` — entire file (974 lines, OverlayTxn eliminated)
- [x] `atomic-core/tests/stack_graph_test.rs` — entire file (2,108 lines, tested STACK_GRAPH behavior that no longer exists; replacement `view_test.rs` tracked in Phase 7)

---

## Phase 2: Eliminate ApplyTarget — All Edges to GRAPH ✅ COMPLETE

**Goal**: Remove the two-tier edge routing. All edges always go to `GRAPH` + `INODE_GRAPH`.  
**Depends on**: Phase 1 (table constants, trait renames).  
**Result**: `cargo test -p atomic-core` — 442 passed, 0 failed, 178 ignored. Zero `ApplyTarget` references remain.

### Files to Modify

| File | Changes |
|------|---------|
| `atomic-core/src/apply/mod.rs` | Remove `ApplyTarget` enum, update re-exports, ~60 lines |
| `atomic-core/src/apply/edge.rs` | `apply_edge_map` → `write_edge_map`, `apply_new_edge` → `write_new_edge`, `add_edge_with_reverse` — remove `Local` branch (always GRAPH + INODE_GRAPH), `del_edge_with_reverse` — same, ~150 lines |
| `atomic-core/src/apply/insertion.rs` | `apply_new_vertex` → `write_new_vertex`, `add_edge_with_reverse` — remove `Local` branch, ~100 lines |
| `atomic-core/src/apply/position.rs` | Remove `resolve_context_vertex_for_target`, `resolve_vertex_for_target`, ~30 lines |
| `atomic-core/src/apply/file_ops.rs` | `apply_file_ops` → `write_file_ops`, ~20 lines |
| `atomic-core/src/apply/change.rs` | Update references to `ViewState` (was `StackState`), ~30 lines |
| `atomic-core/src/apply/workspace.rs` | Update doc comments, ~10 lines |
| `atomic-core/src/apply/error.rs` | Update if any `ApplyTarget` refs, ~10 lines |

### Key Simplification

`add_edge_with_reverse` in both `edge.rs` and `insertion.rs` currently has a
`match apply_target` with two branches. After this phase, the `Local` branch
is deleted. The function signature drops the `&ApplyTarget` parameter entirely:

```rust
// BEFORE (edge.rs ~L346-405):
fn add_edge_with_reverse(txn, source, dest, inode, apply_target) {
    match apply_target {
        ApplyTarget::Global => {
            txn.put_graph(source, forward_edge)?;
            txn.put_graph(dest, reverse_edge)?;
            txn.put_inode_graph(inode, source, forward_edge)?;
            txn.put_inode_graph(inode, dest, reverse_edge)?;
        }
        ApplyTarget::Local { stack_id } => {
            txn.put_stack_graph(*stack_id, source, forward_edge)?;
            txn.put_stack_graph(*stack_id, dest, reverse_edge)?;
        }
    }
}

// AFTER:
fn add_edge_with_reverse(txn, source, dest, inode) {
    txn.put_graph(source, forward_edge)?;
    txn.put_graph(dest, reverse_edge)?;
    if let Some(inode_val) = inode {
        txn.put_inode_graph(inode_val, source, forward_edge)?;
        txn.put_inode_graph(inode_val, dest, reverse_edge)?;
    }
}
```

### Checklist

- [x] Remove `ApplyTarget` enum from `mod.rs`
- [x] Remove `ApplyTarget::from_view_scope()` constructor and tests
- [x] Rename and simplify `write_edge_map` (was `apply_edge_map`) — dropped `ApplyTarget` param
- [x] Rename and simplify `write_new_edge` (was `apply_new_edge`) — dropped `ApplyTarget` param
- [x] Simplify `add_edge_with_reverse` in `edge.rs` — removed match, always GRAPH + INODE_GRAPH
- [x] Simplify `del_edge_with_reverse` in `edge.rs` — removed match, always GRAPH + INODE_GRAPH
- [x] Rename and simplify `write_new_vertex` (was `apply_new_vertex`) in `insertion.rs` — dropped `ApplyTarget` param
- [x] Simplify `add_edge_with_reverse` in `insertion.rs` — removed match, always GRAPH + INODE_GRAPH
- [x] Replace `resolve_vertex_for_target` with `resolve_vertex(txn, pos, is_predecessor)` in `edge.rs`
- [x] Remove `resolve_context_vertex_for_target` from `position.rs` (callers use `resolve_context_vertex`)
- [x] Remove `FindBlockMode` enum — replaced by `is_predecessor: bool`
- [x] `apply_file_ops` left unchanged (CRDT operation, not graph routing)
- [x] Updated re-exports in `mod.rs`
- [x] Proactively fixed `atomic-repository/src/apply.rs`: removed `ApplyTarget` import, simplified `should_apply_hunks` to `!already_in_graph`, updated call sites to `write_new_vertex`/`write_edge_map`
- [x] Run `cargo check -p atomic-core` — passes clean
- [x] Run `cargo test -p atomic-core` — 442 passed, 0 failed

---

## Phase 3: Materialize Rename

**Goal**: Rename the output/materialization layer.  
**Depends on**: Phase 1 (trait renames only — can run in parallel with Phase 2).

### Files to Modify

| File | Changes |
|------|---------|
| `atomic-core/src/output/repo/repository.rs` | `output_repository` → `materialize_view`, `output_repository_prefix` → `materialize_prefix`, ~30 lines |
| `atomic-core/src/output/repo/options.rs` | `RepositoryOutputOptions` → `MaterializeOptions`, ~40 lines |
| `atomic-core/src/output/repo/error.rs` | `RepositoryOutputError` → `MaterializeError`, ~20 lines |
| `atomic-core/src/output/repo/mod.rs` | Update re-exports, ~10 lines |
| `atomic-core/src/output/mod.rs` | Update re-exports, ~10 lines |

### Optional: Module Rename

The `output/` directory could be renamed to `materialize/` for full consistency.
This is optional — the function names carry the semantics regardless of module path.
Recommendation: rename the module.

### Checklist

- [ ] Rename `output_repository()` → `materialize_view()`
- [ ] Rename `output_repository_prefix()` → `materialize_prefix()`
- [ ] Rename `RepositoryOutputOptions` → `MaterializeOptions`
- [ ] Rename `RepositoryOutputResult` → `MaterializeResult`
- [ ] Rename `RepositoryOutputError` → `MaterializeError`
- [ ] Update re-exports in `output/repo/mod.rs` and `output/mod.rs`
- [ ] Update all call sites in `atomic-core` tests
- [ ] Run `cargo check -p atomic-core`

---

## Phase 4: Repository Layer

**Goal**: Rename all repository methods, simplify the insert operation, remove OverlayTxn usage.  
**Depends on**: Phases 1, 2, 3.

### Files to Modify

| File | Changes |
|------|---------|
| `atomic-repository/src/apply.rs` → `insert.rs` | Rename file, rename all types (`ApplyOptions` → `InsertOptions`, etc.), remove `should_apply_hunks` logic, simplify `write_change_to_graph`, ~300 lines |
| `atomic-repository/src/repository/mod.rs` | All stack → view method renames, `output_working_copy` → `materialize`, remove OverlayTxn usage, `collect_stack_change_ids` → `collect_view_change_ids`, ~400 lines |
| `atomic-repository/src/repository/apply.rs` → `insert.rs` | Rename file, `apply_change` → `insert_change`, `apply_from_stack` → `insert_from_view` (simplify to metadata-only), `apply_recorded` → `write_recorded`, ~300 lines |
| `atomic-repository/src/repository/content.rs` | Remove OverlayTxn usage, replace with change-filter-only approach, `get_file_content_via_overlay` → `get_file_content`, `diff_stacks` → `diff_views`, ~200 lines |
| `atomic-repository/src/repository/status.rs` | Remove OverlayTxn usage, ~50 lines |
| `atomic-repository/src/repository/stacks.rs` → `views.rs` | Rename file if exists, ~50 lines |
| `atomic-repository/src/repository/record.rs` | Update call to `write_recorded` (was `apply_recorded`), ~20 lines |
| `atomic-repository/src/repository/tags.rs` | `*_stack` → `*_view` in method names, ~40 lines |
| `atomic-repository/src/repository/changes.rs` | `find_attestations_for_stack` → `find_attestations_for_view`, ~10 lines |
| `atomic-repository/src/repository/history.rs` | Update `StackState` → `ViewState` references, ~50 lines |
| `atomic-repository/src/repository/tests.rs` | Update all test names and assertions, ~200 lines |
| `atomic-repository/src/lib.rs` | Update `pub mod apply` → `pub mod insert`, update re-exports, ~30 lines |
| `atomic-repository/src/error.rs` | `StackNotFound` → `ViewNotFound`, etc., ~20 lines |

### Key Simplification: insert_from_view

The most impactful change is `apply_from_stack` → `insert_from_view`. The current
implementation at `repository/apply.rs:669-786` re-applies hunks and copies edges.
The new implementation is metadata-only:

```rust
pub fn insert_from_view(&self, options: CrossViewInsertOptions)
    -> Result<CrossViewInsertOutcome, RepositoryError>
{
    let mut txn = self.pristine.write_txn()?;
    let source = txn.get_view(&options.from_view)?;
    let mut target = txn.get_view(&options.to_view)?;

    // Get changes in source but not in target
    let missing = filter_missing_in_view(&txn, &source, &target)?;

    let mut outcome = CrossViewInsertOutcome::new();
    for (node_id, hash) in &missing {
        // Just add the change reference to the target view's log
        txn.put_change(&mut target, *node_id, hash)?;
        outcome.record_inserted(hash);
    }

    txn.update_view(&target)?;
    txn.commit()?;
    Ok(outcome)
}
```

### Key Simplification: materialize (was output_working_copy)

Remove all `OverlayTxn` construction. Build a `change_filter` from the view's
change set and pass it to `materialize_view()`:

```rust
pub fn materialize(&self) -> Result<MaterializeResult, RepositoryError> {
    let txn = self.pristine.read_txn()?;
    let view = txn.get_view(&self.current_view)?;

    // Build the change filter — this IS the view
    let change_filter = compute_view_filter(&txn, &view)?;

    let working_copy = FileSystem::from_root(&self.root);
    let options = MaterializeOptions::new()
        .with_change_filter(change_filter);

    // No OverlayTxn needed — just the raw txn + filter
    materialize_view(&txn, &self.change_store, &working_copy, options)
}
```

### Checklist

- [ ] Rename `atomic-repository/src/apply.rs` → `insert.rs`
- [ ] Rename `atomic-repository/src/repository/apply.rs` → `insert.rs`
- [ ] Rename all types: `ApplyOptions` → `InsertOptions`, etc.
- [ ] Rewrite `insert_from_view` as metadata-only operation
- [ ] Remove `should_apply_hunks` logic from `write_change_to_graph`
- [ ] Rewrite `materialize()` without `OverlayTxn`
- [ ] Replace `get_file_content_via_overlay` with filter-only `get_file_content`
- [ ] Update `status.rs` to use filter-only approach
- [ ] Rename all `*_stack*` methods → `*_view*`
- [ ] Rename `RecordOptions::stack()` → `view()`, `.get_stack()` → `.get_view()`, field `stack` → `view`
- [ ] Rename `RecordOptions::apply_after_record()` → `write_after_record()`, `.get_apply_after_record()` → `.get_write_after_record()`, field `apply_after_record` → `write_after_record`
- [ ] Update `lib.rs` module declarations and re-exports
- [ ] Update all tests in `repository/tests.rs`
- [ ] Update record tests: `test_options_stack` → `test_options_view`, `test_options_apply_after_record` → `test_options_write_after_record`
- [ ] Run `cargo check -p atomic-repository`
- [ ] Run `cargo test -p atomic-repository`

---

## Phase 4.5: Agent Crate

**Goal**: Update `atomic-agent` references to stack/apply vocabulary.  
**Depends on**: Phase 4 (repository API renames).

### Scope

The `atomic-agent` crate has 897 tests and references the repository API directly.
It does **not** import core pristine types (`StackState`, `StackTxnT`, etc.) but
it does call `Repository` methods and defines its own stack-related error variants.

### Files to Modify

| File | Changes |
|------|---------|
| `atomic-agent/src/error.rs` | `StackError` variant → `ViewError`, field `stack_name` → `view_name`, error message `"stack"` → `"view"`, ~20 lines |
| `atomic-agent/src/export.rs` | `atomic_revision()`: `current_stack()` → `current_view()`, `get_stack_info()` → `get_view_info()`, `VcsInfo.stack` field → `VcsInfo.view`, doc comments, ~20 lines |
| `atomic-agent/src/turn/orchestrator.rs` | `handle_session_start`: `current_stack()` → `current_view()`, `set_parent_stack()` → `set_parent_view()`, `create_stack_from()` → `create_view_from()`, `set_current_stack()` → `set_current_view()`, log messages `"agent stack"` → `"agent view"`, ~30 lines |
| `atomic-agent/src/turn/orchestrator.rs` | `handle_session_end`: `switch_stack()` → `switch_view()`, log messages `"user stack"` → `"user view"`, ~15 lines |
| `atomic-agent/src/record.rs` | `.stack()` → `.view()` on `RecordOptions` builder, doc comments, ~10 lines |
| `atomic-agent/src/lib.rs` | Doc table: `"Atomic stacks"` → `"Atomic views"`, ~5 lines |
| `atomic-agent/src/hooks/sherpa.rs` | Comment: `"stack-fork"` → `"view-fork"`, ~2 lines |
| `atomic-agent/src/transcript.rs` | Doc comments only (CRDT `apply` references stay), ~2 lines |

### AgentSession Fields

The `AgentSession` struct likely has a `stack_name` field and a `parent_stack`
field. These should be renamed:

| Current | Proposed |
|---------|----------|
| `session.stack_name` | `session.view_name` |
| `session.set_parent_stack()` | `session.set_parent_view()` |
| `session.parent_stack` | `session.parent_view` |

### Checklist

- [ ] Rename `AgentError::StackError` → `AgentError::ViewError` (with field renames)
- [ ] Update `atomic_revision()` in `export.rs` to use `current_view()` / `get_view_info()`
- [ ] Rename `VcsInfo.stack` → `VcsInfo.view`
- [ ] Update `TurnOrchestrator` session start/end to use view API
- [ ] Update `AgentSession` fields: `stack_name` → `view_name`, `parent_stack` → `parent_view`
- [ ] Update `record.rs` RecordOptions builder call
- [ ] Update doc comments and log messages throughout
- [ ] Run `cargo check -p atomic-agent`
- [ ] Run `cargo test -p atomic-agent`

---

## Phase 4.6: Remote Client Crate

**Goal**: Update `atomic-remote` references to stack/apply vocabulary.  
**Depends on**: Phase 4 (repository API renames).

### Scope

The `atomic-remote` crate is the HTTP client for remote operations (push, pull,
clone). It has 158 tests and a public API where `stack` appears as a parameter
name in every method. It also constructs URL query strings with `?stack=` and
`?apply=` parameters, which means the **wire protocol** changes too.

> **Wire protocol note**: The `atomic-enterprise/atomic-api` server must be
> updated in lockstep. The query parameter renames (`?stack=` → `?view=`,
> `?apply=` → `?insert=`) are a **breaking API change**. Since nobody is using
> this yet, that's fine — but both sides must move together.

### Files to Modify

| File | Changes |
|------|---------|
| `atomic-remote/src/error.rs` | `StackNotFound` → `ViewNotFound`, `EmptyStack` → `EmptyView`, `stack_not_found()` → `view_not_found()`, `empty_stack()` → `empty_view()`, suggestion text `"atomic stack list"` → `"atomic view list"`, ~40 lines |
| `atomic-remote/src/http.rs` | All method parameter renames `stack: &str` → `view: &str`, URL query strings `?stack=` → `?view=`, `?apply=` → `?insert=`, doc comments throughout, ~80 lines |
| `atomic-remote/src/lib.rs` | Re-exports and trait definitions if any use `stack`, ~10 lines |

### Key Method Renames in `HttpRemote`

| Current | Proposed |
|---------|----------|
| `get_state(stack: &str)` | `get_state(view: &str)` |
| `get_changelist(stack: &str, from: u64)` | `get_changelist(view: &str, from: u64)` |
| `get_id(stack: &str)` | `get_id(view: &str)` |
| `upload_change(hash, stack, data)` | `upload_change(hash, view, data)` |
| `upload_tag(state, stack, short_data)` | `upload_tag(state, view, short_data)` |

### URL Query Parameter Renames

| Current | Proposed |
|---------|----------|
| `?stack={}&state=` | `?view={}&state=` |
| `?stack={}&changelist={}` | `?view={}&changelist={}` |
| `?stack={}&id` | `?view={}&id` |
| `?apply={}&stack={}` | `?insert={}&view={}` |
| `?tagup={}&stack={}` | `?tagup={}&view={}` |

### Checklist

- [ ] Rename `RemoteError::StackNotFound` → `ViewNotFound` (with field rename)
- [ ] Rename `RemoteError::EmptyStack` → `EmptyView` (with field rename)
- [ ] Rename constructor methods `stack_not_found()` → `view_not_found()`, `empty_stack()` → `empty_view()`
- [ ] Update suggestion text: `"atomic stack list"` → `"atomic view list"`
- [ ] Rename all `stack` parameters → `view` in `HttpRemote` methods
- [ ] Update all URL query strings: `?stack=` → `?view=`, `?apply=` → `?insert=`
- [ ] Update debug log messages: `"GET state"`, `"POST apply"` → `"POST insert"`
- [ ] Update all doc comments
- [ ] Update tests in `error.rs` (`test_empty_stack`, `test_is_not_found_variants`)
- [ ] Run `cargo check -p atomic-remote`
- [ ] Run `cargo test -p atomic-remote`

### atomic-enterprise Coordination

The `atomic-enterprise/atomic-api` server parses these query parameters. It must
be updated to accept the new parameter names. This is tracked separately but must
ship at the same time as this phase:

- [ ] Audit `atomic-enterprise/atomic-api` for `?stack=` and `?apply=` parsing
- [ ] Update server-side parameter parsing to use `?view=` and `?insert=`
- [ ] Update server-side error messages

---

## Phase 5: CLI Layer

**Goal**: Rename all user-facing commands, flags, help text, and error messages.  
**Depends on**: Phase 4 (repository API).

### Files to Modify

| File | Changes |
|------|---------|
| `atomic-cli/src/commands/stack/mod.rs` → `view/mod.rs` | `Stack` → `View`, `StackCommands` → `ViewCommands`, ~40 lines |
| `atomic-cli/src/commands/stack/new.rs` → `view/new.rs` | `--local` → `--draft` (or keep --local), `--parent` stays, `stack` → `view` in help text, ~100 lines |
| `atomic-cli/src/commands/stack/switch.rs` → `view/switch.rs` | Rename, ~30 lines |
| `atomic-cli/src/commands/stack/delete.rs` → `view/delete.rs` | Rename, ~50 lines |
| `atomic-cli/src/commands/stack/list.rs` → `view/list.rs` | `[shared]`/`[isolated]` → `[shared]`/`[draft]`, ~40 lines |
| `atomic-cli/src/commands/apply.rs` → `insert.rs` | Full rewrite of command struct, subcommands, runners, help text, ~300 lines |
| `atomic-cli/src/commands/mod.rs` | `pub mod stack` → `pub mod view`, `pub mod apply` → `pub mod insert`, ~20 lines |
| `atomic-cli/src/main.rs` | `Commands::Stack` → `Commands::View`, `Commands::Apply` → `Commands::Insert`, help text, examples, ~80 lines |
| `atomic-cli/src/error.rs` | `StackNotFound` → `ViewNotFound`, suggestion text (`atomic stack list` → `atomic view list`), ~40 lines |
| `atomic-cli/src/commands/clone/command.rs` | Update apply references → insert, ~20 lines |
| `atomic-cli/src/commands/pull/command.rs` | Update apply references → insert, ~20 lines |
| `atomic-cli/src/commands/revise.rs` | Update apply references → insert, ~30 lines |
| `atomic-cli/src/commands/stash.rs` | Update stack references → view, ~10 lines |
| `atomic-cli/src/commands/init.rs` | "initial stack" → "initial view" in help text, ~10 lines |
| `atomic-cli/src/commands/log/` | Stack references in tests, ~10 lines |
| `atomic-cli/src/commands/diff/` | Stack references in tests, ~10 lines |

### CLI Flag Renames

| Current | Proposed |
|---------|----------|
| `atomic stack new <name>` | `atomic view create <name>` |
| `atomic stack new --local --parent dev` | `atomic view create --draft --parent dev` |
| `atomic stack list --verbose` | `atomic view list --verbose` |
| `atomic stack switch <name>` | `atomic view switch <name>` |
| `atomic stack delete <name>` | `atomic view delete <name>` |
| `atomic apply <hash>` | `atomic insert <hash>` |
| `atomic apply <hash> --stack dev` | `atomic insert <hash> --view dev` |
| `atomic apply from-stack <from> --to-stack <to>` | `atomic insert from-view <from> --to-view <to>` |
| `atomic apply tag <tag> --from-stack <from>` | `atomic insert tag <tag> --from-view <from>` |
| `atomic apply pick <hash...> --to-stack <to>` | `atomic insert pick <hash...> --to-view <to>` |
| `atomic apply preview <from> --to-stack <to>` | `atomic insert preview <from> --to-view <to>` |

### Checklist

- [ ] Move `commands/stack/` directory → `commands/view/`
- [ ] Rename `commands/apply.rs` → `commands/insert.rs`
- [ ] Rename `Stack` command struct → `View`
- [ ] Rename `Apply` command struct → `Insert`
- [ ] Update all subcommand names and help text
- [ ] Update `--stack` flag → `--view` everywhere
- [ ] Update `--to-stack` → `--to-view`, `--from-stack` → `--from-view`
- [ ] Rename `--local` flag → `--draft` in `view create`
- [ ] Update `main.rs` `Commands` enum and dispatch
- [ ] Update `error.rs` variants and suggestion text
- [ ] Update `clone`, `pull`, `revise`, `stash` commands
- [ ] Run `cargo check -p atomic-cli`
- [ ] Run `cargo test -p atomic-cli`

---

## Phase 6: Test Harness

**Goal**: Update all shell-based integration tests to use the new vocabulary.  
**Depends on**: Phase 5 (CLI commands must be renamed first).

### Estimated Changes: ~400 lines across 11 files

### helpers.sh — The Source of Truth

Function renames (these cascade to every test that calls them):

| Current | Proposed |
|---------|----------|
| `new_stack()` | `new_view()` |
| `switch_stack()` | `switch_view()` |
| `apply_from_stack()` | `insert_from_view()` |
| `list_stacks()` | `list_views()` |
| `assert_current_stack()` | `assert_current_view()` |
| `assert_stack_exists()` | `assert_view_exists()` |

Internal CLI calls inside helpers:

| Current | Proposed |
|---------|----------|
| `atomic stack new "$name"` | `atomic view create "$name"` |
| `atomic stack switch "$name"` | `atomic view switch "$name"` |
| `atomic stack list` | `atomic view list` |
| `atomic apply from-stack "$from" --to-stack "$to"` | `atomic insert from-view "$from" --to-view "$to"` |
| `.atomic/current_stack` | `.atomic/current_view` |

### Per-File Impact

| File | Lines to Change | Key Changes |
|------|----------------|-------------|
| `helpers.sh` | ~30 | Rename 6 functions + CLI commands inside them |
| `01_single_file.sh` | ~2 | `assert_current_stack` → `assert_current_view` |
| `02_multiple_files.sh` | 0 | No stack/apply references |
| `03_cross_stack.sh` | ~100 | **Heaviest** — all helper calls, comments, rename file to `03_cross_view.sh` |
| `04_directories.sh` | ~25 | Stack helper calls + comments |
| `05_semantic_layer.sh` | ~60 | Stack helper calls + comments about "stack isolation" → "view isolation" |
| `05_workspaces.sh` | ~80 | Stack helpers, `.atomic/workspaces/` paths (unchanged), comments |
| `06_tags_and_stash.sh` | ~30 | Stack helper calls |
| `07_server_push.sh` | ~20 | `--stack` → `--view`, `atomic apply "$HASH" --stack` → `atomic insert "$HASH" --view` |
| `08_local_stack_apply.sh` | ~25 | Rename to `08_draft_view_insert.sh`, `--stack` → `--view`, `AGENT_STACK` → `AGENT_VIEW` |
| `09_semantic_diff_pairing.sh` | 0 | No stack/apply references |
| `10_git_import.sh` | ~5 | `assert_stack_exists` → `assert_view_exists` |

### File Renames

| Current | Proposed |
|---------|----------|
| `03_cross_stack.sh` | `03_cross_view.sh` |
| `08_local_stack_apply.sh` | `08_draft_view_insert.sh` |

### Checklist

- [ ] Update `helpers.sh` — rename all 6 functions and their internal CLI calls
- [ ] Update `01_single_file.sh`
- [ ] Rename and update `03_cross_stack.sh` → `03_cross_view.sh`
- [ ] Update `04_directories.sh`
- [ ] Update `05_semantic_layer.sh`
- [ ] Update `05_workspaces.sh`
- [ ] Update `06_tags_and_stash.sh`
- [ ] Rename and update `07_server_push.sh`
- [ ] Rename and update `08_local_stack_apply.sh` → `08_draft_view_insert.sh`
- [ ] Update `10_git_import.sh`
- [ ] Run `./tests/harness/run_all.sh` — all tests should pass

---

## Phase 7: Rust Test Suite

**Goal**: Update all Rust unit and integration tests.  
**Depends on**: Phases 1-5 (the code they test must be renamed first).

### Scope

| Category | Test Count | Files |
|----------|-----------|-------|
| Tests matching `fn test_.*stack` | 142 | 40 files |
| Tests matching `fn test_.*apply` | 90 | 14 files (only non-CRDT need renaming) |
| Tests matching `fn test_.*output` | 76 | 16 files |
| Integration test: `stack_graph_test.rs` | 69 | 1 file (2,108 lines) — **rewrite** |
| Integration test: `push_integration_test.rs` | 33 | 1 file (520 lines) |

### High-Impact Files

| File | Tests | Action |
|------|-------|--------|
| `atomic-core/tests/stack_graph_test.rs` | 69 | **Rewrite as `view_test.rs`** — most tests for STACK_GRAPH are now dead (STACK_GRAPH eliminated). Keep tests that exercise GRAPH behavior, rewrite to use view filters instead of overlay. |
| `atomic-repository/src/repository/tests.rs` | 22 stack + 6 apply | Rename test functions, update API calls |
| `atomic-repository/src/apply.rs` | 12 | Rename to `insert.rs`, rename test functions |
| `atomic-cli/src/commands/stack/new.rs` | 5 | Move to `view/new.rs`, rename tests |
| `atomic-cli/src/error.rs` | 5 | Rename error variant tests |
| `atomic-core/src/pristine/traits.rs` | 9 | Rename `test_stack_*` → `test_view_*` |
| `atomic-core/src/pristine/tables.rs` | 8 | Rename table tests |
| `atomic-core/src/apply/change.rs` | 8 | Rename tests |
| `atomic-core/src/output/repo/repository.rs` | 6 | Rename `test_output_*` → `test_materialize_*` |

### CRDT Apply Tests — NO RENAME

The following files contain `apply` in test names but are **CRDT operations**
(genuinely "applying" ops to a data structure). These stay unchanged:

- `atomic-core/src/crdt/apply/trunk.rs` (14 tests)
- `atomic-core/src/crdt/apply/leaf.rs` (14 tests)
- `atomic-core/src/crdt/apply/branch.rs` (12 tests)
- `atomic-core/src/crdt/apply/error.rs` (3 tests)
- `atomic-core/src/crdt/apply/mod.rs` (1 test)
- `atomic-core/src/record/workflow/crdt/builder.rs` (2 tests)
- `atomic-core/tests/crdt_integration_test.rs` (7 tests)

### Checklist

- [ ] Rewrite `stack_graph_test.rs` → `view_test.rs` (eliminate STACK_GRAPH tests, add view filter tests)
- [ ] Update `push_integration_test.rs`
- [ ] Update all `test_*stack*` functions (142 tests, 40 files)
- [ ] Update non-CRDT `test_*apply*` functions (~45 tests, ~7 files)
- [ ] Update `test_*output*` functions that reference materialization (~30 tests)
- [ ] Run `cargo test` — full workspace, all tests pass

---

## Phase 8: Documentation and AGENTS.md

**Goal**: Update all documentation to reflect the new vocabulary and architecture.  
**Depends on**: All previous phases.

### Files to Modify

| File | Changes |
|------|---------|
| `atomic/AGENTS.md` | Major rewrite — stack→view, apply→insert, output→materialize throughout. Update architecture diagrams, table references, code examples, roadmap. ~500 lines. |
| `atomic/README.md` | Update CLI examples, ~50 lines |
| `atomic/docs/REFACTOR-VIEWS.md` | Mark phases as complete, update status |

### Checklist

- [ ] Rewrite `AGENTS.md` — all stack→view, apply→insert, output→materialize
- [ ] Update `AGENTS.md` architecture diagrams
- [ ] Update `AGENTS.md` "Two-Tier Stack Graph Model" section → "Ambient Graph + View Filters"
- [ ] Update `AGENTS.md` code examples throughout
- [ ] Update `README.md` CLI examples
- [ ] Update this document's status to Complete

---

## Deleted Code

Code that is **eliminated entirely** (not renamed):

| What | Where | Why |
|------|-------|-----|
| `OverlayTxn` struct + all impls | `atomic-core/src/pristine/overlay.rs` (~610 lines) | Views use change filters on canonical GRAPH, no overlay needed |
| `ApplyTarget` enum | `atomic-core/src/apply/mod.rs` (~40 lines) | Always writes to GRAPH |
| `STACK_GRAPH` table | `atomic-core/src/pristine/tables.rs` | All edges in GRAPH |
| `put_stack_graph` / `del_stack_graph` / `del_stack_graph_prefix` | `pristine/traits.rs`, `txn/write/*.rs` | Dead code without STACK_GRAPH |
| `iter_stack_graph_adjacent` | `pristine/traits.rs`, `txn/read.rs` | Dead code |
| `iter_stack_graph_vertices_for_change` | `pristine/traits.rs`, `txn/read.rs` | Dead code |
| `should_apply_hunks` logic | `atomic-repository/src/apply.rs` | Always write hunks |
| `resolve_vertex_for_target` | `atomic-core/src/apply/position.rs` | Was ApplyTarget-aware |
| `resolve_context_vertex_for_target` | `atomic-core/src/apply/position.rs` | Was ApplyTarget-aware |
| `FindBlockMode` enum (if only used by overlay) | `atomic-core/src/pristine/overlay.rs` | Part of deleted OverlayTxn |
| `collect_stack_graph_vertices_for_change` | `atomic-core/src/pristine/overlay.rs` | Part of deleted OverlayTxn |
| `find_block_in_stack_graph` | `atomic-core/src/pristine/overlay.rs` | Part of deleted OverlayTxn |
| `is_file_alive` (OverlayTxn method) | `atomic-core/src/pristine/overlay.rs` | Replaced by filter-based aliveness |
| `has_change_in_graph` (OverlayTxn-specific) | Check if still needed for GRAPH-only | May survive as utility |

**Estimated lines deleted**: ~900 lines  
**Estimated lines modified**: ~3,500 lines  
**Net impact**: Simpler codebase, fewer abstractions, one canonical graph.

---

## Risk Register

| Risk | Severity | Mitigation |
|------|----------|------------|
| **Phase 1 breaks everything downstream** | High | Expected. Work crate-by-crate: `atomic-core` first, then `atomic-repository`, then `atomic-cli`. Use `cargo check` per crate. |
| **View filter performance regression** | Medium | The `change_filter` HashSet is O(1) per lookup. `INODE_GRAPH` handles file-local traversal. Profile `materialize()` on a repo with 10k+ changes before/after. |
| **GC complexity for view deletion** | Medium | Draft view deletion just removes `VIEW_CHANGES` entries. Orphaned edges in GRAPH are harmless until GC runs. Implement GC as a separate follow-up command (`atomic gc`). |
| **`stack_graph_test.rs` (2,108 lines) rewrite** | High | Most tests verify STACK_GRAPH behavior that no longer exists. Rewrite to test view-filter-based isolation instead. Keep graph-level tests that exercise GRAPH directly. |
| **Cross-crate import breakage cascade** | High | Rename in dependency order: `atomic-core` → `atomic-repository` → `atomic-cli`. Each phase should end with `cargo check` for that crate. |
| **Shell harness tests depend on exact CLI output** | Medium | Update helpers first, then run each test file individually to catch output format changes. |
| **`atomic-remote` wire protocol change** | Medium | URL query params change (`?stack=` → `?view=`, `?apply=` → `?insert=`). Covered in Phase 4.6. Must update `atomic-enterprise/atomic-api` server in lockstep. |
| **`atomic-enterprise` server-side breakage** | Medium | Server parses `?stack=` and `?apply=` query params. Must be updated to match Phase 4.6 client changes. Audit separately. |
| **`atomic-agent` crate references** | Medium | 897 tests in `atomic-agent`. Covered in Phase 4.5 — `AgentError::StackError`, `VcsInfo.stack`, `TurnOrchestrator` session start/end, `AgentSession` fields. |

---

## Verification Strategy

### Per-Phase Gates

Each phase must pass its gate before the next phase begins:

| Phase | Gate |
|-------|------|
| Phase 1 | `cargo check -p atomic-core` passes |
| Phase 2 | `cargo check -p atomic-core` passes |
| Phase 3 | `cargo check -p atomic-core` passes |
| Phase 4 | `cargo check -p atomic-repository` passes, `cargo test -p atomic-repository` passes |
| Phase 4.5 | `cargo check -p atomic-agent` passes, `cargo test -p atomic-agent` passes |
| Phase 4.6 | `cargo check -p atomic-remote` passes, `cargo test -p atomic-remote` passes |
| Phase 5 | `cargo check -p atomic-cli` passes, `cargo test -p atomic-cli` passes |
| Phase 6 | `./tests/harness/run_all.sh` — all harness tests pass |
| Phase 7 | `cargo test` (full workspace) — all 6,414 tests pass |
| Phase 8 | Manual review of documentation |

### End-to-End Validation

After all phases complete, run the full validation:

```bash
# 1. Full workspace build
cargo build

# 2. Full workspace tests (6,414 tests)
cargo test

# 3. Shell harness (13 test scripts)
./tests/harness/run_all.sh

# 4. Clippy clean
cargo clippy -- -D warnings

# 5. Verify no "stack" references remain in user-facing code
# (internal comments referencing the old model are acceptable)
grep -r "atomic stack" atomic-cli/src/ && echo "FAIL: stack references in CLI" || echo "PASS"
grep -r "atomic apply" atomic-cli/src/ && echo "FAIL: apply references in CLI" || echo "PASS"
grep -r "StackKind\|StackState\|StackTxnT" atomic-core/src/ && echo "FAIL: old types" || echo "PASS"
grep -r "STACK_GRAPH" atomic-core/src/ && echo "FAIL: STACK_GRAPH references" || echo "PASS"

# 6. Verify new vocabulary is used
grep -r "atomic view" atomic-cli/src/ && echo "PASS: view command exists"
grep -r "atomic insert" atomic-cli/src/ && echo "PASS: insert command exists"
grep -r "ViewScope\|ViewState\|ViewTxnT" atomic-core/src/ && echo "PASS: new types exist"
grep -r "materialize" atomic-core/src/output/ && echo "PASS: materialize exists"
```

### Regression Targets

The following test files are the most critical regression targets:

| File | Why |
|------|-----|
| `tests/harness/03_cross_view.sh` | Core view isolation invariant (was cross-stack) |
| `tests/harness/07_server_push.sh` | Server-side insert flow |
| `tests/harness/08_draft_view_insert.sh` | 10-change sequential insert at scale |
| `atomic-core/tests/view_test.rs` | Rewritten from stack_graph_test.rs |
| `atomic-repository/src/repository/tests.rs` | Repository-level integration |
| `atomic-cli/tests/push_integration_test.rs` | Push/pull with view model |
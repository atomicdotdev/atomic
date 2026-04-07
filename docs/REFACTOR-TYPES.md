# REFACTOR-TYPES.md — Type-Safe Edge Model & File Splitting

> **Status**: In Progress — Phase A1 + B1.3 Complete  
> **Created**: 2025-07-13  
> **Last Updated**: 2025-07-13  
> **Prerequisite**: REFACTOR-VIEWS.md (complete)  

## Table of Contents

- [Motivation](#motivation)
- [Part A: Type-Safe Edge Model](#part-a-type-safe-edge-model)
  - [Current State: Bitflag Spaghetti](#current-state-bitflag-spaghetti)
  - [Proposed: Semantic Edge Types](#proposed-semantic-edge-types)
  - [Phase A1: Edge Kind Enum](#phase-a1-edge-kind-enum)
  - [Phase A2: Typed Graph Queries](#phase-a2-typed-graph-queries)
  - [Phase A3: ViewGraph as the Standard Interface](#phase-a3-viewgraph-as-the-standard-interface)
  - [Phase A4: Typed Edge Construction in Record Pipeline](#phase-a4-typed-edge-construction-in-record-pipeline)
- [Part B: File Splitting](#part-b-file-splitting)
  - [Inventory: Files Over 500 Lines](#inventory-files-over-500-lines)
  - [Phase B1: Critical Path Splits](#phase-b1-critical-path-splits)
  - [Phase B2: Repository Layer Splits](#phase-b2-repository-layer-splits)
  - [Phase B3: Remaining Splits](#phase-b3-remaining-splits)
- [Risk Register](#risk-register)
- [Verification Strategy](#verification-strategy)

---

## Motivation

The REFACTOR-VIEWS work exposed a fundamental problem: the edge model uses
bitflags (`EdgeFlags`) with `if`/`then`/`continue`/`while` chains scattered
across 30+ files. This makes the code:

1. **Impossible to reason about** — you trace an `if edge.flag().contains(EdgeFlags::DELETED)`
   through 6 call levels, each with its own filter logic, and still miss the bug.
2. **Impossible to refactor** — changing one conditional breaks a distant `continue`
   in a loop you didn't know existed.
3. **Impossible to analyze statically** — a tool like tree-sitter can't tell you
   "this function handles deleted edges" because the semantics are encoded in
   runtime bit comparisons, not in the type system.
4. **Hostile to the ambient graph model** — the view filter logic was bolted onto
   existing bitflag checks with `Option<Arc<HashSet<NodeId>>>` threading, creating
   a second axis of conditionals layered on top of the first.

The fix is to use **Rust's type system** — enums, structs, traits, pattern matching —
to make edge semantics explicit and compiler-checkable.

### The Evidence

| File | Lines | Control Constructs | Density |
|------|------:|-------------------:|--------:|
| `output/alive/retrieve.rs` | 1,273 | 112 | 1 per 11 lines |
| `record/workflow/globalize/hunk.rs` | 613 | 65 | 1 per 9 lines |
| `apply/edge.rs` | 884 | 62 | 1 per 14 lines |
| `pristine/txn/write/mod.rs` | 1,269 | 78 | 1 per 16 lines |
| `record/workflow/record.rs` | 2,758 | 58 | 1 per 48 lines |

131 files exceed 500 lines. 7 exceed 2,000. The three densest files
(`retrieve.rs`, `hunk.rs`, `edge.rs`) are exactly where the view-filter
bug manifested — and where the fix required understanding interactions
across all three simultaneously.

---

## Part A: Type-Safe Edge Model

### Current State: Bitflag Spaghetti

```rust
// Current: 5 bits, 2^5 = 32 possible combinations
bitflags! {
    pub struct EdgeFlags: u8 {
        const BLOCK   = 0b0000_0001;  // Sequential content
        const PSEUDO  = 0b0000_0100;  // Synthetic connectivity
        const FOLDER  = 0b0001_0000;  // Filesystem hierarchy
        const PARENT  = 0b0010_0000;  // Reverse direction
        const DELETED = 0b1000_0000;  // Removed content
    }
}
```

Of 32 possible combinations, only **13 are semantically valid**. The other 19
are nonsensical (e.g., `BLOCK | FOLDER` — an edge can't be both content and
directory hierarchy). But the type system can't prevent constructing them.

Every function that touches edges does runtime checks:

```rust
// This pattern appears 70+ times across the codebase
if edge.flag().contains(EdgeFlags::DELETED) {
    if edge.flag().contains(EdgeFlags::PARENT) {
        if self.passes_filter(edge.introduced_by()) {
            // ...
        } else {
            if edge.flag().contains(EdgeFlags::BLOCK) || vertex.is_empty() {
                // ...
            }
        }
    }
}
```

### The 13 Valid Edge Kinds

Analysis of every `EdgeFlags` usage across the codebase reveals exactly 13
semantically distinct edge kinds:

| # | Flags | Name | Meaning |
|---|-------|------|---------|
| 1 | `BLOCK` | Alive content | Sequential content edge within a file |
| 2 | `BLOCK \| DELETED` | Deleted content | Content was removed |
| 3 | `FOLDER` | Alive folder | Directory hierarchy edge |
| 4 | `FOLDER \| DELETED` | Deleted folder | Directory was removed |
| 5 | `PSEUDO \| BLOCK` | Pseudo content | Synthetic connectivity (content) |
| 6 | `PSEUDO \| FOLDER` | Pseudo folder | Synthetic connectivity (folder) |
| 7 | `BLOCK \| PARENT` | Parent content | Reverse of #1 |
| 8 | `BLOCK \| PARENT \| DELETED` | Parent deleted content | Reverse of #2 |
| 9 | `FOLDER \| PARENT` | Parent folder | Reverse of #3 |
| 10 | `FOLDER \| PARENT \| DELETED` | Parent deleted folder | Reverse of #4 |
| 11 | `PSEUDO \| BLOCK \| PARENT` | Parent pseudo content | Reverse of #5 |
| 12 | `PSEUDO \| FOLDER \| PARENT` | Parent pseudo folder | Reverse of #6 |
| 13 | `PARENT` | Alive parent (bare) | Used as range minimum only |

### The 6 Semantic Query Patterns

Every `iter_adjacent` call in the codebase falls into one of 6 patterns:

| Pattern | Purpose | Current (min, max flags) | Files |
|---------|---------|--------------------------|-------|
| **Forward alive** | DFS traversal | `(empty, BLOCK\|PSEUDO)` | `retrieve.rs` |
| **Forward all** | DFS with deleted | `(empty, BLOCK\|PSEUDO\|DELETED)` | `retrieve.rs` |
| **Parent alive** | Aliveness check | `(PARENT, all()-DELETED)` | `retrieve.rs` |
| **Parent all** | Full parent check | `(PARENT, all())` | `retrieve.rs` |
| **Parent content** | Predecessor lookup | `(BLOCK\|PARENT, BLOCK\|PARENT\|FOLDER)` | `hunk.rs` |
| **Forward content** | Edge provenance | `(BLOCK, BLOCK\|FOLDER)` | `hunk.rs` |

### Proposed: Semantic Edge Types

Replace the bitflag model with types that make invalid states unrepresentable:

```rust
/// The semantic kind of a graph edge.
///
/// This replaces `EdgeFlags` bitflags with an exhaustive enum that the
/// compiler can verify.  Every variant is a valid edge kind; invalid
/// combinations cannot be constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    // ── Forward edges (content flows in this direction) ──────────
    /// Live content within a file.
    Block,
    /// Deleted content (still in graph, not alive).
    BlockDeleted,
    /// Live directory hierarchy.
    Folder,
    /// Deleted directory hierarchy.
    FolderDeleted,
    /// Synthetic content connectivity (computed, not stored in changes).
    PseudoBlock,
    /// Synthetic folder connectivity.
    PseudoFolder,
}

impl EdgeKind {
    /// The reverse (parent) variant of this forward edge.
    /// Every forward edge has exactly one parent mirror.
    pub fn as_parent(self) -> ParentEdgeKind {
        match self {
            Self::Block        => ParentEdgeKind::Block,
            Self::BlockDeleted => ParentEdgeKind::BlockDeleted,
            Self::Folder       => ParentEdgeKind::Folder,
            Self::FolderDeleted => ParentEdgeKind::FolderDeleted,
            Self::PseudoBlock  => ParentEdgeKind::PseudoBlock,
            Self::PseudoFolder => ParentEdgeKind::PseudoFolder,
        }
    }

    pub fn is_deleted(self) -> bool {
        matches!(self, Self::BlockDeleted | Self::FolderDeleted)
    }

    pub fn is_folder(self) -> bool {
        matches!(self, Self::Folder | Self::FolderDeleted
                     | Self::PseudoFolder)
    }

    pub fn is_pseudo(self) -> bool {
        matches!(self, Self::PseudoBlock | Self::PseudoFolder)
    }

    /// Convert to wire format for storage.
    pub fn to_flags(self) -> EdgeFlags { ... }

    /// Parse from wire format.
    pub fn from_flags(flags: EdgeFlags) -> Option<Self> { ... }
}

/// Parent (reverse) edge kinds — separate type prevents mixing directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParentEdgeKind {
    Block,
    BlockDeleted,
    Folder,
    FolderDeleted,
    PseudoBlock,
    PseudoFolder,
}
```

### Typed Edge Structs

```rust
/// A forward edge in the graph.
pub struct ForwardEdge {
    pub kind: EdgeKind,
    pub dest: Position<NodeId>,
    pub introduced_by: NodeId,
}

/// A parent (reverse) edge in the graph.
pub struct ParentEdge {
    pub kind: ParentEdgeKind,
    pub dest: Position<NodeId>,
    pub introduced_by: NodeId,
}

/// Any edge (for raw iteration when you need both directions).
pub enum Edge {
    Forward(ForwardEdge),
    Parent(ParentEdge),
}
```

### Typed Query Methods on GraphTxnT

Replace the single `iter_adjacent(node, min_flag, max_flag)` with
purpose-specific methods:

```rust
pub trait GraphTxnT {
    // ── Typed queries (new) ─────────────────────────────────────
    
    /// Iterate forward edges (content flows in this direction).
    /// Returns only non-parent edges.  Filter by kind if needed.
    fn iter_forward(
        &self,
        node: GraphNode<NodeId>,
        include_deleted: bool,
    ) -> Result<impl Iterator<Item = Result<ForwardEdge, PristineError>> + '_, PristineError>;

    /// Iterate parent (reverse) edges.
    /// Returns only parent edges.
    fn iter_parents(
        &self,
        node: GraphNode<NodeId>,
        include_deleted: bool,
    ) -> Result<impl Iterator<Item = Result<ParentEdge, PristineError>> + '_, PristineError>;

    // ── Structural lookups (unchanged) ──────────────────────────
    fn find_block(&self, pos: Position<NodeId>) -> ...;
    fn find_block_end(&self, pos: Position<NodeId>) -> ...;
    fn has_vertex(&self, node: GraphNode<NodeId>) -> ...;
    fn get_external(&self, id: NodeId) -> ...;
    fn get_internal(&self, hash: &Hash) -> ...;

    // ── Legacy (deprecated, remove after migration) ─────────────
    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> ...;
}
```

### What the Code Becomes

**Before** (70 `if` statements in `retrieve.rs`):

```rust
// Gate 1: Skip parent edges (14 characters of bitflag check)
if edge.flag().intersects(EdgeFlags::PARENT) {
    continue;
}

// Gate 3: Check aliveness (nested 4-level conditional)
if edge.flag().contains(EdgeFlags::DELETED) {
    let introduced_by = edge.introduced_by();
    if !self.passes_filter(introduced_by) {
        return true;
    }
    return false;
}
true
```

**After** (exhaustive pattern match):

```rust
// Forward edges only — parent edges are a different type, can't appear here.
// The compiler enforces this.

for edge in txn.iter_forward(node, include_deleted)? {
    let edge = edge?;
    match edge.kind {
        EdgeKind::Block | EdgeKind::PseudoBlock | EdgeKind::Folder => {
            // Alive edge — vertex is reachable
        }
        EdgeKind::BlockDeleted | EdgeKind::FolderDeleted => {
            // Deleted edge — skip (or check filter for state-based retrieval)
        }
        EdgeKind::PseudoFolder => {
            // Pseudo folder — structural connectivity only
        }
    }
}
```

**Before** (`is_vertex_alive_at_target` — 40 lines of nested conditionals):

```rust
if flag.contains(EdgeFlags::DELETED) {
    if self.passes_filter(introduced_by) {
        deleted_by_filter_change = true;
    } else {
        if flag.contains(EdgeFlags::BLOCK) || vertex.is_empty() {
            has_live_parent = true;
        }
    }
} else if flag.contains(EdgeFlags::BLOCK) || vertex.is_empty() {
    has_live_parent = true;
}
```

**After** (flat match):

```rust
for parent in txn.iter_parents(vertex, true)? {
    let parent = parent?;
    match parent.kind {
        ParentEdgeKind::Block | ParentEdgeKind::Folder
        | ParentEdgeKind::PseudoBlock | ParentEdgeKind::PseudoFolder => {
            // Non-deleted parent — vertex is connected
            has_live_parent = true;
        }
        ParentEdgeKind::BlockDeleted | ParentEdgeKind::FolderDeleted => {
            if visible.contains(&parent.introduced_by) {
                // Deletion from our view — vertex is dead
                deleted_by_our_view = true;
            } else {
                // Deletion from another view — still alive from our perspective
                has_live_parent = true;
            }
        }
    }
}
```

Every branch is visible. Every case is handled. The compiler rejects
missing arms. A tree-sitter query can find "every place that handles
deleted parent edges" with a single AST pattern.

---

### Phase A1: Edge Kind Enum ✅ COMPLETE

**Goal**: Introduce `EdgeKind`, `ParentEdgeKind`, `ForwardEdge`, `ParentEdge`
types alongside the existing `EdgeFlags`.  Provide conversion functions.
Do NOT change any call sites yet.

**Result**: `atomic-core/src/types/edge_kind.rs` created (797 lines, 35 tests). All 6,876 workspace tests pass.

**Files created**:
- `atomic-core/src/types/edge_kind.rs` — the new types (797 lines)

**Files modified**:
- `atomic-core/src/types/mod.rs` — added `pub mod edge_kind;` and re-exports

**Checklist**:
- [x] Define `EdgeKind` enum with 6 forward variants
- [x] Define `ParentEdgeKind` enum with 6 reverse variants
- [x] Define `ForwardEdge` and `ParentEdge` structs
- [x] Define `Edge` enum (Forward | Parent)
- [x] Implement `EdgeKind::to_flags()` and `EdgeKind::from_flags()`
- [x] Implement `ParentEdgeKind::to_flags()` and `ParentEdgeKind::from_flags()`
- [x] Implement conversion from `SerializedGraphEdge` → `Edge` (parse direction + kind)
- [x] Exhaustive round-trip tests (35 tests covering all 13 valid combinations + rejection of invalid combos)
- [x] `cargo test -p atomic-core` — passes

---

### Phase A2: Typed Graph Queries ✅ COMPLETE

**Goal**: Add `iter_forward` and `iter_parents` methods to `GraphTxnT`.
These wrap `iter_adjacent` and return typed edges. Existing code continues
to use `iter_adjacent`; new code uses the typed methods.

**Result**: Two default methods added to `GraphTxnT` with 12 new tests. All 3,196 `atomic-core` lib tests pass.

**Files modified**:
- `atomic-core/src/pristine/traits.rs` — added default methods + `MockGraph` tests

**Implementation notes**:
- Both methods are **default methods** on `GraphTxnT`, so `ReadTxn`, `WriteTxn`,
  and `ViewGraph` get them for free with no code changes.
- `ViewGraph` inherits correct filtering automatically because its `iter_adjacent`
  already filters by `introduced_by` visibility — the default impls delegate
  through it.
- Flag ranges are carefully computed to avoid missing edge kinds:
  - `iter_forward` alive: `[empty, PSEUDO|FOLDER]` (0x00–0x14)
  - `iter_forward` +deleted: `[empty, DELETED|FOLDER]` (0x00–0x90), skips PARENT edges in loop
  - `iter_parents` alive: `[PARENT, all()-DELETED]` (0x20–0x35)
  - `iter_parents` +deleted: `[PARENT, all()]` (0x20–0xB5), skips non-PARENT edges in loop
- A consistency test verifies `iter_forward(true).len() + iter_parents(true).len()`
  equals the count of valid `Edge` variants from raw `iter_adjacent`.

**Checklist**:
- [x] Add `iter_forward(&self, node, include_deleted)` as default method on `GraphTxnT`
- [x] Add `iter_parents(&self, node, include_deleted)` as default method on `GraphTxnT`
- [x] Both methods delegate to `iter_adjacent` + parse edges into typed structs
- [x] `ViewGraph` inherits via default methods (no override needed)
- [x] Unit tests: 12 tests with `MockGraph` verify typed iteration matches raw iteration
- [x] `cargo test -p atomic-core` — 3,196 tests pass

---

### Phase A3: ViewGraph as the Standard Interface

**Goal**: Make `ViewGraph` the standard way to read the graph for any
view-scoped operation. Every function that currently takes
`T: GraphTxnT` + a manual `Option<Arc<HashSet<NodeId>>>` change filter
should take `T: GraphTxnT` where `T` is already a `ViewGraph`.

**Files to modify (migrate to typed queries)**:

| File | Change |
|------|--------|
| `output/alive/retrieve.rs` | Replace all `iter_adjacent` + `if PARENT continue` with `iter_forward`. Replace `is_vertex_alive_at_target` internals with `iter_parents` + match. Remove `is_alive_at_target_state` (single-edge check — replaced by typed iteration). |
| `output/alive/retrieve.rs` | Remove `change_filter` field from `RetrieveOptions` — filtering is handled by `ViewGraph`. |
| `output/repo/repository.rs` | Remove `change_filter` field from `MaterializeOptions` — use `ViewGraph` in the caller. |
| `repository/mod.rs` | `materialize()` passes `ViewGraph` to `materialize_view()` instead of raw txn + filter. |
| `repository/content.rs` | All content retrieval uses `ViewGraph`. Remove manual filter building. |
| `repository/record.rs` | Already uses `ViewGraph` (from the bug fix). |

**Checklist**:
- [ ] Rewrite `retrieve_graph` to use `iter_forward` and `iter_parents`
- [ ] Remove `is_alive_at_target_state` (single-edge check — no longer needed)
- [ ] Simplify `is_vertex_alive_at_target` to use `iter_parents` + match
- [ ] Remove `change_filter` from `RetrieveOptions`
- [ ] Remove `change_filter` from `MaterializeOptions`
- [ ] Update `materialize()` to pass `ViewGraph` instead of raw txn
- [ ] Update `materialize_prefix()` same way
- [ ] Update all content retrieval to use `ViewGraph`
- [ ] Remove `is_vertex_alive` (non-filtered version) — `ViewGraph` with no filter handles this
- [ ] `cargo test -p atomic-core`
- [ ] `cargo test -p atomic-repository`
- [ ] Run `tests/harness/run_all.sh` — all suites pass

---

### Phase A4: Typed Edge Construction in Record Pipeline

**Goal**: The record/globalize pipeline creates `NewEdge` structs with
`EdgeFlags` fields. Replace these with typed edge construction.

**Files to modify**:
- `record/workflow/globalize/hunk.rs` — `create_deletion_edges_for_vertices` uses `EdgeFlags::BLOCK` and `EdgeFlags::BLOCK | EdgeFlags::DELETED`. Replace with `EdgeKind::Block` and `EdgeKind::BlockDeleted`.
- `apply/edge.rs` — `write_new_edge` and `add_edge_with_reverse` take `EdgeFlags`. Add overloads or migrate to `EdgeKind`.

**Checklist**:
- [ ] Update `NewEdge` to use `EdgeKind` for `previous` and `flag` fields
- [ ] Update `create_deletion_edges_for_vertices` to use `EdgeKind::BlockDeleted`
- [ ] Update `write_new_edge` to work with `EdgeKind`
- [ ] Update `add_edge_with_reverse` to accept `EdgeKind` and compute `ParentEdgeKind` from it
- [ ] Deprecate raw `EdgeFlags` construction outside of serialization
- [ ] `cargo test` — full workspace
- [ ] `tests/harness/run_all.sh` — all suites pass

---

## Part B: File Splitting

### Inventory: Files Over 500 Lines

**131 files** exceed 500 lines. 7 exceed 2,000. Prioritized by impact:

| Tier | Criteria | Files | Action |
|------|----------|------:|--------|
| 🔴 Critical | >1500 lines AND on the bug-prone critical path | 8 | Split immediately |
| 🟠 High | >1000 lines OR high conditional density | 25 | Split next |
| 🟡 Medium | 500–1000 lines, moderate complexity | 40 | Split opportunistically |
| 🟢 Low | 500–1000 lines, simple structure (e.g., tests, types) | 58 | Leave or split on touch |

### Phase B1: Critical Path Splits

These files are on the hot path for the view-filter bug class and have the
highest conditional density. Splitting them makes the type-safe edge migration
(Part A) tractable.

#### B1.1: `record/workflow/record.rs` (2,758 lines → 5 files)

| New File | Contents | ~Lines |
|----------|----------|-------:|
| `record/workflow/record/options.rs` | `RecordingOptions` + builder + `Default` | 250 |
| `record/workflow/record/types.rs` | `RecordingStats`, `RecordedFile`, `RecordingResult`, `IntoIterator` impls | 560 |
| `record/workflow/record/mod.rs` | `record_added_file`, `record_deleted_file`, `record_modified_file`, `calculate_line_offsets`, re-exports | 380 |
| `record/workflow/record/crdt.rs` | `build_crdt_ops_for_added_file`, `build_crdt_ops_for_deleted_file`, `build_crdt_ops_for_modified_file` (the 566-line function) | 600 |
| `record/workflow/record/tests.rs` | All `#[cfg(test)]` tests | 762 |

**Note**: `build_crdt_ops_for_modified_file` is 566 lines and contains its
own sub-algorithm for bigram-based Delete/Insert → Modify promotion.
Extract to `crdt_consolidation.rs` as a follow-up.

#### B1.2: `output/alive/retrieve.rs` (1,273 lines → 4 files)

| New File | Contents | ~Lines |
|----------|----------|-------:|
| `output/alive/retrieve/options.rs` | `RetrieveOptions` + builder + filter logic + `PartialEq`/`Eq`, `RetrieveResult` | 350 |
| `output/alive/retrieve/mod.rs` | `retrieve_graph` + re-exports | 160 |
| `output/alive/retrieve/classify.rs` | `create_alive_vertex`, `new_vertex_at_position`, `is_vertex_alive`, `is_vertex_zombie` | 110 |
| `output/alive/retrieve/tests.rs` | All tests | 506 |

#### B1.3: `repository/mod.rs` (1,562 lines → 5 files) ✅ COMPLETE

| New File | Actual Lines | Contents |
|----------|-------------:|---------:|
| `repository/mod.rs` | 574 | `Repository` struct, construction, path accessors, internal helpers, re-exports |
| `repository/views.rs` | 458 | `create_view`, `create_shared_view`, `nearest_shared_ancestor`, `create_view_from`, `list_views`, `view_exists`, `delete_view`, `get_view_info`, `ViewInfo` |
| `repository/switch.rs` | 303 | `switch_view`, `restore_workspace_to_working_copy`, `merge_dir_into`, `collect_ignored_paths_on_disk`, workspace helpers |
| `repository/materialize.rs` | 169 | `visible_file_paths`, `materialize`, `materialize_prefix` |
| `repository/filter.rs` | 98 | `collect_view_change_ids`, `collect_visible_change_ids` |

#### B1.4: `apply.rs` (1,290 lines → 5 files)

| New File | Contents | ~Lines |
|----------|----------|-------:|
| `apply/types.rs` | `InsertError`, `InsertResult`, `InsertOptions`, `InsertStats`, `InsertOutcome`, impls | 290 |
| `apply/mod.rs` | `write_change_to_graph`, `write_hunk`, `check_missing_dependencies`, `compute_insert_order`, `collect_all_dependencies`, re-exports | 310 |
| `apply/cross_view.rs` | `CrossViewInsertOptions`, `CrossViewInsertOutcome`, impls | 140 |
| `apply/queries.rs` | `get_view_changes`, `get_missing_changes`, `get_changes_up_to_seq`, `filter_missing_in_view`, `order_changes_by_deps` | 165 |
| `apply/tests.rs` | All tests | 249 |

**Checklist for Phase B1**:
- [ ] Split `record/workflow/record.rs` into 5 files
- [ ] Split `output/alive/retrieve.rs` into 4 files
- [x] Split `repository/mod.rs` into 5 files — 1,562 → 574 lines (+ 4 sub-modules all under 500)
- [ ] Split `apply.rs` into 5 files
- [ ] `cargo test` — full workspace, all tests pass
- [ ] `tests/harness/run_all.sh` — all suites pass

---

### Phase B2: Repository Layer Splits

| File | Lines | Split Into | ~Files |
|------|------:|------------|-------:|
| `pristine/txn/write/mod.rs` | 1,269 | Extract `view_ops.rs` (452 lines: `create_view`, `del_change`, `reinsert_change`, `del_view`), `crdt_ops.rs` (198 lines: 18 CRDT table methods), `registration.rs` (123 lines) | 3 new |
| `pristine/traits.rs` | 2,079 | Extract `view_trait.rs` (~260 lines: `ViewTxnT` + default impls), `mut_trait.rs` (~500 lines: `MutTxnT`), keep `GraphTxnT` + `TreeTxnT` in `traits.rs` | 2 new |
| `record.rs` (repository) | 1,614 | Extract `record/options.rs`, `record/outcome.rs`, `record/tests.rs` | 3 new |
| `history.rs` | 1,590 | Extract `history/types.rs`, `history/iter.rs`, `history/tests.rs` | 3 new |
| `changestore.rs` | 1,933 | Extract `changestore/memory.rs`, `changestore/filesystem.rs`, `changestore/tests.rs` | 3 new |

**Checklist for Phase B2**:
- [ ] Split each file as described
- [ ] `cargo test` — full workspace
- [ ] No file exceeds 800 lines after splitting

---

### Phase B3: Remaining Splits

The remaining 40+ files in the 500–1000 line range. These are lower priority
and can be split opportunistically when touching them for other reasons.

**Guideline**: When modifying any file over 500 lines, check if it can be
split. Apply the same pattern: types in `types.rs`, tests in `tests.rs`,
core logic in `mod.rs`, domain sub-concerns in named files.

Key candidates:

| File | Lines | Notes |
|------|------:|-------|
| `change/format_v3/compact.rs` | 2,300 | Serialization — split by read/write |
| `change/format_v3/types.rs` | 2,195 | Type definitions — split by category |
| `change/format_v3/writer.rs` | 2,084 | Writer — split by section type |
| `record/workflow/graph_op.rs` | 2,064 | Graph operation types — split types/impls/tests |
| `change/provenance_graph.rs` | 1,828 | Provenance — split graph/serialization/tests |
| `tracking.rs` | 1,806 | Tracking — split by concern (add/remove/query) |
| `tags.rs` | 1,682 | Tags — split by concern (CRUD/query/tests) |
| `output/repo/repository.rs` | 1,640 | Materialize — split options/traversal/tests |

---

## Critical: Add Tests to `hunk.rs`

`record/workflow/globalize/hunk.rs` has **ZERO tests** and implements the
critical globalization pipeline where the view-filter bug manifested.

Before ANY refactoring of this file, add integration tests covering:
- [ ] Single file add (insertion context)
- [ ] Single file modify (deletion + insertion edges)
- [ ] File delete (deletion edges only)
- [ ] Two views modifying same file (the bug scenario)
- [ ] File with multiple content blocks
- [ ] Predecessor edge resolution
- [ ] Introduced-by edge lookup

---

## Risk Register

| Risk | Severity | Mitigation |
|------|----------|------------|
| **EdgeKind enum adds overhead** | Low | `EdgeKind` is 1 byte (same as `EdgeFlags`). Pattern matches compile to jump tables. Zero runtime cost. |
| **Storage format change** | None | `EdgeKind` converts to/from `EdgeFlags` for serialization. The on-disk format is unchanged. |
| **`iter_adjacent` is load-bearing** | High | Keep `iter_adjacent` during migration. New typed methods are default impls that delegate to it. Remove `iter_adjacent` only after all call sites are migrated. |
| **File splitting breaks imports** | Medium | Split one file at a time. Re-export from `mod.rs` so external callers don't change. Run `cargo check` after each split. |
| **Splitting tests breaks test isolation** | Low | Move tests to `tests.rs` sub-module. `#[cfg(test)]` and `use super::*;` preserve access. |
| **131 files to split is overwhelming** | High | Phase B1 (4 critical files) provides 80% of the benefit. B2 (5 more) covers the next tier. B3 is opportunistic. |

---

## Verification Strategy

### Per-Phase Gates

| Phase | Gate |
|-------|------|
| A1 | `cargo test -p atomic-core` — edge kind round-trips pass |
| A2 | `cargo test -p atomic-core` — typed queries match raw queries |
| A3 | `cargo test` (full workspace) + `tests/harness/run_all.sh` — all pass |
| A4 | `cargo test` (full workspace) + `tests/harness/run_all.sh` — all pass |
| B1 | `cargo test` (full workspace) — no regressions, no file over 800 lines |
| B2 | `cargo test` (full workspace) — no regressions |

### Structural Validation

After Phase A3, verify that the codebase has:
- Zero `if edge.flag().contains(EdgeFlags::DELETED)` patterns (replaced by match arms)
- Zero `if edge.flag().intersects(EdgeFlags::PARENT) { continue }` patterns (replaced by typed iteration)
- Zero `Option<Arc<HashSet<NodeId>>>` change filter threading (replaced by `ViewGraph`)
- All `iter_adjacent` calls migrated to `iter_forward` or `iter_parents`

```bash
# These should all return 0 after Phase A3:
grep -rn "flag().contains(EdgeFlags::DELETED)" atomic-core/src/ | wc -l
grep -rn "flag().intersects(EdgeFlags::PARENT)" atomic-core/src/ | wc -l
grep -rn "Option<Arc<HashSet<NodeId>>>" atomic-core/src/ atomic-repository/src/ | wc -l
grep -rn "iter_adjacent" atomic-core/src/output/ atomic-core/src/record/ | wc -l
```

### Line Count Validation

After Phase B1, verify:
```bash
# No file should exceed 800 lines in the split modules:
find atomic-core/src/output/alive/retrieve/ \
     atomic-core/src/record/workflow/record/ \
     atomic-repository/src/repository/ \
     atomic-repository/src/apply/ \
     -name '*.rs' -exec wc -l {} \; | awk '$1 > 800 {print "FAIL:", $0}'
```

# Graph Access Performance Refactoring Plan

## Problem

redb's `open_multimap_table()` acquires a mutex and performs a system-catalog B-tree lookup on every call. The `ReadTxn` implementation of `GraphTxnT` reopens the GRAPH table on every `find_block`, `iter_adjacent`, `find_block_end`, and `has_vertex` call. For operations that traverse the graph (materialize, record, content retrieval), this means thousands of table opens per file.

Under parallel execution (rayon), multiple threads contend on redb's internal mutex, amplifying the overhead from ~1ms to ~20-60ms per table open.

## Solutions Built

### 1. `CachedGraphTxn` (table handle caching)

Opens the GRAPH table once at construction, reuses the handle for all subsequent operations. Eliminates the per-call table-open mutex contention.

**Impact**: ~90x improvement per vertex (59ms → 0.65ms)

**Location**: `atomic-core/src/pristine/txn/read.rs`

### 2. `InodePreloadTxn` (full inode preloading)

Does ONE sequential range scan of INODE_GRAPH at construction, loading all edges for a specific file into a HashMap. All `find_block`/`iter_adjacent` calls then operate purely in-memory with O(1) lookups.

**Impact**: 14.2s → ~100ms for a 21,867-vertex file (140x improvement)

**Location**: `atomic-core/src/pristine/txn/read.rs`

### 3. `ViewGraph<T>` composability

`ViewGraph` wraps any `T: GraphTxnT` and filters edges by change visibility. It composes with both `CachedGraphTxn` and `InodePreloadTxn`, so callers get view filtering + performance optimization in one wrapper.

## Current Adoption

| Wrapper | Used In | NOT Used (Should Be) |
|---------|---------|---------------------|
| `CachedGraphTxn` | (defined but unused in hot paths) | record, content retrieval, status |
| `InodePreloadTxn` | `materialize_parallel` | record globalization, content retrieval, sequential materialize |
| `ViewGraph<ReadTxn>` | `record()` | Should be `ViewGraph<CachedGraphTxn>` |

## Refactoring Plan

### Phase 1: Record Path (P0 — largest single improvement)

**Current**: `record()` creates `ViewGraph::new(&txn, filter)` where `&txn` is a bare `ReadTxn`. Every `find_block`/`iter_adjacent` during globalization reopens the GRAPH table.

**Target**: `ViewGraph::new(&cached_txn, filter)` where `cached_txn` is `CachedGraphTxn`. Requires `CachedGraphTxn` to implement `InodeGraphOps` (delegation to inner `ReadTxn`).

**Files**:
- `atomic-core/src/pristine/txn/read.rs` — add `InodeGraphOps` impl for `CachedGraphTxn`
- `atomic-repository/src/repository/record.rs` — wrap txn in `CachedGraphTxn` before `ViewGraph`

### Phase 2: Content Retrieval (P1)

**Current**: 7 `get_file_content_*` methods create bare `ReadTxn` and call `retrieve_graph` directly.

**Target**: Each method creates `InodePreloadTxn` for the target file before calling `retrieve_graph`.

**Files**:
- `atomic-repository/src/repository/content.rs` — all `get_file_content_*` methods

### Phase 3: Sequential Materialize Paths (P1)

**Current**: `materialize_sequential`, `materialize_paths_sequential`, `materialize_prefix` use `materialize_view` with bare `ReadTxn`.

**Target**: Route through `materialize_parallel` or use per-file `InodePreloadTxn`.

**Files**:
- `atomic-repository/src/repository/materialize.rs`

### Phase 4: Apply Read Path (P2)

**Current**: `WriteTxn`'s `GraphTxnT` impl reopens GRAPH on every read call during change application.

**Target**: Cache the GRAPH table handle on `WriteTxn` (similar to how `GraphWriteBatch` caches it for writes).

**Files**:
- `atomic-core/src/pristine/txn/write/graph.rs` — add cached table handle

### Phase 5: Import Line Index Seeding (P2)

**Current**: `import_line_index_seed` uses bare `ReadTxn` with per-call table opens.

**Target**: `InodePreloadTxn` per file.

**Files**:
- `atomic-repository/src/repository/insert.rs`

### Phase 6: Architectural Default (P3)

**Goal**: Make `CachedGraphTxn` the default for ALL read transactions. Instead of callers wrapping `ReadTxn`, the `Pristine::read_txn()` method should return a type that has the GRAPH table handle pre-opened.

**Approach**: Either:
- Change `ReadTxn` to open the GRAPH table in its constructor and store it (requires solving the self-referential lifetime with `ouroboros` or `self_cell` crate)
- Or rename current `ReadTxn` to `RawReadTxn` and make `ReadTxn` an alias for `CachedGraphTxn`

This eliminates the need for callers to know about `CachedGraphTxn` — the optimization is built into the transaction layer.

## Performance Baseline

Measured on a real project (HHPTrailRouter, ~80 files, ~80K graph edges):

| Operation | Before All Optimizations | After All Optimizations |
|-----------|------------------------|----------------------|
| `view switch` (full materialize) | 8+ minutes | 1.1 seconds |
| Per-vertex graph access | 59ms | <0.01ms (in-memory) |
| Materialize 21K-vertex file | 345 seconds | ~100ms |
| Files processed during switch | 60+ (all) | 2 (only changed) |

## Design Principles

1. **Per-file operations should use `InodePreloadTxn`** — one range scan, then pure in-memory lookups
2. **Global operations should use `CachedGraphTxn`** — table handle cached, no per-call mutex
3. **View-filtered operations compose with `ViewGraph<T>`** — the filter layer is orthogonal to the performance layer
4. **The INODE_GRAPH table handle should be opened ONCE** per parallel operation and shared via `open_inode_graph_table()` + `InodePreloadTxn::from_table()`
5. **ChangeStore content reads use `peek()` with read locks** — no write-lock serialization for cache hits

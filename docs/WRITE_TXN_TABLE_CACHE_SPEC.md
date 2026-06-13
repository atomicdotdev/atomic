# WriteTxn Table Cache Refactor

## Problem

`WriteTxn::put_graph` and `WriteTxn::put_inode_graph` call `self.txn.open_multimap_table(GRAPH)` / `self.txn.open_multimap_table(INODE_GRAPH)` on every invocation. During `write_change_to_graph`, a single change with 1,500 hunks generates ~18,000 table opens (~6 edges × 2 tables per hunk). At ~1ms per open, the apply phase takes **19 seconds** — 70% of total record time.

The read side (`GraphTxnT::find_block`, `iter_adjacent`) on `WriteTxn` also calls `open_multimap_table(GRAPH)` per invocation (7 call sites in `graph.rs`).

## Root Cause

redb's `WriteTransaction::open_multimap_table` creates a new `MultimapTable` handle each call. Unlike `ReadTransaction` (where we already have `CachedGraphTxn` that opens tables once), `WriteTxn` has no caching.

We cannot store the `MultimapTable<'txn>` alongside `WriteTransaction` in the same struct because of the self-referential borrow — the table borrows the transaction.

## Proposed Solution: `CachedWriteGraphTxn`

Create a wrapper that opens GRAPH and INODE_GRAPH once, then delegates all reads and writes through the cached handles.

### New struct in `atomic-core/src/apply/graph_batch.rs` (or new file)

```rust
/// Cached write-side graph transaction.
/// Opens GRAPH and INODE_GRAPH once and provides both read and write
/// access through the same handles, eliminating per-operation table opens.
pub struct CachedWriteGraphTxn<'txn> {
    /// Mutable GRAPH table — used for put_graph, del_graph, find_block, iter_adjacent
    graph: MultimapTable<'txn, &'static [u8; 24], &'static [u8; 24]>,
    /// Mutable INODE_GRAPH table — used for put_inode_graph, del_inode_graph
    inode_graph: MultimapTable<'txn, &'static [u8; 32], &'static [u8; 24]>,
    /// Reference to the underlying WriteTxn for non-graph operations
    /// (get_external, get_internal, tree ops, etc.)
    txn: &'txn WriteTxn<'txn>,
}
```

### Key constraint

redb doesn't allow opening the same table as both a `MultimapTable` (write) and a read-only table simultaneously within the same `WriteTransaction`. So **all** GRAPH access must go through the cached `MultimapTable` handle — reads AND writes.

The `MultimapTable` already implements `ReadableMultimapTable`, so reads work through the same handle.

### Methods to implement

From `GraphTxnT` (reads via cached write handle):
- `find_block(pos) -> GraphNode` — uses `self.graph.range()` 
- `find_block_end(pos) -> GraphNode` — uses `self.graph.range()` + `self.graph.get()`
- `iter_adjacent(node, min, max) -> Vec<Edge>` — uses `self.graph.get()`
- `has_vertex(node) -> bool` — uses `self.graph.get()`
- `get_external(id) -> Hash` — delegates to `self.txn.get_external()`
- `get_internal(hash) -> NodeId` — delegates to `self.txn.get_internal()`

From `MutTxnT` (writes via cached write handle):
- `put_graph(node, edge)` — uses `self.graph.insert()`
- `del_graph(node, edge)` — uses `self.graph.remove()`
- `put_inode_graph(inode, node, edge)` — uses `self.inode_graph.insert()`
- `del_inode_graph(inode, node, edge)` — uses `self.inode_graph.remove()`

All other `MutTxnT` methods (tree ops, view ops, CRDT ops) delegate to `self.txn`.

### Existing code using `GraphWriteBatch`

`GraphWriteBatch` already has `find_block`, `find_block_end`, `iter_adjacent`, and `resolve_context_vertex`. These can be moved into `CachedWriteGraphTxn` or `CachedWriteGraphTxn` can wrap `GraphWriteBatch` and add the trait implementations.

### Files to change

| File | Change |
|------|--------|
| `atomic-core/src/apply/graph_batch.rs` | Evolve `GraphWriteBatch` into `CachedWriteGraphTxn` or add new struct. Implement `GraphTxnT` trait on it. |
| `atomic-core/src/apply/insertion.rs` | `write_new_vertex_batched` already uses `graph_batch` — just ensure ALL graph reads go through it (already done for `resolve_context_vertex`, need `check_deleted_context`). |
| `atomic-core/src/apply/edge.rs` | `write_edge_map_batched` — ensure it uses the cached handle for reads too. |
| `atomic-repository/src/apply/mod.rs` | `write_change_to_graph` — create `CachedWriteGraphTxn` at the top of the hunk loop, pass to all hunk writes. Drop it before `apply_file_ops_batched` (which needs `&mut txn`). |
| `atomic-core/src/pristine/txn/write/mod.rs` | No changes needed — `put_graph`/`del_graph` stay as-is for non-cached callers. |

### Call site change in `write_change_to_graph`

```rust
// Before (current — opens tables ~18,000 times):
for graph_op in hunks {
    write_hunk_unbatched(txn, ...)?;  // each call opens GRAPH + INODE_GRAPH
}

// After (opens tables ONCE):
{
    let mut cached = CachedWriteGraphTxn::new(&*txn)?;
    for graph_op in hunks {
        write_hunk_cached(&cached, ...)?;  // reads and writes through cached handle
    }
}  // cached dropped, releasing borrow on txn
// apply_file_ops_batched(txn, ...) can now borrow txn mutably
```

### Critical detail: `check_deleted_context`

`check_deleted_context` (insertion.rs:437) calls `txn.iter_adjacent()` which opens GRAPH. This MUST go through the cached handle. Options:
1. Pass `&CachedWriteGraphTxn` instead of `&T: GraphTxnT` to `check_deleted_context`
2. Make `CachedWriteGraphTxn` implement `GraphTxnT` so it can be passed as `&impl GraphTxnT`

Option 2 is cleaner — implement `GraphTxnT` on `CachedWriteGraphTxn`, then the existing function signatures work without change.

### Self-referential borrow avoidance

`CachedWriteGraphTxn` borrows `&'txn WriteTxn` (not owns it). The `WriteTxn` outlives the cached wrapper. The wrapper is scoped to the hunk loop and dropped before any `&mut WriteTxn` is needed.

### Expected performance

- **Before**: 1,529 hunks × ~12ms/hunk = 18.3s
- **After**: 1,529 hunks × ~0.1ms/hunk = 0.15s (table open cost amortized to zero)
- **Total record**: 28s → ~8s

### Testing

- `tests/harness/01_single_file.sh` — 63 tests including modify→record→status cycle
- `tests/harness/02_multiple_files.sh` — 83 tests including multi-file changes
- `tests/harness/19_unrecord.sh` — 48 tests including unrecord→re-record cycle
- `tests/harness/03_cross_view.sh` — 368 tests for cross-view operations

### Related: Delete the duplicated code

After this refactor, remove:
- `write_hunk_batched` / `write_hunk_unbatched` distinction — only one path
- `add_edge_with_reverse_batched` — use the cached handle's write methods
- `write_new_vertex_batched` / `write_new_vertex` — merge into one function taking `&impl GraphTxnT + GraphWriteOps`

This eliminates ~300 lines of duplicated apply code.

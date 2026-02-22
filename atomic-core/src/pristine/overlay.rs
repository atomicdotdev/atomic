//! Overlay transaction for stack-scoped graph traversal
//!
//! This module provides [`OverlayTxn`], a wrapper around any transaction that
//! implements [`GraphTxnT`] by unioning edges from the `STACK_GRAPH` chain
//! (for local workspaces) with the global `GRAPH`.
//!
//! # Motivation
//!
//! An local workspace's **effective view** is the union of its own `STACK_GRAPH`,
//! each isolated ancestor's `STACK_GRAPH`, and the global `GRAPH`. Without
//! `OverlayTxn`, all graph operations (`find_block`, `iter_adjacent`, etc.)
//! only read from the global `GRAPH`, making local workspace edges invisible.
//!
//! # Architecture
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────────────┐
//! │                        OverlayTxn<T>                               │
//! │                                                                    │
//! │  stack_chain: [feature_login, service_auth]                        │
//! │                                                                    │
//! │  iter_adjacent(vertex):                                            │
//! │    1. STACK_GRAPH[feature_login, vertex] → edges                   │
//! │    2. STACK_GRAPH[service_auth, vertex]  → edges                   │
//! │    3. GRAPH[vertex]                      → edges  (via inner T)    │
//! │    4. Deduplicate by SerializedGraphEdge equality                  │
//! │                                                                    │
//! │  find_block(pos):                                                  │
//! │    1. Scan STACK_GRAPH[chain[0]] for vertex containing pos         │
//! │    2. Scan STACK_GRAPH[chain[1]] for vertex containing pos         │
//! │    3. Fall back to inner.find_block(pos) → GRAPH                   │
//! │                                                                    │
//! │  get_external, get_internal, etc. → delegate to inner T            │
//! └────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use atomic_core::pristine::{OverlayTxn, StackTxnT};
//!
//! let chain = txn.resolve_overlay_chain(&stack)?;
//! let overlay = OverlayTxn::new(&txn, chain);
//!
//! // All GraphTxnT operations now read from the overlay:
//! let edges = overlay.iter_adjacent(vertex, min_flag, max_flag)?;
//! let block = overlay.find_block(position)?;
//! ```
//!
//! # When To Use
//!
//! - **Shared stacks**: No overlay needed — pass `chain = vec![]` or use
//!   the inner transaction directly. An empty chain makes `OverlayTxn`
//!   behave identically to the inner transaction.
//! - **Local workspaces**: Use `resolve_overlay_chain` to compute the chain,
//!   then wrap with `OverlayTxn`.

use std::collections::HashSet;

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::traits::{GraphTxnT, StackState, StackTxnT, TreeTxnT};
use crate::pristine::AdjIterator;
use crate::types::{
    ChangePosition, EdgeFlags, GraphNode, Hash, Inode, Merkle, NodeId, Position,
    SerializedGraphEdge,
};

/// A transaction wrapper that overlays `STACK_GRAPH` edges on top of the
/// global `GRAPH`.
///
/// `OverlayTxn` implements [`GraphTxnT`] so it can be used anywhere a
/// graph-reading transaction is expected (retrieve, diff, output, etc.).
///
/// The overlay chain is a list of local workspace IDs ordered from most
/// specific (current stack) to least specific (last isolated ancestor).
/// The global `GRAPH` is implicitly the base and is always consulted last.
///
/// When the chain is empty, `OverlayTxn` delegates directly to the inner
/// transaction with zero overhead on the hot path (only an `is_empty` check).
pub struct OverlayTxn<'a, T> {
    /// The underlying transaction (ReadTxn or WriteTxn).
    inner: &'a T,

    /// Stack IDs to overlay, ordered most-specific-first.
    ///
    /// For example, if the stack hierarchy is:
    ///   main (Shared) → dev (Shared) → service-auth (Local) → feature-login (Local)
    ///
    /// Then `stack_chain` for feature-login is `[feature_login_id, service_auth_id]`.
    /// The global GRAPH (containing dev and main's edges) is the implicit base.
    stack_chain: Vec<u64>,
}

impl<'a, T> OverlayTxn<'a, T> {
    /// Create a new overlay transaction.
    ///
    /// # Arguments
    ///
    /// * `inner` - The underlying transaction providing `GraphTxnT` + `StackTxnT`
    /// * `stack_chain` - Local workspace IDs to overlay, most-specific first.
    ///   Use an empty vec for shared stacks (equivalent to using `inner` directly).
    pub fn new(inner: &'a T, stack_chain: Vec<u64>) -> Self {
        Self { inner, stack_chain }
    }

    /// Create an overlay from a stack, automatically resolving the chain.
    ///
    /// This is a convenience constructor that calls `resolve_overlay_chain`
    /// on the given stack.
    pub fn from_stack(inner: &'a T, stack: &crate::pristine::StackState) -> PristineResult<Self>
    where
        T: StackTxnT,
    {
        let chain = inner.resolve_overlay_chain(stack)?;
        Ok(Self::new(inner, chain))
    }

    /// Get the overlay chain.
    pub fn stack_chain(&self) -> &[u64] {
        &self.stack_chain
    }

    /// Check if this overlay has any local workspace layers.
    ///
    /// When false, all operations delegate directly to the inner transaction.
    #[inline]
    pub fn has_overlay(&self) -> bool {
        !self.stack_chain.is_empty()
    }

    /// Get a reference to the inner transaction.
    pub fn inner(&self) -> &'a T {
        self.inner
    }
}

// ---------------------------------------------------------------------------
// GraphTxnT implementation
// ---------------------------------------------------------------------------

impl<'a, T: GraphTxnT + StackTxnT> GraphTxnT for OverlayTxn<'a, T> {
    type Adj = AdjIterator;

    // -- Pass-through methods (not affected by overlay) ---------------------

    fn get_external(&self, id: NodeId) -> PristineResult<Option<Hash>> {
        self.inner.get_external(id)
    }

    fn get_internal(&self, hash: &Hash) -> PristineResult<Option<NodeId>> {
        self.inner.get_internal(hash)
    }

    fn get_node_type(&self, node_id: NodeId) -> PristineResult<Option<u8>> {
        self.inner.get_node_type(node_id)
    }

    fn get_rev_deps(&self, dep_id: NodeId) -> PristineResult<Vec<NodeId>> {
        self.inner.get_rev_deps(dep_id)
    }

    // -- Overlay-aware methods ----------------------------------------------

    /// Iterate adjacent edges by unioning STACK_GRAPH chain with GRAPH.
    ///
    /// Edges are collected from each layer (most-specific first), then from
    /// the global GRAPH. Duplicates are removed by `SerializedGraphEdge`
    /// equality (24-byte comparison).
    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> PristineResult<Self::Adj> {
        // Fast path: no overlay → collect inner results into our AdjIterator
        if !self.has_overlay() {
            let inner_iter = self.inner.iter_adjacent(node, min_flag, max_flag)?;
            let edges: Vec<SerializedGraphEdge> = inner_iter.collect::<Result<Vec<_>, _>>()?;
            return Ok(AdjIterator::new(edges));
        }

        // Collect edges from each STACK_GRAPH layer
        let mut all_edges: Vec<SerializedGraphEdge> = Vec::new();
        let mut seen: HashSet<SerializedGraphEdge> = HashSet::new();

        for &stack_id in &self.stack_chain {
            let iter = self
                .inner
                .iter_stack_graph_adjacent(stack_id, node, min_flag, max_flag)?;
            for edge_result in iter {
                let edge = edge_result?;
                if seen.insert(edge) {
                    all_edges.push(edge);
                }
            }
        }

        // Collect edges from the global GRAPH
        let global_iter = self.inner.iter_adjacent(node, min_flag, max_flag)?;
        for edge_result in global_iter {
            let edge = edge_result?;
            if seen.insert(edge) {
                all_edges.push(edge);
            }
        }

        Ok(AdjIterator::new(all_edges))
    }

    /// Find the block containing a position, checking STACK_GRAPH layers first.
    ///
    /// For each layer in the stack chain, we scan `STACK_GRAPH[(stack_id, *)]`
    /// for a vertex that contains the given position. If found, we return it
    /// immediately. Otherwise, we fall back to the inner transaction's
    /// `find_block` which reads from the global `GRAPH`.
    ///
    /// The position-matching logic is the same as `ReadTxn::find_block`:
    /// - Non-empty vertices: `start <= pos < end` (preferred)
    /// - Empty vertices: `start == pos == end` (fallback)
    /// - ROOT position returns `GraphNode::ROOT`
    fn find_block(&self, pos: Position<NodeId>) -> PristineResult<GraphNode<NodeId>> {
        // ROOT is always virtual
        if pos.change.is_root() {
            return Ok(GraphNode::ROOT);
        }

        // Fast path: no overlay → delegate directly
        if !self.has_overlay() {
            return self.inner.find_block(pos);
        }

        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        // Search each STACK_GRAPH layer
        for &stack_id in &self.stack_chain {
            if let Some(vertex) = find_block_in_stack_graph(
                self.inner,
                stack_id,
                change_id,
                target_pos,
                FindBlockMode::ContainingPosition,
            )? {
                return Ok(vertex);
            }
        }

        // Fall back to global GRAPH
        self.inner.find_block(pos)
    }

    /// Find the block ending at a position, checking STACK_GRAPH layers first.
    ///
    /// Same layered lookup as `find_block` but uses end-position matching:
    /// - Empty vertices at exact position (preferred)
    /// - Vertices where `end == pos`
    /// - Vertices containing the position
    fn find_block_end(&self, pos: Position<NodeId>) -> PristineResult<GraphNode<NodeId>> {
        // ROOT is always virtual
        if pos.change.is_root() {
            return Ok(GraphNode::ROOT);
        }

        // Fast path: no overlay → delegate directly
        if !self.has_overlay() {
            return self.inner.find_block_end(pos);
        }

        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        // Search each STACK_GRAPH layer
        for &stack_id in &self.stack_chain {
            if let Some(vertex) = find_block_in_stack_graph(
                self.inner,
                stack_id,
                change_id,
                target_pos,
                FindBlockMode::EndingAtPosition,
            )? {
                return Ok(vertex);
            }
        }

        // Fall back to global GRAPH
        self.inner.find_block_end(pos)
    }

    /// Check if a vertex exists in any STACK_GRAPH layer or the global GRAPH.
    fn has_vertex(&self, node: GraphNode<NodeId>) -> PristineResult<bool> {
        // Fast path: no overlay → delegate directly
        if !self.has_overlay() {
            return self.inner.has_vertex(node);
        }

        // Check each STACK_GRAPH layer
        for &stack_id in &self.stack_chain {
            let iter = self.inner.iter_stack_graph_adjacent(
                stack_id,
                node,
                EdgeFlags::empty(),
                EdgeFlags::all(),
            )?;
            // A vertex "exists" if it has at least one edge
            for edge_result in iter {
                let _ = edge_result?;
                return Ok(true);
            }
        }

        // Check global GRAPH
        self.inner.has_vertex(node)
    }

    fn has_change_in_graph(&self, change_id: NodeId) -> PristineResult<bool> {
        // Intentionally checks only the global GRAPH — this method answers
        // "are this change's edges in the permanent shared graph?" which is
        // the correct semantics for deciding whether to re-apply hunks.
        // STACK_GRAPH edges are ephemeral and don't count.
        self.inner.has_change_in_graph(change_id)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers for STACK_GRAPH vertex lookup
//
// These are pub(crate) so that the apply module can reuse the same lookup
// logic instead of reimplementing it.  The canonical vertex-resolution
// strategy lives HERE; all other modules delegate to these helpers.
// ---------------------------------------------------------------------------

/// Controls which position-matching strategy `find_block_in_stack_graph` uses.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FindBlockMode {
    /// Match a vertex that **contains** the position: `start <= pos < end`
    /// (non-empty) or `start == pos == end` (empty, fallback).
    /// Used by `find_block`.
    ContainingPosition,

    /// Match a vertex that **ends at** the position: `end == pos`
    /// or an empty vertex at exact position (preferred).
    /// Used by `find_block_end`.
    EndingAtPosition,
}

/// Search a single STACK_GRAPH layer for a vertex matching the given position.
///
/// This replicates the matching logic from `ReadTxn::find_block` and
/// `ReadTxn::find_block_end`, but reads from `STACK_GRAPH[(stack_id, *)]`
/// instead of `GRAPH`.
///
/// # Arguments
///
/// * `txn` - The underlying transaction (for reading STACK_GRAPH via StackTxnT)
/// * `stack_id` - The local workspace to search
/// * `change_id` - The change ID from the position
/// * `target_pos` - The byte offset from the position
/// * `mode` - Whether to match containing-position or ending-at-position
///
/// # Returns
///
/// `Ok(Some(vertex))` if a matching vertex was found, `Ok(None)` otherwise.
pub(crate) fn find_block_in_stack_graph<T: StackTxnT>(
    txn: &T,
    stack_id: u64,
    change_id: u64,
    target_pos: u64,
    mode: FindBlockMode,
) -> PristineResult<Option<GraphNode<NodeId>>> {
    // For EndingAtPosition, check empty vertex at exact position first
    // (same priority as ReadTxn::find_block_end)
    if matches!(mode, FindBlockMode::EndingAtPosition) {
        let empty_node = GraphNode::new(
            NodeId::new(change_id),
            ChangePosition::new(target_pos),
            ChangePosition::new(target_pos),
        );
        // Check if this exact vertex has edges in STACK_GRAPH
        let iter = txn.iter_stack_graph_adjacent(
            stack_id,
            empty_node,
            EdgeFlags::empty(),
            EdgeFlags::all(),
        )?;
        for edge_result in iter {
            let _ = edge_result?;
            return Ok(Some(empty_node));
        }
    }

    // Scan all vertices for this change_id in this stack's STACK_GRAPH.
    //
    // We use a range scan on (stack_id, change_id, 0, 0) .. (stack_id, change_id+1, 0, 0)
    // to find all vertices belonging to this change.
    //
    // Since StackTxnT doesn't expose a raw range scan on STACK_GRAPH, we use
    // a targeted approach: try common vertex patterns that the position might
    // refer to. This avoids needing a new trait method for raw range iteration.
    //
    // Strategy: We check the position against vertices we can discover by
    // probing the STACK_GRAPH with likely vertex keys. For each vertex found
    // (via iter_stack_graph_adjacent returning non-empty), we check if it
    // matches the position criteria.
    //
    // However, this probe-based approach can miss vertices. For correctness,
    // we add a `iter_stack_graph_vertices` scan method below.

    let vertices = collect_stack_graph_vertices_for_change(txn, stack_id, change_id)?;

    let mut empty_match: Option<GraphNode<NodeId>> = None;

    for (v_start, v_end) in vertices {
        match mode {
            FindBlockMode::ContainingPosition => {
                // Prefer non-empty vertex containing this position
                if v_start != v_end && v_start <= target_pos && target_pos < v_end {
                    return Ok(Some(GraphNode::new(
                        NodeId::new(change_id),
                        ChangePosition::new(v_start),
                        ChangePosition::new(v_end),
                    )));
                }
                // Track empty vertex as fallback
                if v_start == v_end && v_start == target_pos && empty_match.is_none() {
                    empty_match = Some(GraphNode::new(
                        NodeId::new(change_id),
                        ChangePosition::new(v_start),
                        ChangePosition::new(v_end),
                    ));
                }
            }
            FindBlockMode::EndingAtPosition => {
                // Check for span that ends at this position
                if v_end == target_pos && v_start < v_end {
                    return Ok(Some(GraphNode::new(
                        NodeId::new(change_id),
                        ChangePosition::new(v_start),
                        ChangePosition::new(v_end),
                    )));
                }
                // Also check if position falls within [start, end)
                if v_start <= target_pos && target_pos < v_end {
                    return Ok(Some(GraphNode::new(
                        NodeId::new(change_id),
                        ChangePosition::new(v_start),
                        ChangePosition::new(v_end),
                    )));
                }
                // Empty vertex already checked above (priority lookup)
            }
        }
    }

    // Return empty fallback if found (ContainingPosition mode)
    if let Some(v) = empty_match {
        return Ok(Some(v));
    }

    Ok(None)
}

/// Collect all unique (start, end) vertex positions for a given change_id
/// within a stack's STACK_GRAPH.
///
/// This performs a range scan on the STACK_GRAPH table using the composite
/// key `(stack_id, change_id, *, *)` to find all vertices belonging to a
/// specific change in a specific stack.
///
/// # Implementation Note
///
/// This reads directly from the redb table via the `StackTxnT` trait's
/// `iter_stack_graph_adjacent` method. Since we don't have a dedicated
/// "list vertices" method, we use the range scan on the multimap table.
/// We access the table through the inner transaction's ability to read
/// STACK_GRAPH entries.
///
/// For now, we use a trait method that returns vertex+edge pairs and
/// extract just the vertex coordinates. A future optimization could add
/// a dedicated `iter_stack_graph_vertices` method to `StackTxnT`.
pub(crate) fn collect_stack_graph_vertices_for_change<T: StackTxnT>(
    txn: &T,
    stack_id: u64,
    change_id: u64,
) -> PristineResult<Vec<(u64, u64)>> {
    // We need to scan the STACK_GRAPH table for all keys matching
    // (stack_id, change_id, *, *). Since iter_stack_graph_adjacent requires
    // a specific vertex, we need a different approach.
    //
    // We use a range scan helper that's available through the StackTxnT
    // trait. For this, we add a targeted scan method. But to avoid expanding
    // the trait surface in this phase, we'll use a workaround: scan the
    // STACK_GRAPH range directly.
    //
    // The OverlayTxn has access to `inner: &T` where T: StackTxnT + GraphTxnT.
    // We can call `iter_stack_graph_vertices_for_change` if it exists, or
    // use the raw table scan pattern.
    //
    // Since we need to keep Phase 3 self-contained, we'll add a helper
    // method to StackTxnT with a default implementation.
    txn.iter_stack_graph_vertices_for_change(stack_id, change_id)
}

// ---------------------------------------------------------------------------
// TreeTxnT delegation (tree operations are global, no overlay needed)
// ---------------------------------------------------------------------------

impl<'a, T: TreeTxnT + StackTxnT> TreeTxnT for OverlayTxn<'a, T> {
    fn get_inode(&self, path: &str) -> PristineResult<Option<Inode>> {
        self.inner.get_inode(path)
    }

    fn get_directory_flags(&self, inode: Inode) -> PristineResult<Option<u8>> {
        self.inner.get_directory_flags(inode)
    }

    fn get_path(&self, inode: Inode) -> PristineResult<Option<String>> {
        self.inner.get_path(inode)
    }

    fn inode_position(
        &self,
        inode: Inode,
    ) -> PristineResult<Option<crate::types::Position<NodeId>>> {
        self.inner.inode_position(inode)
    }

    fn position_inode(&self, pos: crate::types::Position<NodeId>) -> PristineResult<Option<Inode>> {
        self.inner.position_inode(pos)
    }

    fn iter_tree(
        &self,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>> {
        self.inner.iter_tree()
    }

    fn iter_inode_vertices(
        &self,
        inode: Inode,
    ) -> PristineResult<
        Box<
            dyn Iterator<Item = Result<(GraphNode<NodeId>, SerializedGraphEdge), PristineError>>
                + '_,
        >,
    > {
        self.inner.iter_inode_vertices(inode)
    }

    fn get_file_mtime(&self, path: &str) -> PristineResult<Option<(i64, u32, u64)>> {
        self.inner.get_file_mtime(path)
    }
}

// ---------------------------------------------------------------------------
// StackTxnT delegation (stack operations are global, no overlay needed)
// ---------------------------------------------------------------------------

impl<'a, T: GraphTxnT + StackTxnT> StackTxnT for OverlayTxn<'a, T> {
    fn get_stack_by_id(&self, id: u64) -> PristineResult<Option<StackState>> {
        self.inner.get_stack_by_id(id)
    }

    fn iter_stack_graph_adjacent(
        &self,
        stack_id: u64,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<SerializedGraphEdge, PristineError>> + '_>>
    {
        self.inner
            .iter_stack_graph_adjacent(stack_id, node, min_flag, max_flag)
    }

    fn iter_stack_graph_vertices_for_change(
        &self,
        stack_id: u64,
        change_id: u64,
    ) -> PristineResult<Vec<(u64, u64)>> {
        self.inner
            .iter_stack_graph_vertices_for_change(stack_id, change_id)
    }

    fn get_stack(&self, name: &str) -> PristineResult<Option<StackState>> {
        self.inner.get_stack(name)
    }

    fn list_stacks(&self) -> PristineResult<Vec<String>> {
        self.inner.list_stacks()
    }

    fn get_change_seq(&self, stack: &StackState, change_id: NodeId) -> PristineResult<Option<u64>> {
        self.inner.get_change_seq(stack, change_id)
    }

    fn get_change_at_seq(&self, stack: &StackState, seq: u64) -> PristineResult<Option<NodeId>> {
        self.inner.get_change_at_seq(stack, seq)
    }

    fn iter_changes(
        &self,
        stack: &StackState,
        from_seq: u64,
    ) -> PristineResult<Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>>
    {
        self.inner.iter_changes(stack, from_seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::{MutTxnT, Pristine, StackKind, StackState, StackTxnT};
    use crate::types::Position;
    use tempfile::tempdir;

    // -- Helpers -----------------------------------------------------------

    fn open_pristine() -> (tempfile::TempDir, Pristine) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();
        (dir, pristine)
    }

    fn make_edge(
        flag: EdgeFlags,
        dest_change: u64,
        dest_pos: u64,
        introduced_by: u64,
    ) -> SerializedGraphEdge {
        SerializedGraphEdge::new(
            flag,
            Position::new(NodeId::new(dest_change), ChangePosition::new(dest_pos)),
            NodeId::new(introduced_by),
        )
    }

    fn make_vertex(change: u64, start: u64, end: u64) -> GraphNode<NodeId> {
        GraphNode::new(
            NodeId::new(change),
            ChangePosition::new(start),
            ChangePosition::new(end),
        )
    }

    fn collect_edges<T: GraphTxnT>(txn: &T, vertex: GraphNode<NodeId>) -> Vec<SerializedGraphEdge> {
        txn.iter_adjacent(vertex, EdgeFlags::empty(), EdgeFlags::all())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    // -- Tests: empty overlay (shared stack behavior) ----------------------

    #[test]
    fn empty_chain_delegates_to_inner() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let v = make_vertex(1, 0, 10);
        let e = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
        txn.put_graph(v, e).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let overlay = OverlayTxn::new(&txn, vec![]);

        let edges = collect_edges(&overlay, v);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].dest().change, NodeId::new(2));
    }

    // -- Tests: single local workspace overlay ------------------------------

    #[test]
    fn overlay_sees_stack_graph_edges() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();

        let v = make_vertex(1, 0, 10);
        let stack_edge = make_edge(EdgeFlags::BLOCK, 10, 0, 100);
        txn.put_stack_graph(feature.id, v, stack_edge).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let chain = txn
            .resolve_overlay_chain(&txn.get_stack("feature").unwrap().unwrap())
            .unwrap();
        let overlay = OverlayTxn::new(&txn, chain);

        let edges = collect_edges(&overlay, v);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].dest().change, NodeId::new(10));
    }

    #[test]
    fn overlay_unions_stack_graph_and_global_graph() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();

        let v = make_vertex(1, 0, 10);
        let global_edge = make_edge(EdgeFlags::BLOCK, 50, 0, 500);
        let stack_edge = make_edge(EdgeFlags::BLOCK, 60, 0, 600);

        txn.put_graph(v, global_edge).unwrap();
        txn.put_stack_graph(feature.id, v, stack_edge).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let overlay = OverlayTxn::new(&txn, vec![feature.id]);

        let edges = collect_edges(&overlay, v);
        assert_eq!(edges.len(), 2);

        let dest_changes: HashSet<u64> = edges.iter().map(|e| e.dest().change.get()).collect();
        assert!(dest_changes.contains(&50));
        assert!(dest_changes.contains(&60));
    }

    #[test]
    fn overlay_deduplicates_same_edge_across_layers() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();

        let v = make_vertex(1, 0, 10);
        let edge = make_edge(EdgeFlags::BLOCK, 50, 0, 500);

        // Same edge in both GRAPH and STACK_GRAPH
        txn.put_graph(v, edge).unwrap();
        txn.put_stack_graph(feature.id, v, edge).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let overlay = OverlayTxn::new(&txn, vec![feature.id]);

        let edges = collect_edges(&overlay, v);
        assert_eq!(edges.len(), 1, "duplicate edge should be deduplicated");
    }

    // -- Tests: stacked isolated overlay -----------------------------------

    #[test]
    fn overlay_unions_stacked_isolated_chains() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let service = txn
            .create_stack("service-auth", StackKind::Local, Some(dev.id))
            .unwrap();
        let feature = txn
            .create_stack("feature-login", StackKind::Local, Some(service.id))
            .unwrap();

        let v = make_vertex(1, 0, 10);

        let global_edge = make_edge(EdgeFlags::BLOCK, 10, 0, 1);
        let service_edge = make_edge(EdgeFlags::BLOCK, 20, 0, 2);
        let feature_edge = make_edge(EdgeFlags::BLOCK, 30, 0, 3);

        txn.put_graph(v, global_edge).unwrap();
        txn.put_stack_graph(service.id, v, service_edge).unwrap();
        txn.put_stack_graph(feature.id, v, feature_edge).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let stack = txn.get_stack("feature-login").unwrap().unwrap();
        let chain = txn.resolve_overlay_chain(&stack).unwrap();
        assert_eq!(chain, vec![feature.id, service.id]);

        let overlay = OverlayTxn::new(&txn, chain);
        let edges = collect_edges(&overlay, v);
        assert_eq!(edges.len(), 3);

        let dest_changes: HashSet<u64> = edges.iter().map(|e| e.dest().change.get()).collect();
        assert!(dest_changes.contains(&10)); // global
        assert!(dest_changes.contains(&20)); // service-auth
        assert!(dest_changes.contains(&30)); // feature-login
    }

    // -- Tests: has_vertex through overlay ---------------------------------

    #[test]
    fn has_vertex_finds_stack_graph_only_vertex() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();

        let v = make_vertex(1, 0, 10);
        let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);

        // Only in STACK_GRAPH, not in GRAPH
        txn.put_stack_graph(feature.id, v, edge).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();

        // Without overlay: vertex not found
        assert!(!txn.has_vertex(v).unwrap());

        // With overlay: vertex found
        let overlay = OverlayTxn::new(&txn, vec![feature.id]);
        assert!(overlay.has_vertex(v).unwrap());
    }

    // -- Tests: find_block through overlay ---------------------------------

    #[test]
    fn find_block_finds_vertex_in_stack_graph() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();

        // Vertex V(1, 0, 20) only in STACK_GRAPH
        let v = make_vertex(1, 0, 20);
        let fwd = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
        let rev = make_edge(EdgeFlags::BLOCK | EdgeFlags::PARENT, 1, 0, 1);
        txn.put_stack_graph(feature.id, v, fwd).unwrap();
        txn.put_stack_graph(feature.id, v, rev).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let overlay = OverlayTxn::new(&txn, vec![feature.id]);

        // Position 5 is within V(1, 0, 20), should find the vertex
        let pos = Position::new(NodeId::new(1), ChangePosition::new(5));
        let found = overlay.find_block(pos).unwrap();
        assert_eq!(found.change, NodeId::new(1));
        assert_eq!(found.start, ChangePosition::new(0));
        assert_eq!(found.end, ChangePosition::new(20));
    }

    #[test]
    fn find_block_prefers_global_when_no_stack_match() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();

        // Vertex only in GRAPH (not in STACK_GRAPH for feature)
        let v = make_vertex(1, 0, 20);
        let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
        txn.put_graph(v, edge).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let overlay = OverlayTxn::new(&txn, vec![feature.id]);

        let pos = Position::new(NodeId::new(1), ChangePosition::new(5));
        let found = overlay.find_block(pos).unwrap();
        assert_eq!(found.start, ChangePosition::new(0));
        assert_eq!(found.end, ChangePosition::new(20));
    }

    #[test]
    fn find_block_root_always_works() {
        let (_dir, pristine) = open_pristine();
        let txn = pristine.read_txn().unwrap();
        let overlay = OverlayTxn::new(&txn, vec![42]); // non-existent stack OK

        let pos = Position::new(NodeId::ROOT, ChangePosition::ROOT);
        let found = overlay.find_block(pos).unwrap();
        assert!(found.is_root());
    }

    // -- Tests: find_block_end through overlay -----------------------------

    #[test]
    fn find_block_end_finds_empty_vertex_in_stack_graph() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();

        // Empty inode vertex V(1, 9, 9) only in STACK_GRAPH
        let inode_v = make_vertex(1, 9, 9);
        let edge = make_edge(EdgeFlags::BLOCK, 2, 0, 1);
        txn.put_stack_graph(feature.id, inode_v, edge).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let overlay = OverlayTxn::new(&txn, vec![feature.id]);

        let pos = Position::new(NodeId::new(1), ChangePosition::new(9));
        let found = overlay.find_block_end(pos).unwrap();
        assert_eq!(found.start, ChangePosition::new(9));
        assert_eq!(found.end, ChangePosition::new(9));
    }

    // -- Tests: pass-through methods ---------------------------------------

    #[test]
    fn get_external_passes_through() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let hash = Hash::of(b"test");
        let id = txn.register_change(&hash).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let overlay = OverlayTxn::new(&txn, vec![42]);

        let found = overlay.get_external(id).unwrap();
        assert_eq!(found, Some(hash));
    }

    // -- Tests: from_stack convenience constructor -------------------------

    #[test]
    fn from_stack_resolves_chain() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        let dev = txn.create_stack("dev", StackKind::Shared, None).unwrap();
        let feature = txn
            .create_stack("feature", StackKind::Local, Some(dev.id))
            .unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let stack = txn.get_stack("feature").unwrap().unwrap();
        let overlay = OverlayTxn::from_stack(&txn, &stack).unwrap();

        assert!(overlay.has_overlay());
        assert_eq!(overlay.stack_chain(), &[feature.id]);
    }

    #[test]
    fn from_stack_shared_has_no_overlay() {
        let (_dir, pristine) = open_pristine();
        let mut txn = pristine.write_txn().unwrap();

        txn.create_stack("dev", StackKind::Shared, None).unwrap();
        txn.commit().unwrap();

        let txn = pristine.read_txn().unwrap();
        let stack = txn.get_stack("dev").unwrap().unwrap();
        let overlay = OverlayTxn::from_stack(&txn, &stack).unwrap();

        assert!(!overlay.has_overlay());
        assert!(overlay.stack_chain().is_empty());
    }
}

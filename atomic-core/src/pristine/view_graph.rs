//! View-scoped graph wrapper that filters edge traversal by visibility.
//!
//! `ViewGraph` wraps any `GraphTxnT` implementor and a validated
//! [`GraphVisibilityClosure`]. When iterating adjacent edges, only edges whose
//! `introduced_by` is in the closure (or is ROOT) are returned.
//!
//! Position lookups (`find_block`, `find_block_end`) are NOT filtered
//! because they are structural — a vertex exists at a position regardless
//! of which view introduced edges to it.
//!
//! This replaces the old `OverlayTxn` which unioned `STACK_GRAPH` with
//! `GRAPH`. In the ambient graph model, there is only `GRAPH`, and
//! `ViewGraph` controls which edges are visible per-view.

use crate::pristine::{
    FileIndexEntry, FileIndexMetadata, GraphTxnT, GraphVisibilityClosure, InodeAdjState,
    InodeGraphOps, PristineError, StoredConflict, TreeTxnT, ViewState, ViewTxnT,
};
use crate::types::{
    EdgeFlags, GraphNode, Hash, Inode, Merkle, NodeId, Position, SerializedGraphEdge,
};

/// A view-scoped graph wrapper that filters edge traversal by visibility.
///
/// `ViewGraph` wraps any `GraphTxnT` implementor and a validated visibility
/// closure. When iterating adjacent edges, only edges whose `introduced_by` is
/// in the closure (or is ROOT) are returned.
///
/// Position lookups (`find_block`, `find_block_end`) are NOT filtered
/// because they are structural — a vertex exists at a position regardless
/// of which view introduced edges to it.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::pristine::{GraphVisibilityClosure, ViewGraph};
///
/// let membership = txn.view_membership_set(&view)?;
/// let visibility = GraphVisibilityClosure::try_from_membership(&txn, &membership)?;
/// let vg = ViewGraph::new(&txn, visibility);
///
/// // iter_adjacent now only returns edges from the view's changes
/// let edges = vg.iter_adjacent(node, min_flag, max_flag)?;
/// ```
pub struct ViewGraph<'a, T> {
    inner: &'a T,
    visibility: GraphVisibilityClosure,
}

impl<'a, T> ViewGraph<'a, T> {
    /// Create a new view-scoped graph wrapper.
    ///
    /// # Arguments
    ///
    /// * `inner` - The underlying transaction implementing `GraphTxnT`
    /// * `visibility` - Validated dependency closure whose edges are visible
    pub fn new(inner: &'a T, visibility: GraphVisibilityClosure) -> Self {
        Self { inner, visibility }
    }

    /// Get a reference to the inner transaction.
    pub fn inner(&self) -> &T {
        self.inner
    }

    /// Check whether a change is visible in this view.
    ///
    /// ROOT is always visible regardless of the filter set.
    #[cfg(test)]
    fn is_visible(&self, change_id: NodeId) -> bool {
        change_id == NodeId::ROOT || self.visibility.contains(change_id)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// InodeGraphOps — delegate to inner txn (INODE_GRAPH is unfiltered)
// ─────────────────────────────────────────────────────────────────────────

impl<'a, T: InodeGraphOps> InodeGraphOps for ViewGraph<'a, T> {
    type InodeError = T::InodeError;

    fn init_inode_adj(
        &self,
        inode: Inode,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<InodeAdjState, Self::InodeError> {
        self.inner.init_inode_adj(inode, node, min_flag, max_flag)
    }

    fn next_inode_adj(
        &self,
        adj: &mut InodeAdjState,
    ) -> Option<Result<SerializedGraphEdge, Self::InodeError>> {
        if adj.is_exhausted() {
            return None;
        }

        // Filter inode edges by the view's visible change set,
        // same as iter_adjacent does for the global GRAPH.
        loop {
            match self.inner.next_inode_adj(adj) {
                Some(Ok(edge)) => {
                    let introduced = edge.introduced_by();
                    if introduced.is_root() || self.visibility.contains(introduced) {
                        return Some(Ok(edge));
                    }
                    // Edge from a non-visible change — skip it
                    continue;
                }
                Some(Err(error)) => {
                    adj.mark_exhausted();
                    return Some(Err(error));
                }
                None => return None,
            }
        }
    }

    fn find_block_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        self.inner.find_block_in_inode(inode, pos)
    }

    fn find_block_end_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        self.inner.find_block_end_in_inode(inode, pos)
    }

    fn count_inode_vertices(&self, inode: Inode) -> Result<usize, Self::InodeError> {
        self.inner.count_inode_vertices(inode)
    }

    fn inode_graph_is_populated(&self, inode: Inode) -> Result<bool, Self::InodeError> {
        self.inner.inode_graph_is_populated(inode)
    }

    fn inode_graph_needs_view_filter(&self) -> bool {
        // ViewGraph now filters inode edges in next_inode_adj,
        // so callers can safely use the INODE_GRAPH fast path.
        false
    }
}

/// Filtered adjacency iterator that only yields edges from visible changes.
pub struct FilteredAdj<I> {
    inner: I,
    visibility: GraphVisibilityClosure,
    exhausted: bool,
}

impl<I> Iterator for FilteredAdj<I>
where
    I: Iterator<Item = Result<SerializedGraphEdge, PristineError>>,
{
    type Item = Result<SerializedGraphEdge, PristineError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.exhausted {
            return None;
        }

        loop {
            match self.inner.next() {
                Some(Ok(edge)) => {
                    let introduced_by = edge.introduced_by();
                    if introduced_by == NodeId::ROOT || self.visibility.contains(introduced_by) {
                        return Some(Ok(edge));
                    }
                    // Skip edges not visible in this view
                    continue;
                }
                Some(Err(error)) => {
                    self.exhausted = true;
                    return Some(Err(error));
                }
                None => {
                    self.exhausted = true;
                    return None;
                }
            }
        }
    }
}

impl<'a, T: GraphTxnT> GraphTxnT for ViewGraph<'a, T> {
    type Adj = FilteredAdj<T::Adj>;

    /// Iterate adjacent edges, filtering by visibility.
    ///
    /// Only edges whose `introduced_by` is ROOT or is in the visible set
    /// are yielded. All other edges are silently skipped.
    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<Self::Adj, PristineError> {
        let inner_iter = self.inner.iter_adjacent(node, min_flag, max_flag)?;
        Ok(FilteredAdj {
            inner: inner_iter,
            visibility: self.visibility.clone(),
            exhausted: false,
        })
    }

    /// Structural lookup — no filtering. Delegates to inner.
    fn find_block(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
        self.inner.find_block(pos)
    }

    /// Structural lookup — no filtering. Delegates to inner.
    fn find_block_end(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
        self.inner.find_block_end(pos)
    }

    /// Structural check — no filtering. Delegates to inner.
    fn has_vertex(&self, node: GraphNode<NodeId>) -> Result<bool, PristineError> {
        self.inner.has_vertex(node)
    }

    /// ID mapping — no filtering. Delegates to inner.
    fn get_external(&self, id: NodeId) -> Result<Option<Hash>, PristineError> {
        self.inner.get_external(id)
    }

    /// ID mapping — no filtering. Delegates to inner.
    fn get_internal(&self, hash: &Hash) -> Result<Option<NodeId>, PristineError> {
        self.inner.get_internal(hash)
    }

    /// Registered change listing — no filtering. Delegates to inner.
    fn list_registered_changes(&self) -> Result<Vec<(NodeId, Hash)>, PristineError> {
        self.inner.list_registered_changes()
    }

    fn is_change_visible(&self, id: NodeId) -> bool {
        id.is_root() || self.visibility.contains(id)
    }

    /// Node type lookup — no filtering. Delegates to inner.
    fn get_node_type(&self, node_id: NodeId) -> Result<Option<u8>, PristineError> {
        self.inner.get_node_type(node_id)
    }

    /// Reverse dependency lookup — no filtering. Delegates to inner.
    fn get_rev_deps(&self, dep_id: NodeId) -> Result<Vec<NodeId>, PristineError> {
        self.inner.get_rev_deps(dep_id)
    }

    /// Indexed normal change dependency lookup — no filtering. Delegates to inner.
    fn get_change_deps(&self, change_id: NodeId) -> Result<Vec<Hash>, PristineError> {
        self.inner.get_change_deps(change_id)
    }

    /// Indexed dependency count lookup — no filtering. Delegates to inner.
    fn change_deps_indexed_count(&self, change_id: NodeId) -> Result<Option<u64>, PristineError> {
        self.inner.change_deps_indexed_count(change_id)
    }

    /// Reverse indexed normal change dependency lookup — no filtering. Delegates to inner.
    fn get_rev_change_deps(&self, dep_hash: &Hash) -> Result<Vec<NodeId>, PristineError> {
        self.inner.get_rev_change_deps(dep_hash)
    }

    /// Graph presence check — no filtering. Delegates to inner.
    fn has_change_in_graph(&self, change_id: NodeId) -> Result<bool, PristineError> {
        self.inner.has_change_in_graph(change_id)
    }
}

impl<'a, T: ViewTxnT> ViewTxnT for ViewGraph<'a, T> {
    fn get_view_by_id(&self, id: u64) -> Result<Option<ViewState>, PristineError> {
        self.inner.get_view_by_id(id)
    }

    fn get_conflicts(
        &self,
        view_id: u64,
        inode: u64,
    ) -> Result<Vec<StoredConflict>, PristineError> {
        self.inner.get_conflicts(view_id, inode)
    }

    fn iter_conflicts(
        &self,
        view_id: u64,
    ) -> Result<Vec<(u64, Vec<StoredConflict>)>, PristineError> {
        self.inner.iter_conflicts(view_id)
    }

    fn snapshot_conflicts(&self) -> Result<Vec<(u64, Inode, Vec<StoredConflict>)>, PristineError> {
        self.inner.snapshot_conflicts()
    }

    fn get_view(&self, name: &str) -> Result<Option<ViewState>, PristineError> {
        self.inner.get_view(name)
    }

    fn snapshot_views(&self) -> Result<Vec<(String, ViewState)>, PristineError> {
        self.inner.snapshot_views()
    }

    fn list_views(&self) -> Result<Vec<String>, PristineError> {
        self.inner.list_views()
    }

    fn get_change_seq(
        &self,
        view: &ViewState,
        change_id: NodeId,
    ) -> Result<Option<u64>, PristineError> {
        self.inner.get_change_seq(view, change_id)
    }

    fn get_change_at_seq(
        &self,
        view: &ViewState,
        seq: u64,
    ) -> Result<Option<NodeId>, PristineError> {
        self.inner.get_change_at_seq(view, seq)
    }

    fn iter_changes(
        &self,
        view: &ViewState,
        from_seq: u64,
    ) -> Result<
        Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>,
        PristineError,
    > {
        self.inner.iter_changes(view, from_seq)
    }
}

impl<'a, T: TreeTxnT> TreeTxnT for ViewGraph<'a, T> {
    fn get_inode(&self, path: &str) -> Result<Option<Inode>, PristineError> {
        self.inner.get_inode(path)
    }

    fn get_directory_flags(&self, inode: Inode) -> Result<Option<u8>, PristineError> {
        self.inner.get_directory_flags(inode)
    }

    fn get_path(&self, inode: Inode) -> Result<Option<String>, PristineError> {
        self.inner.get_path(inode)
    }

    fn inode_position(&self, inode: Inode) -> Result<Option<Position<NodeId>>, PristineError> {
        self.inner.inode_position(inode)
    }

    fn position_inode(&self, pos: Position<NodeId>) -> Result<Option<Inode>, PristineError> {
        self.inner.position_inode(pos)
    }

    fn snapshot_inodes(&self) -> Result<Vec<(Inode, Position<NodeId>)>, PristineError> {
        self.inner.snapshot_inodes()
    }

    fn snapshot_rev_inodes(&self) -> Result<Vec<(Position<NodeId>, Inode)>, PristineError> {
        self.inner.snapshot_rev_inodes()
    }

    fn snapshot_directories(&self) -> Result<Vec<(Inode, u8)>, PristineError> {
        self.inner.snapshot_directories()
    }

    fn snapshot_inode_graph_keys(&self) -> Result<Vec<(Inode, GraphNode<NodeId>)>, PristineError> {
        self.inner.snapshot_inode_graph_keys()
    }

    fn iter_tree(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>, PristineError>
    {
        self.inner.iter_tree()
    }

    fn iter_inode_vertices(
        &self,
        inode: Inode,
    ) -> Result<
        Box<
            dyn Iterator<Item = Result<(GraphNode<NodeId>, SerializedGraphEdge), PristineError>>
                + '_,
        >,
        PristineError,
    > {
        self.inner.iter_inode_vertices(inode)
    }

    fn get_file_index(&self, path: &str) -> Result<Option<FileIndexMetadata>, PristineError> {
        self.inner.get_file_index(path)
    }

    fn iter_file_index(&self) -> Result<Vec<FileIndexEntry>, PristineError> {
        self.inner.iter_file_index()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// CrdtTxnT — delegate to inner txn (CRDT tables are global, not filtered)
// ─────────────────────────────────────────────────────────────────────────

impl<'a, T: crate::pristine::CrdtTxnT> crate::pristine::CrdtTxnT for ViewGraph<'a, T> {
    fn get_crdt_trunk(
        &self,
        key: &[u8; 12],
    ) -> Result<Option<crate::crdt::tables::SerializedTrunk>, PristineError> {
        self.inner.get_crdt_trunk(key)
    }

    fn get_crdt_inode_trunk(&self, inode: u64) -> Result<Option<[u8; 12]>, PristineError> {
        self.inner.get_crdt_inode_trunk(inode)
    }

    fn get_crdt_branch(
        &self,
        key: &[u8; 12],
    ) -> Result<Option<crate::crdt::tables::SerializedBranch>, PristineError> {
        self.inner.get_crdt_branch(key)
    }

    fn get_crdt_branch_after(
        &self,
        branch_key: &[u8; 12],
    ) -> Result<Option<[u8; 12]>, PristineError> {
        self.inner.get_crdt_branch_after(branch_key)
    }

    fn get_crdt_leaf(
        &self,
        key: &[u8; 12],
    ) -> Result<Option<crate::crdt::tables::SerializedLeaf>, PristineError> {
        self.inner.get_crdt_leaf(key)
    }

    fn get_trunk_by_path(&self, path: &str) -> Result<Option<crate::crdt::TrunkId>, PristineError> {
        self.inner.get_trunk_by_path(path)
    }

    fn iter_trunk_branches(
        &self,
        trunk_key: &[u8; 12],
    ) -> Result<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>, PristineError> {
        self.inner.iter_trunk_branches(trunk_key)
    }

    fn iter_branch_leaves(
        &self,
        branch_key: &[u8; 12],
    ) -> Result<Box<dyn Iterator<Item = Result<[u8; 12], PristineError>> + '_>, PristineError> {
        self.inner.iter_branch_leaves(branch_key)
    }

    fn get_crdt_branch_vertex(
        &self,
        branch_key: &[u8; 12],
    ) -> Result<Option<crate::types::GraphNode<crate::types::NodeId>>, PristineError> {
        self.inner.get_crdt_branch_vertex(branch_key)
    }

    fn get_crdt_vertex_branch(
        &self,
        vertex_key: &[u8; 24],
    ) -> Result<Option<crate::crdt::BranchId>, PristineError> {
        self.inner.get_crdt_vertex_branch(vertex_key)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// InodeAttrTxnT — register events are view-scoped (review CB-9C R1)
// ─────────────────────────────────────────────────────────────────────────

impl<'a, T: crate::pristine::InodeAttrTxnT> crate::pristine::InodeAttrTxnT for ViewGraph<'a, T> {
    /// Only register events introduced by changes visible in THIS view.
    ///
    /// The POSITION_ATTRS/INODE_ATTRS tables are global, so the raw read
    /// returns every sibling's writers. A change assembled on this view must
    /// not import invisible sibling causality: an event written by a change
    /// outside the view's closure is not an ancestor of the new change, and
    /// wiring it as a dependency leaks foreign causality into the assembly
    /// (review CB-9C R1). Event writers can never be ROOT
    /// ([`InodeAttrEvent::new`] rejects it), so the closure check alone is
    /// exact.
    fn get_inode_attr_events(
        &self,
        position: Position<NodeId>,
        name: crate::change::InodeAttrName,
    ) -> Result<Vec<crate::pristine::InodeAttrEvent>, PristineError> {
        Ok(self
            .inner
            .get_inode_attr_events(position, name)?
            .into_iter()
            .filter(|event| self.visibility.contains(event.introduced_by))
            .collect())
    }

    fn get_inode_attr_events_by_inode(
        &self,
        inode: Inode,
        name: crate::change::InodeAttrName,
    ) -> Result<Vec<crate::pristine::InodeAttrEvent>, PristineError> {
        Ok(self
            .inner
            .get_inode_attr_events_by_inode(inode, name)?
            .into_iter()
            .filter(|event| self.visibility.contains(event.introduced_by))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;
    use std::cell::Cell;

    fn visible_edge() -> SerializedGraphEdge {
        SerializedGraphEdge::new(
            EdgeFlags::BLOCK,
            Position::new(NodeId::ROOT, ChangePosition::new(0)),
            NodeId::ROOT,
        )
    }

    #[test]
    fn filtered_adj_is_terminal_after_underlying_error() {
        let error = PristineError::BlockNotFound { change: 1, pos: 0 };
        let mut adj = FilteredAdj {
            inner: vec![Err(error), Ok(visible_edge())].into_iter(),
            visibility: GraphVisibilityClosure::empty(),
            exhausted: false,
        };

        assert!(matches!(adj.next(), Some(Err(_))));
        assert!(adj.next().is_none());
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ScriptedInodeError;

    impl std::fmt::Display for ScriptedInodeError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("scripted inode error")
        }
    }

    impl std::error::Error for ScriptedInodeError {}

    struct ScriptedInodeTxn {
        calls: Cell<usize>,
    }

    impl InodeGraphOps for ScriptedInodeTxn {
        type InodeError = ScriptedInodeError;

        fn init_inode_adj(
            &self,
            inode: Inode,
            node: GraphNode<NodeId>,
            min_flag: EdgeFlags,
            max_flag: EdgeFlags,
        ) -> Result<InodeAdjState, Self::InodeError> {
            Ok(InodeAdjState::new(inode, node, min_flag, max_flag))
        }

        fn next_inode_adj(
            &self,
            _adj: &mut InodeAdjState,
        ) -> Option<Result<SerializedGraphEdge, Self::InodeError>> {
            let call = self.calls.get();
            self.calls.set(call + 1);
            if call == 0 {
                Some(Err(ScriptedInodeError))
            } else {
                Some(Ok(visible_edge()))
            }
        }

        fn find_block_in_inode(
            &self,
            _inode: Inode,
            _pos: Position<NodeId>,
        ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
            Ok(None)
        }

        fn count_inode_vertices(&self, _inode: Inode) -> Result<usize, Self::InodeError> {
            Ok(0)
        }
    }

    #[test]
    fn filtered_inode_adj_marks_state_terminal_after_underlying_error() {
        let inner = ScriptedInodeTxn {
            calls: Cell::new(0),
        };
        let graph = ViewGraph::new(&inner, GraphVisibilityClosure::empty());
        let mut adj = graph
            .init_inode_adj(
                Inode::new(1),
                GraphNode::ROOT,
                EdgeFlags::empty(),
                EdgeFlags::all(),
            )
            .unwrap();

        assert!(matches!(graph.next_inode_adj(&mut adj), Some(Err(_))));
        assert!(adj.is_exhausted());
        assert!(graph.next_inode_adj(&mut adj).is_none());
        assert_eq!(inner.calls.get(), 1);
    }

    #[test]
    fn test_is_visible_root_always_visible() {
        // We can't easily construct a full GraphTxnT mock here, but we can
        // test the is_visible logic directly.
        struct DummyTxn;

        let vg: ViewGraph<'_, DummyTxn> = ViewGraph {
            inner: &DummyTxn,
            visibility: GraphVisibilityClosure::empty(),
        };

        // ROOT is always visible even with an empty filter
        assert!(vg.is_visible(NodeId::ROOT));
    }

    #[test]
    fn test_is_visible_checks_set() {
        struct DummyTxn;

        let visibility =
            GraphVisibilityClosure::from_ordered_unchecked([NodeId::new(42), NodeId::new(99)]);

        let vg: ViewGraph<'_, DummyTxn> = ViewGraph {
            inner: &DummyTxn,
            visibility,
        };

        assert!(vg.is_visible(NodeId::new(42)));
        assert!(vg.is_visible(NodeId::new(99)));
        assert!(!vg.is_visible(NodeId::new(1)));
        assert!(!vg.is_visible(NodeId::new(100)));
        // ROOT is always visible
        assert!(vg.is_visible(NodeId::ROOT));
    }

    #[test]
    fn test_inner_returns_reference() {
        struct DummyTxn(u32);

        let txn = DummyTxn(123);
        let vg = ViewGraph {
            inner: &txn,
            visibility: GraphVisibilityClosure::empty(),
        };

        assert_eq!(vg.inner().0, 123);
    }

    /// Review CB-9C R1: register events read through a view-scoped graph are
    /// exactly the events introduced by the view's visible changes. An
    /// invisible sibling chmod must never appear — otherwise a change
    /// assembled on this view would import foreign sibling causality as a
    /// dependency.
    #[test]
    fn register_events_are_visible_only_inside_the_view_closure() {
        use crate::change::InodeAttr;
        use crate::pristine::{
            InodeAttrEvent, InodeAttrMutTxnT, InodeAttrTxnT, MutTxnT, Pristine,
        };

        let temp = tempfile::tempdir().unwrap();
        let pristine = Pristine::open(temp.path().join("vg-attrs.redb")).unwrap();
        let h1 = Hash::of(b"vg base change");
        let h2 = Hash::of(b"vg left sibling chmod");
        let h3 = Hash::of(b"vg right sibling chmod");
        let inode = Inode::new(11);
        let position = Position::new(NodeId::new(5), ChangePosition::new(3));
        let (id1, id2, id3) = {
            let mut txn = pristine.write_txn().unwrap();
            let id1 = txn.register_change(&h1).unwrap();
            let id2 = txn.register_change(&h2).unwrap();
            let id3 = txn.register_change(&h3).unwrap();
            txn.put_change_deps(id1, &[]).unwrap();
            txn.put_change_deps(id2, &[h1]).unwrap();
            txn.put_change_deps(id3, &[]).unwrap();
            txn.put_inode_attr_event(
                inode,
                position,
                InodeAttrEvent::new(id1, InodeAttr::Mode(0o644)).unwrap(),
            )
            .unwrap();
            txn.put_inode_attr_event(
                inode,
                position,
                InodeAttrEvent::new(id2, InodeAttr::Mode(0o755)).unwrap(),
            )
            .unwrap();
            txn.put_inode_attr_event(
                inode,
                position,
                InodeAttrEvent::new(id3, InodeAttr::Mode(0o700)).unwrap(),
            )
            .unwrap();
            txn.commit().unwrap();
            (id1, id2, id3)
        };

        let txn = pristine.read_txn().unwrap();

        // Base-only view: the register's visible state is the base writer
        // alone, even though the global table also holds both siblings.
        let base_only = GraphVisibilityClosure::from_ordered_unchecked([id1]);
        let view = ViewGraph::new(&txn, base_only);
        let events = view
            .get_inode_attr_events(position, crate::change::InodeAttrName::Mode)
            .unwrap();
        assert_eq!(
            events,
            vec![InodeAttrEvent {
                introduced_by: id1,
                value: InodeAttr::Mode(0o644)
            }],
            "an invisible sibling chmod must not surface in the view's register"
        );
        assert!(view
            .get_inode_attr_events_by_inode(inode, crate::change::InodeAttrName::Mode)
            .unwrap()
            .iter()
            .all(|event| event.introduced_by == id1));

        // A view that saw the left sibling sees base + left writers, never
        // the independent right sibling.
        let with_left = GraphVisibilityClosure::from_ordered_unchecked([id1, id2]);
        let view = ViewGraph::new(&txn, with_left);
        let writers: std::collections::HashSet<NodeId> = view
            .get_inode_attr_events(position, crate::change::InodeAttrName::Mode)
            .unwrap()
            .into_iter()
            .map(|event| event.introduced_by)
            .collect();
        assert_eq!(
            writers,
            std::iter::once(id1).chain(std::iter::once(id2)).collect(),
            "the view sees its own closure's writers only"
        );
        // Contrast: the unscoped transaction still holds all three global
        // rows — the fence is at the view boundary, not in the table.
        let all = txn
            .get_inode_attr_events(position, crate::change::InodeAttrName::Mode)
            .unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|event| event.introduced_by == id3));
    }
}

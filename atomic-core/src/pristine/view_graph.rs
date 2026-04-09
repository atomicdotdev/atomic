//! View-scoped graph wrapper that filters edge traversal by visibility.
//!
//! `ViewGraph` wraps any `GraphTxnT` implementor and a set of visible
//! change `NodeId`s. When iterating adjacent edges, only edges whose
//! `introduced_by` is in the visible set (or is ROOT) are returned.
//!
//! Position lookups (`find_block`, `find_block_end`) are NOT filtered
//! because they are structural — a vertex exists at a position regardless
//! of which view introduced edges to it.
//!
//! This replaces the old `OverlayTxn` which unioned `STACK_GRAPH` with
//! `GRAPH`. In the ambient graph model, there is only `GRAPH`, and
//! `ViewGraph` controls which edges are visible per-view.

use std::collections::HashSet;
use std::sync::Arc;

use crate::pristine::{GraphTxnT, InodeAdjState, InodeGraphOps, PristineError, TreeTxnT};
use crate::types::{EdgeFlags, GraphNode, Hash, Inode, NodeId, Position, SerializedGraphEdge};

/// A view-scoped graph wrapper that filters edge traversal by visibility.
///
/// `ViewGraph` wraps any `GraphTxnT` implementor and a set of visible
/// change `NodeId`s. When iterating adjacent edges, only edges whose
/// `introduced_by` is in the visible set (or is ROOT) are returned.
///
/// Position lookups (`find_block`, `find_block_end`) are NOT filtered
/// because they are structural — a vertex exists at a position regardless
/// of which view introduced edges to it.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::pristine::ViewGraph;
/// use std::sync::Arc;
/// use std::collections::HashSet;
///
/// let visible = Arc::new(collect_visible_change_ids(&txn, &view)?);
/// let vg = ViewGraph::new(&txn, visible);
///
/// // iter_adjacent now only returns edges from the view's changes
/// let edges = vg.iter_adjacent(node, min_flag, max_flag)?;
/// ```
pub struct ViewGraph<'a, T> {
    inner: &'a T,
    visible: Arc<HashSet<NodeId>>,
}

impl<'a, T> ViewGraph<'a, T> {
    /// Create a new view-scoped graph wrapper.
    ///
    /// # Arguments
    ///
    /// * `inner` - The underlying transaction implementing `GraphTxnT`
    /// * `visible` - Set of `NodeId`s whose edges should be visible
    pub fn new(inner: &'a T, visible: Arc<HashSet<NodeId>>) -> Self {
        Self { inner, visible }
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
        change_id == NodeId::ROOT || self.visible.contains(&change_id)
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
        self.inner.next_inode_adj(adj)
    }

    fn find_block_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        self.inner.find_block_in_inode(inode, pos)
    }

    fn count_inode_vertices(&self, inode: Inode) -> Result<usize, Self::InodeError> {
        self.inner.count_inode_vertices(inode)
    }

    fn inode_graph_is_populated(&self, inode: Inode) -> Result<bool, Self::InodeError> {
        self.inner.inode_graph_is_populated(inode)
    }
}

/// Filtered adjacency iterator that only yields edges from visible changes.
pub struct FilteredAdj<I> {
    inner: I,
    visible: Arc<HashSet<NodeId>>,
}

impl<I> Iterator for FilteredAdj<I>
where
    I: Iterator<Item = Result<SerializedGraphEdge, PristineError>>,
{
    type Item = Result<SerializedGraphEdge, PristineError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.inner.next() {
                Some(Ok(edge)) => {
                    let introduced_by = edge.introduced_by();
                    if introduced_by == NodeId::ROOT || self.visible.contains(&introduced_by) {
                        return Some(Ok(edge));
                    }
                    // Skip edges not visible in this view
                    continue;
                }
                Some(Err(e)) => return Some(Err(e)),
                None => return None,
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
            visible: Arc::clone(&self.visible),
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

    /// Node type lookup — no filtering. Delegates to inner.
    fn get_node_type(&self, node_id: NodeId) -> Result<Option<u8>, PristineError> {
        self.inner.get_node_type(node_id)
    }

    /// Reverse dependency lookup — no filtering. Delegates to inner.
    fn get_rev_deps(&self, dep_id: NodeId) -> Result<Vec<NodeId>, PristineError> {
        self.inner.get_rev_deps(dep_id)
    }

    /// Graph presence check — no filtering. Delegates to inner.
    fn has_change_in_graph(&self, change_id: NodeId) -> Result<bool, PristineError> {
        self.inner.has_change_in_graph(change_id)
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

    fn get_file_mtime(&self, path: &str) -> Result<Option<(i64, u32, u64)>, PristineError> {
        self.inner.get_file_mtime(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_visible_root_always_visible() {
        // We can't easily construct a full GraphTxnT mock here, but we can
        // test the is_visible logic directly.
        struct DummyTxn;

        let vg: ViewGraph<'_, DummyTxn> = ViewGraph {
            inner: &DummyTxn,
            visible: Arc::new(HashSet::new()),
        };

        // ROOT is always visible even with an empty filter
        assert!(vg.is_visible(NodeId::ROOT));
    }

    #[test]
    fn test_is_visible_checks_set() {
        struct DummyTxn;

        let mut visible = HashSet::new();
        visible.insert(NodeId::new(42));
        visible.insert(NodeId::new(99));

        let vg: ViewGraph<'_, DummyTxn> = ViewGraph {
            inner: &DummyTxn,
            visible: Arc::new(visible),
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
            visible: Arc::new(HashSet::new()),
        };

        assert_eq!(vg.inner().0, 123);
    }
}

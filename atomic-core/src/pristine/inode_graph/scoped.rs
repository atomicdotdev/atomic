//! Inode-scoped graph adapter for content retrieval.
//!
//! The global `GRAPH` B-tree stores every vertex from every file. The
//! byte-graph walk (`retrieve_graph`) resolves edge destinations with
//! `find_block` against that global tree, so on a large repository every
//! traversal step is a global lookup even though the walk never leaves one
//! file. `InodeScopedGraph` wraps any `GraphTxnT + InodeGraphOps + TreeTxnT`
//! transaction and resolves the structural operations the walk uses
//! (`iter_adjacent`, `find_block`, `find_block_end`) through the file-local
//! `INODE_GRAPH` index, falling back to the global lookup only when the inode
//! index cannot answer.
//!
//! The file's entire `INODE_GRAPH` adjacency is preloaded in ONE cursor pass
//! on construction and served from memory. Per-vertex adjacency cursors are
//! the dominant cost of the dead-chain walk on a heavily edited file (each
//! `init_inode_adj` repositions a B-tree cursor on the multi-GB database); one
//! contiguous range pass plus in-memory adjacency makes the walk CPU-bound.
//!
//! Correctness: when an inode's `INODE_GRAPH` rows are not populated the
//! adapter delegates every call to the inner transaction, so the result is
//! identical to the unscoped walk. When they are populated, the `INODE_GRAPH`
//! index is a faithful secondary index of the same edges (the write path
//! writes both unconditionally), and `find_block` still falls back to the
//! inode-local then global lookup if the in-memory vertex index misses.

use crate::pristine::{GraphTxnT, InodeGraphOps, PristineError, TreeTxnT};
use crate::types::{EdgeFlags, GraphNode, Hash, Inode, NodeId, Position, SerializedGraphEdge};

/// A graph transaction scoped to one inode for file-local traversal.
pub struct InodeScopedGraph<'a, T> {
    inner: &'a T,
    inode: Inode,
    /// Whether the inode's secondary index is populated. When false the
    /// adapter is a transparent delegate.
    scoped: bool,
    /// Preloaded outgoing edges per source vertex (INODE_GRAPH order).
    adjacency: std::collections::HashMap<GraphNode<NodeId>, Vec<SerializedGraphEdge>>,
    /// Sorted source vertices for in-memory `find_block`.
    vertices: Vec<GraphNode<NodeId>>,
}

impl<'a, T> InodeScopedGraph<'a, T>
where
    T: GraphTxnT + InodeGraphOps<InodeError = PristineError> + TreeTxnT,
{
    /// Wrap `inner` so structural lookups are attempted in `inode` first.
    pub fn new(inner: &'a T, inode: Inode) -> Result<Self, PristineError> {
        let scoped = inner.inode_graph_is_populated(inode)?;
        let mut adjacency: std::collections::HashMap<GraphNode<NodeId>, Vec<SerializedGraphEdge>> =
            std::collections::HashMap::new();
        let mut vertices: Vec<GraphNode<NodeId>> = Vec::new();
        if scoped {
            let iter = inner.iter_inode_vertices(inode)?;
            for entry in iter {
                let (node, edge) = entry?;
                if !adjacency.contains_key(&node) {
                    vertices.push(node);
                }
                adjacency.entry(node).or_default().push(edge);
            }
            vertices.sort_by(|left, right| {
                (left.change.get(), left.start.get(), left.end.get()).cmp(&(
                    right.change.get(),
                    right.start.get(),
                    right.end.get(),
                ))
            });
            vertices.dedup();
        }
        Ok(Self {
            inner,
            inode,
            scoped,
            adjacency,
            vertices,
        })
    }

    /// Whether the inode index is populated (and therefore scoping active).
    pub fn is_scoped(&self) -> bool {
        self.scoped
    }

    /// In-memory `find_block`: the vertex containing `pos`, preferring a
    /// non-empty vertex (matching `GraphTxnT::find_block`).
    fn find_block_cached(&self, pos: Position<NodeId>) -> Option<GraphNode<NodeId>> {
        let key = (pos.change.get(), pos.pos.get());
        // First index with change == pos.change and start <= pos.pos.
        let mut lo = 0usize;
        let mut hi = self.vertices.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let node = &self.vertices[mid];
            let order = (node.change.get(), node.start.get());
            if order <= (key.0, key.1) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // Scan backwards over vertices with the same change for a match,
        // preferring a non-empty containing vertex.
        let mut empty_match = None;
        let mut index = lo;
        while index > 0 {
            index -= 1;
            let node = &self.vertices[index];
            if node.change.get() != key.0 {
                break;
            }
            if node.start.get() == node.end.get() {
                if node.start.get() == key.1 {
                    empty_match = Some(*node);
                }
                continue;
            }
            if node.start.get() <= key.1 && key.1 < node.end.get() {
                return Some(*node);
            }
        }
        empty_match
    }
}

impl<'a, T> GraphTxnT for InodeScopedGraph<'a, T>
where
    T: GraphTxnT + InodeGraphOps<InodeError = PristineError> + TreeTxnT,
{
    type Adj = std::vec::IntoIter<Result<SerializedGraphEdge, PristineError>>;

    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<Self::Adj, PristineError> {
        if !self.scoped {
            return Ok(self
                .inner
                .iter_adjacent(node, min_flag, max_flag)?
                .collect::<Vec<_>>()
                .into_iter());
        }
        // `iter_inode_vertices`/`iter_inode_edges` select edges with
        // `flag >= min_flag && flag <= max_flag`; mirror that exactly.
        let out: Vec<Result<SerializedGraphEdge, PristineError>> = self
            .adjacency
            .get(&node)
            .map(|edges| {
                edges
                    .iter()
                    .filter(|edge| {
                        let flag = edge.flag();
                        flag >= min_flag && flag <= max_flag
                    })
                    .copied()
                    .map(Ok)
                    .collect()
            })
            .unwrap_or_default();
        Ok(out.into_iter())
    }

    fn find_block(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
        if self.scoped {
            if let Some(vertex) = self.find_block_cached(pos) {
                return Ok(vertex);
            }
            if let Some(vertex) = self.inner.find_block_in_inode(self.inode, pos)? {
                return Ok(vertex);
            }
        }
        self.inner.find_block(pos)
    }

    fn find_block_end(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
        if self.scoped {
            if let Some(vertex) = self.inner.find_block_end_in_inode(self.inode, pos)? {
                return Ok(vertex);
            }
        }
        self.inner.find_block_end(pos)
    }

    fn has_vertex(&self, node: GraphNode<NodeId>) -> Result<bool, PristineError> {
        if self.scoped && self.adjacency.contains_key(&node) {
            return Ok(true);
        }
        if self.scoped
            && self
                .inner
                .find_block_in_inode(self.inode, Position::new(node.change, node.start))?
                .is_some()
        {
            return Ok(true);
        }
        self.inner.has_vertex(node)
    }

    fn get_external(&self, id: NodeId) -> Result<Option<Hash>, PristineError> {
        self.inner.get_external(id)
    }

    fn get_internal(&self, hash: &Hash) -> Result<Option<NodeId>, PristineError> {
        self.inner.get_internal(hash)
    }

    fn list_registered_changes(&self) -> Result<Vec<(NodeId, Hash)>, PristineError> {
        self.inner.list_registered_changes()
    }

    fn is_change_visible(&self, id: NodeId) -> bool {
        self.inner.is_change_visible(id)
    }

    fn get_node_type(&self, node_id: NodeId) -> Result<Option<u8>, PristineError> {
        self.inner.get_node_type(node_id)
    }

    fn get_rev_deps(&self, dep_id: NodeId) -> Result<Vec<NodeId>, PristineError> {
        self.inner.get_rev_deps(dep_id)
    }

    fn get_change_deps(&self, change_id: NodeId) -> Result<Vec<Hash>, PristineError> {
        self.inner.get_change_deps(change_id)
    }

    fn change_deps_indexed_count(&self, change_id: NodeId) -> Result<Option<u64>, PristineError> {
        self.inner.change_deps_indexed_count(change_id)
    }

    fn get_rev_change_deps(&self, dep_hash: &Hash) -> Result<Vec<NodeId>, PristineError> {
        self.inner.get_rev_change_deps(dep_hash)
    }

    fn has_change_in_graph(&self, change_id: NodeId) -> Result<bool, PristineError> {
        self.inner.has_change_in_graph(change_id)
    }
}

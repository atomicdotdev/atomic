//! Read-only graph operations trait.
//!
//! `GraphTxnT` is the base trait in the pristine trait hierarchy.
//! All other transaction traits extend it.

use crate::types::{
    EdgeFlags, ForwardEdge, GraphNode, Hash, NodeId, ParentEdge, Position, SerializedGraphEdge,
};

use crate::pristine::error::PristineError;

/// Read-only graph operations
///
/// This is the base trait that provides read access to the repository graph.
/// All other transaction traits extend this one.
///
/// # Graph Structure
///
/// The graph consists of:
/// - **Vertices**: Ranges of content within changes, identified by (change_id, start, end)
/// - **Edges**: Connections between vertices with flags indicating relationship type
///
/// # ID System
///
/// Atomic uses two ID systems:
/// - **External (Hash)**: Content-addressed, globally unique, used for sync
/// - **Internal (NodeId)**: Repository-local, compact, used for storage
///
/// This trait provides methods to translate between these two systems.
pub trait GraphTxnT {
    /// Iterator type for adjacency lists
    ///
    /// This returns edges from a span. The iterator yields `Result` to handle
    /// potential storage errors during iteration.
    type Adj: Iterator<Item = Result<SerializedGraphEdge, PristineError>>;

    /// Get the external hash for an internal node ID.
    ///
    /// Translates a repository-local NodeId to the globally-unique content hash.
    ///
    /// # Returns
    ///
    /// * `Ok(Some(hash))` - The corresponding hash
    /// * `Ok(None)` - The NodeId is not registered
    /// * `Err(_)` - Database error
    fn get_external(&self, id: NodeId) -> Result<Option<Hash>, PristineError>;

    /// Get the internal node ID for an external hash.
    ///
    /// Translates a globally-unique content hash to a repository-local NodeId.
    /// This is the inverse of `get_external`.
    fn get_internal(&self, hash: &Hash) -> Result<Option<NodeId>, PristineError>;

    /// Initialize an adjacency iterator for a span.
    ///
    /// Returns an iterator over edges from the given span that have flags
    /// within the specified range. This allows filtering edges by type.
    ///
    /// # Arguments
    ///
    /// * `node` - The source span
    /// * `min_flag` - Minimum edge flags (inclusive)
    /// * `max_flag` - Maximum edge flags (inclusive)
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Get all non-deleted block edges
    /// let edges = txn.iter_adjacent(
    ///     span,
    ///     EdgeFlags::BLOCK,
    ///     EdgeFlags::BLOCK | EdgeFlags::PSEUDO,
    /// )?;
    ///
    /// for result in edges {
    ///     let edge = result?;
    ///     println!("Edge to {:?}", edge.dest());
    /// }
    /// ```
    fn iter_adjacent(
        &self,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<Self::Adj, PristineError>;

    /// Find the span containing a given position.
    ///
    /// Given a position (change_id, byte_offset), finds the span that contains
    /// that byte. This is used when navigating the graph, as edges point to
    /// positions, not vertices.
    ///
    /// # Returns
    ///
    /// * `Ok(span)` - The span containing the position
    /// * `Err(BlockNotFound)` - No span contains this position
    fn find_block(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError>;

    /// Find a block that ends at or after the given position.
    ///
    /// This is used for predecessors resolution where we need to find the span
    /// that ENDS at a position, not one that contains it. This is important
    /// when creating edges from an existing span to a new one.
    ///
    /// # Special Cases
    ///
    /// - ROOT position returns GraphNode::ROOT
    /// - Empty vertices (start == end == pos) are matched exactly
    /// - For non-empty vertices, finds one where end == pos (span ends at position)
    fn find_block_end(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError>;

    /// Check if a span exists in the graph.
    ///
    /// Returns true if the span has at least one edge (vertices are
    /// implicitly defined by their edges).
    fn has_vertex(&self, node: GraphNode<NodeId>) -> Result<bool, PristineError>;

    /// Get all edges from a span (convenience method).
    ///
    /// Equivalent to `iter_adjacent` with full flag range, collecting
    /// results into a Vec.
    fn get_edges(
        &self,
        node: GraphNode<NodeId>,
    ) -> Result<Vec<SerializedGraphEdge>, PristineError> {
        let iter = self.iter_adjacent(node, EdgeFlags::empty(), EdgeFlags::all())?;
        iter.collect()
    }

    /// Get the type of a node (Change, Tag, or Attestation).
    ///
    /// # Returns
    ///
    /// * `Ok(Some(node_type::CHANGE))` - The node is a change
    /// * `Ok(Some(node_type::TAG))` - The node is a tag
    /// * `Ok(Some(node_type::ATTESTATION))` - The node is an attestation
    /// * `Ok(None)` - The node ID is not registered
    /// * `Err(_)` - Database error
    fn get_node_type(&self, node_id: NodeId) -> Result<Option<u8>, PristineError>;

    /// Get all nodes that depend on the given node (reverse dependency lookup).
    ///
    /// Returns a list of NodeIds that have registered a dependency on
    /// the given node. Used to find attestations that cover a change
    /// (filter results by `node_type::ATTESTATION`).
    fn get_rev_deps(&self, dep_id: NodeId) -> Result<Vec<NodeId>, PristineError>;

    /// Check whether a change has any vertices in the global GRAPH.
    ///
    /// Performs an O(log N) range probe on the GRAPH B-tree for keys
    /// whose change_id matches `change_id`. This is far cheaper than
    /// loading the full `Change` and probing individual hunks.
    ///
    /// # Use Case
    ///
    /// When applying a change to a view, we need to know whether the
    /// change's edges already exist in the global GRAPH (so we can skip
    /// redundant hunk application).
    fn has_change_in_graph(&self, change_id: NodeId) -> Result<bool, PristineError>;

    /// Iterate forward (non-parent) edges from a vertex.
    ///
    /// Returns typed [`ForwardEdge`] values. When `include_deleted` is true,
    /// deleted edges (`BlockDeleted`, `FolderDeleted`) are included; otherwise
    /// they are filtered out.
    ///
    /// This is the typed replacement for [`iter_adjacent`](Self::iter_adjacent)
    /// with forward-edge flag ranges. New code should prefer this over
    /// `iter_adjacent`.
    fn iter_forward(
        &self,
        node: GraphNode<NodeId>,
        include_deleted: bool,
    ) -> Result<Vec<ForwardEdge>, PristineError> {
        // Flag-range bounds:
        //   alive only  → [0x00, 0x14]  (empty ..= PSEUDO|FOLDER)
        //   with deleted → [0x00, 0x90]  (empty ..= DELETED|FOLDER)
        //
        // The wider range may include PARENT edges (0x20–0x34) when
        // include_deleted is true; the loop filters them out.
        let min_flag = EdgeFlags::empty();
        let max_flag = if include_deleted {
            EdgeFlags::DELETED | EdgeFlags::FOLDER
        } else {
            EdgeFlags::PSEUDO | EdgeFlags::FOLDER
        };

        let adj = self.iter_adjacent(node, min_flag, max_flag)?;
        let mut result = Vec::new();
        for edge_result in adj {
            let edge = edge_result?;
            // Skip any parent edges that snuck into the range
            if edge.flag().contains(EdgeFlags::PARENT) {
                continue;
            }
            if let Some(forward) = ForwardEdge::from_serialized(&edge) {
                result.push(forward);
            }
        }
        Ok(result)
    }

    /// Iterate parent (reverse) edges of a vertex.
    ///
    /// Returns typed [`ParentEdge`] values. When `include_deleted` is true,
    /// deleted parent edges are included; otherwise they are filtered out.
    ///
    /// This is the typed replacement for [`iter_adjacent`](Self::iter_adjacent)
    /// with parent-edge flag ranges. New code should prefer this over
    /// `iter_adjacent`.
    fn iter_parents(
        &self,
        node: GraphNode<NodeId>,
        include_deleted: bool,
    ) -> Result<Vec<ParentEdge>, PristineError> {
        // Flag-range bounds:
        //   alive only  → [0x20, 0x35]  (PARENT ..= all()-DELETED)
        //   with deleted → [0x20, 0xB5]  (PARENT ..= all())
        //
        // The wider range may include forward-deleted edges (0x80–0x90)
        // when include_deleted is true; the loop filters them out.
        let min_flag = EdgeFlags::PARENT;
        let max_flag = if include_deleted {
            EdgeFlags::all()
        } else {
            EdgeFlags::all() - EdgeFlags::DELETED
        };

        let adj = self.iter_adjacent(node, min_flag, max_flag)?;
        let mut result = Vec::new();
        for edge_result in adj {
            let edge = edge_result?;
            if !edge.flag().contains(EdgeFlags::PARENT) {
                continue;
            }
            if let Some(parent) = ParentEdge::from_serialized(&edge) {
                result.push(parent);
            }
        }
        Ok(result)
    }
}

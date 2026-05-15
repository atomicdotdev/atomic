//! Types and traits for inode-scoped graph operations.
//!
//! This module contains the core types used by the inode graph subsystem:
//! [`InodeVertex`], [`InodeAdjState`], [`InodeGraphStats`], the [`InodeGraphOps`]
//! trait, [`InodeEdgeIter`], and the [`IntoInodeVertex`] conversion trait.

use crate::types::{EdgeFlags, GraphNode, Inode, NodeId, Position, SerializedGraphEdge};

// INODE VERTEX COMPOSITE KEY

/// A composite key combining file identity (Inode) with graph span.
///
/// This key structure enables two-level B-tree indexing where:
/// - First level: Inode groups all vertices for a file together
/// - Second level: GraphNode ordering within the file
///
/// The ordering is lexicographic: first by inode, then by span.
/// This ensures all edges for a file are stored contiguously.
///
/// # Memory Layout
///
/// ```text
/// InodeVertex (32 bytes)
/// ├── inode: Inode       (8 bytes)
/// └── node: GraphNode     (24 bytes)
///     ├── change: NodeId (8 bytes)
///     ├── start: u64     (8 bytes)
///     └── end: u64       (8 bytes)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(C)]
pub struct InodeVertex {
    /// The inode identifying which file this node belongs to.
    /// This is the primary sort key.
    pub inode: Inode,
    /// The node within the file's graph.
    /// This is the secondary sort key.
    pub node: GraphNode<NodeId>,
}

impl InodeVertex {
    /// The root inode-span, used as a sentinel.
    pub const ROOT: InodeVertex = InodeVertex {
        inode: Inode::ROOT,
        node: GraphNode::ROOT,
    };

    /// Create a new InodeVertex from components.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file this node belongs to
    /// * `node` - The node within the file
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::pristine::InodeVertex;
    /// use atomic_core::types::{Inode, NodeId, GraphNode, ChangePosition};
    ///
    /// let inode = Inode::new(42);
    /// let node = GraphNode::new(NodeId::new(1), ChangePosition::new(0), ChangePosition::new(100));
    /// let iv = InodeVertex::new(inode, node);
    ///
    /// assert_eq!(iv.inode, inode);
    /// assert_eq!(iv.node, node);
    /// ```
    #[inline]
    pub fn new(inode: Inode, node: GraphNode<NodeId>) -> Self {
        Self { inode, node }
    }

    /// Create an InodeVertex for a specific inode with minimum span.
    ///
    /// Useful for starting iteration over all vertices in a file.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file to iterate
    ///
    /// # Returns
    ///
    /// An InodeVertex with the given inode and `GraphNode::ROOT`.
    #[inline]
    pub fn min_for_inode(inode: Inode) -> Self {
        Self {
            inode,
            node: GraphNode::ROOT,
        }
    }

    /// Create an InodeVertex for a specific inode with maximum span.
    ///
    /// Useful for ending iteration over all vertices in a file.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file to iterate
    ///
    /// # Returns
    ///
    /// An InodeVertex with the given inode and `GraphNode::MAX`.
    #[inline]
    pub fn max_for_inode(inode: Inode) -> Self {
        Self {
            inode,
            node: GraphNode::MAX,
        }
    }

    /// Check if this is the root inode-span.
    #[inline]
    pub fn is_root(&self) -> bool {
        *self == Self::ROOT
    }

    /// Get the start position of the contained span.
    #[inline]
    pub fn start_pos(&self) -> Position<NodeId> {
        self.node.start_pos()
    }

    /// Get the end position of the contained span.
    #[inline]
    pub fn end_pos(&self) -> Position<NodeId> {
        self.node.end_pos()
    }
}

impl std::fmt::Display for InodeVertex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IV({}, {})", self.inode, self.node)
    }
}

// INODE ADJACENCY STATE

/// State for inode-scoped adjacency iteration.
///
/// This structure maintains the cursor state when iterating over edges
/// within a file's inode-scoped index.
#[derive(Debug, Clone)]
pub struct InodeAdjState {
    /// The inode being traversed.
    pub inode: Inode,
    /// The current span.
    pub node: GraphNode<NodeId>,
    /// Minimum edge flags to include.
    pub min_flag: EdgeFlags,
    /// Maximum edge flags to include.
    pub max_flag: EdgeFlags,
    /// Current position in the iteration.
    pub position: usize,
    /// Cached matching edges for this inode/node pair.
    pub edges: Vec<SerializedGraphEdge>,
    /// Whether iteration has completed.
    pub exhausted: bool,
}

impl InodeAdjState {
    /// Create a new adjacency state.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file being traversed
    /// * `node` - The starting span
    /// * `min_flag` - Minimum edge flags to include
    /// * `max_flag` - Maximum edge flags to include
    pub fn new(
        inode: Inode,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Self {
        Self {
            inode,
            node,
            min_flag,
            max_flag,
            position: 0,
            edges: Vec::new(),
            exhausted: false,
        }
    }

    /// Check if the iteration is exhausted.
    #[inline]
    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Mark the iteration as exhausted.
    #[inline]
    pub fn mark_exhausted(&mut self) {
        self.exhausted = true;
    }

    /// Advance to the next position.
    #[inline]
    pub fn advance(&mut self) {
        self.position += 1;
    }

    /// Returns true if the adjacency cache has been populated.
    #[inline]
    pub fn is_loaded(&self) -> bool {
        !self.edges.is_empty() || self.exhausted || self.position > 0
    }

    /// Store the filtered edge list for this adjacency cursor.
    #[inline]
    pub fn set_edges(&mut self, edges: Vec<SerializedGraphEdge>) {
        self.edges = edges;
        self.position = 0;
    }
}

// INODE GRAPH STATISTICS

/// Statistics collected during inode-scoped graph operations.
///
/// Used for performance monitoring and optimization tuning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InodeGraphStats {
    /// Number of vertices visited during traversal.
    pub vertices_visited: usize,
    /// Number of edges traversed.
    pub edges_traversed: usize,
    /// Estimated number of B-tree page accesses.
    pub page_accesses: usize,
    /// Number of cache hits (positions already seen).
    pub cache_hits: usize,
}

impl InodeGraphStats {
    /// Create new empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge statistics from another operation.
    pub fn merge(&mut self, other: &Self) {
        self.vertices_visited += other.vertices_visited;
        self.edges_traversed += other.edges_traversed;
        self.page_accesses += other.page_accesses;
        self.cache_hits += other.cache_hits;
    }

    /// Calculate the cache hit ratio.
    ///
    /// Returns a value between 0.0 (no hits) and 1.0 (all hits).
    pub fn cache_hit_ratio(&self) -> f64 {
        let total = self.vertices_visited + self.cache_hits;
        if total == 0 {
            0.0
        } else {
            self.cache_hits as f64 / total as f64
        }
    }
}

impl std::fmt::Display for InodeGraphStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} vertices, {} edges, {} pages, {:.1}% cache hits",
            self.vertices_visited,
            self.edges_traversed,
            self.page_accesses,
            self.cache_hit_ratio() * 100.0
        )
    }
}

// INODE GRAPH OPS TRAIT

/// Trait for inode-scoped graph operations.
///
/// This trait defines the interface for efficient file-local graph traversal
/// using the two-level B-tree index. Implementations use `InodeVertex` keys
/// to ensure all edges for a file are accessed contiguously.
///
/// # Performance Characteristics
///
/// Operations on this trait should achieve:
/// - O(m) iteration where m is the number of vertices in the file
/// - O(log N + m) for cursor initialization and full scan
/// - Minimal page cache pressure due to spatial locality
///
/// # Design Note
///
/// This trait is separate from `GraphTxnT` to allow implementations that
/// support both standard and optimized traversal. Code can check
/// `inode_graph_is_populated()` to decide which path to take.
pub trait InodeGraphOps {
    /// Error type for inode graph operations.
    type InodeError: std::error::Error + Send + Sync + 'static;

    /// Initialize an adjacency iterator for a span within an inode scope.
    ///
    /// This is the inode-scoped equivalent of `iter_adjacent`, but uses the
    /// `(Inode, Span)` composite key for efficient file-local traversal.
    ///
    /// # Arguments
    ///
    /// * `inode` - The inode (file) context
    /// * `node` - The node to get edges from
    /// * `min_flag` - Minimum edge flags to include
    /// * `max_flag` - Maximum edge flags to include
    ///
    /// # Returns
    ///
    /// An adjacency state for iteration, or an error.
    fn init_inode_adj(
        &self,
        inode: Inode,
        node: GraphNode<NodeId>,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<InodeAdjState, Self::InodeError>;

    /// Get the next adjacent edge within the inode scope.
    ///
    /// Returns `None` when iteration is complete.
    ///
    /// # Arguments
    ///
    /// * `adj` - The adjacency state to advance
    ///
    /// # Returns
    ///
    /// - `Some(Ok(edge))` - The next edge
    /// - `Some(Err(e))` - An error occurred
    /// - `None` - Iteration complete
    fn next_inode_adj(
        &self,
        adj: &mut InodeAdjState,
    ) -> Option<Result<SerializedGraphEdge, Self::InodeError>>;

    /// Find a block containing the given position within an inode scope.
    ///
    /// This is more efficient than the global `find_block` when the inode
    /// is known, as it can use the inode-scoped index.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file context
    /// * `pos` - The position to find
    ///
    /// # Returns
    ///
    /// - `Ok(Some(node))` - The node containing the position
    /// - `Ok(None)` - No node contains this position
    /// - `Err(e)` - Database error
    fn find_block_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError>;

    /// Count vertices in an inode scope.
    ///
    /// More efficient than iterating and counting when only the count is needed.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file to count vertices for
    ///
    /// # Returns
    ///
    /// The number of vertices belonging to this inode.
    fn count_inode_vertices(&self, inode: Inode) -> Result<usize, Self::InodeError>;

    /// Check if the inode-scoped index has been populated for this inode.
    ///
    /// Returns `true` if there's at least one entry in the inode_graph for
    /// this inode. This can be used to determine whether to use the optimized
    /// path or fall back to standard iteration.
    ///
    /// # Default Implementation
    ///
    /// Checks if `count_inode_vertices() > 0`. Implementations may override
    /// with a more efficient check.
    fn inode_graph_is_populated(&self, inode: Inode) -> Result<bool, Self::InodeError> {
        Ok(self.count_inode_vertices(inode)? > 0)
    }

    /// Iterate all edges for vertices within an inode scope.
    ///
    /// This provides a convenient way to iterate all edges for a file
    /// without managing adjacency state manually.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file to iterate
    /// * `min_flag` - Minimum edge flags to include
    /// * `max_flag` - Maximum edge flags to include
    ///
    /// # Returns
    ///
    /// An iterator over all matching edges for the file.
    fn iter_inode_edges(
        &self,
        inode: Inode,
        min_flag: EdgeFlags,
        max_flag: EdgeFlags,
    ) -> Result<InodeEdgeIter<'_, Self>, Self::InodeError>
    where
        Self: Sized,
    {
        Ok(InodeEdgeIter {
            ops: self,
            inode,
            min_flag,
            max_flag,
            current_adj: None,
            exhausted: false,
        })
    }
}

// INODE EDGE ITERATOR

/// Iterator over edges within an inode scope.
///
/// This iterator uses the `InodeGraphOps` trait to efficiently iterate
/// over all edges belonging to a file.
#[allow(dead_code)]
pub struct InodeEdgeIter<'a, T: InodeGraphOps> {
    /// Reference to the graph operations provider.
    ops: &'a T,
    /// The inode being iterated.
    inode: Inode,
    /// Minimum edge flags.
    min_flag: EdgeFlags,
    /// Maximum edge flags.
    max_flag: EdgeFlags,
    /// Current adjacency state.
    current_adj: Option<InodeAdjState>,
    /// Whether iteration is exhausted.
    exhausted: bool,
}

impl<'a, T: InodeGraphOps> Iterator for InodeEdgeIter<'a, T> {
    type Item = Result<SerializedGraphEdge, T::InodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.exhausted {
            return None;
        }

        // If we have a current adjacency state, try to get the next edge
        if let Some(ref mut adj) = self.current_adj {
            if let Some(result) = self.ops.next_inode_adj(adj) {
                return Some(result);
            }
        }

        // Adjacency exhausted, mark as done
        self.exhausted = true;
        None
    }
}

// CONVERSION TRAITS

/// Trait for converting to an InodeVertex.
pub trait IntoInodeVertex {
    /// Convert this value into an InodeVertex with the given inode.
    fn into_inode_vertex(self, inode: Inode) -> InodeVertex;
}

impl IntoInodeVertex for GraphNode<NodeId> {
    #[inline]
    fn into_inode_vertex(self, inode: Inode) -> InodeVertex {
        InodeVertex::new(inode, self)
    }
}

impl IntoInodeVertex for Position<NodeId> {
    #[inline]
    fn into_inode_vertex(self, inode: Inode) -> InodeVertex {
        InodeVertex::new(inode, self.inode_node())
    }
}

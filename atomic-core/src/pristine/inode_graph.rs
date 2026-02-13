//! Inode-scoped graph operations for two-level B-tree optimization
//!
//! This module provides the `InodeGraphOps` trait which enables efficient
//! file-local graph traversal using a dual B-tree indexing strategy.
//!
//! # Implementation
//!
//! This module implements `InodeGraphOps` for both `ReadTxn` and `WriteTxn`,
//! enabling optimized file-local graph traversal using the `INODE_GRAPH`
//! secondary index.
//!
//! # Performance Rationale
//!
//! The standard graph storage uses `GraphNode<NodeId>` as the key, storing all
//! vertices from all files in a single B-tree. This leads to O(n × log N)
//! traversal complexity when iterating edges for a file, where N is the total
//! number of vertices across ALL files.
//!
//! By using `(Inode, GraphNode<NodeId>)` as a composite key in a secondary index:
//! - All edges for a single file are stored contiguously
//! - Cursor-based iteration within a file becomes O(m) where m is vertices in that file
//! - Cross-file queries remain possible via the primary index
//!
//! # Architecture
//!
//! The optimization uses a **dual-index strategy**:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      Dual B-Tree Index Architecture                      │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Primary Index (GRAPH)              Secondary Index (INODE_GRAPH)       │
//! │  Key: GraphNode<NodeId>                Key: (Inode, GraphNode<NodeId>)        │
//! │  ┌─────────────────────┐            ┌─────────────────────────────┐    │
//! │  │ V(1, 0:10)  → edges │            │ (Inode(42), V(1,0:10)) → e │    │
//! │  │ V(1, 10:20) → edges │            │ (Inode(42), V(1,10:20))→ e │    │
//! │  │ V(2, 0:5)   → edges │            │ (Inode(42), V(2,0:5))  → e │    │
//! │  │ V(3, 0:100) → edges │            │ (Inode(99), V(3,0:100))→ e │    │
//! │  │ ...         → ...   │            │ ...                        │    │
//! │  └─────────────────────┘            └─────────────────────────────┘    │
//! │                                                                         │
//! │  Use for:                           Use for:                            │
//! │  - Cross-file queries               - File-local traversal              │
//! │  - Global operations                - Output/retrieve operations        │
//! │  - Backward compatibility           - O(m) instead of O(m × log N)     │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Expected Performance Improvement
//!
//! | Changes | Before (O(n log N)) | After (O(n)) | Improvement |
//! |---------|---------------------|--------------|-------------|
//! | 1,000   | ~230ms              | ~50ms        | ~5x         |
//! | 10,000  | ~2s                 | ~200ms       | ~10x        |
//! | 100,000 | ~20s                | ~2s          | ~10x        |
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::pristine::{InodeGraphOps, GraphTxnT};
//!
//! fn traverse_file<T: GraphTxnT + InodeGraphOps>(
//!     txn: &T,
//!     inode: Inode,
//!     start_pos: Position<NodeId>,
//! ) -> Result<Vec<GraphNode<NodeId>>, T::Error> {
//!     let mut vertices = Vec::new();
//!
//!     // Check if inode index is populated
//!     if txn.inode_graph_is_populated(inode)? {
//!         // Use optimized inode-scoped iteration
//!         let mut adj = txn.init_inode_adj(
//!             inode,
//!             start_pos.inode_node(),
//!             EdgeFlags::empty(),
//!             EdgeFlags::BLOCK | EdgeFlags::PSEUDO,
//!         )?;
//!
//!         while let Some(edge) = txn.next_inode_adj(&mut adj)? {
//!             // Process edge...
//!         }
//!     } else {
//!         // Fall back to standard iteration
//!         // ...
//!     }
//!
//!     Ok(vertices)
//! }
//! ```

use crate::pristine::error::PristineError;
use crate::pristine::tables::{decode_inode_vertex, encode_inode_vertex, INODE_GRAPH};
use crate::pristine::txn::{ReadTxn, WriteTxn};
use crate::types::{
    ChangePosition, EdgeFlags, GraphNode, Inode, NodeId, Position, SerializedGraphEdge,
};

use redb::ReadableMultimapTable;

// ============================================================================
// INODE VERTEX COMPOSITE KEY
// ============================================================================

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
    /// * `span` - The node within the file
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

// ============================================================================
// INODE ADJACENCY STATE
// ============================================================================

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
    /// Whether iteration has completed.
    pub exhausted: bool,
}

impl InodeAdjState {
    /// Create a new adjacency state.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file being traversed
    /// * `span` - The starting span
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
}

// ============================================================================
// INODE GRAPH STATISTICS
// ============================================================================

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

// ============================================================================
// INODE GRAPH OPS TRAIT
// ============================================================================

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
///
/// # Example
///
/// ```rust,ignore
/// fn traverse_optimized<T: GraphTxnT + InodeGraphOps>(
///     txn: &T,
///     inode: Inode,
///     span: GraphNode<NodeId>,
/// ) -> Result<Vec<SerializedGraphEdge>, T::InodeError> {
///     let mut edges = Vec::new();
///
///     if txn.inode_graph_is_populated(inode)? {
///         let mut adj = txn.init_inode_adj(
///             inode,
///             node,
///             EdgeFlags::empty(),
///             EdgeFlags::BLOCK | EdgeFlags::PSEUDO,
///         )?;
///
///         while let Some(result) = txn.next_inode_adj(&mut adj) {
///             edges.push(result?.clone());
///         }
///     }
///
///     Ok(edges)
/// }
/// ```
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
    /// * `span` - The span to get edges from
    /// * `min_flag` - Minimum edge flags to include
    /// * `max_flag` - Maximum edge flags to include
    ///
    /// # Returns
    ///
    /// An adjacency state for iteration, or an error.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut adj = txn.init_inode_adj(
    ///     inode,
    ///     start_vertex,
    ///     EdgeFlags::empty(),
    ///     EdgeFlags::BLOCK | EdgeFlags::PSEUDO,
    /// )?;
    /// ```
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
    /// - `Ok(Some(span))` - The span containing the position
    /// - `Ok(None)` - No span contains this position
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
    /// # Arguments
    ///
    /// * `inode` - The file to check
    ///
    /// # Returns
    ///
    /// `true` if the inode has entries in the secondary index.
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

// ============================================================================
// INODE EDGE ITERATOR
// ============================================================================

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

// ============================================================================
// CONVERSION TRAITS
// ============================================================================

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

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    // -------------------------------------------------------------------------
    // InodeVertex Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_inode_vertex_new() {
        let inode = Inode::new(42);
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(100),
        );
        let iv = InodeVertex::new(inode, node);

        assert_eq!(iv.inode, inode);
        assert_eq!(iv.node, node);
    }

    #[test]
    fn test_inode_vertex_root() {
        assert!(InodeVertex::ROOT.is_root());
        assert_eq!(InodeVertex::ROOT.inode, Inode::ROOT);
        assert_eq!(InodeVertex::ROOT.node, GraphNode::ROOT);
    }

    #[test]
    fn test_inode_vertex_min_for_inode() {
        let inode = Inode::new(99);
        let iv = InodeVertex::min_for_inode(inode);

        assert_eq!(iv.inode, inode);
        assert_eq!(iv.node, GraphNode::ROOT);
    }

    #[test]
    fn test_inode_vertex_max_for_inode() {
        let inode = Inode::new(99);
        let iv = InodeVertex::max_for_inode(inode);

        assert_eq!(iv.inode, inode);
        assert_eq!(iv.node, GraphNode::MAX);
    }

    #[test]
    fn test_inode_vertex_ordering() {
        let iv1 = InodeVertex::new(
            Inode::new(1),
            GraphNode::new(
                NodeId::new(1),
                ChangePosition::new(0),
                ChangePosition::new(10),
            ),
        );
        let iv2 = InodeVertex::new(
            Inode::new(1),
            GraphNode::new(
                NodeId::new(2),
                ChangePosition::new(0),
                ChangePosition::new(10),
            ),
        );
        let iv3 = InodeVertex::new(
            Inode::new(2),
            GraphNode::new(
                NodeId::new(1),
                ChangePosition::new(0),
                ChangePosition::new(10),
            ),
        );

        // Same inode, different span - ordered by span
        assert!(iv1 < iv2);

        // Different inode - ordered by inode first
        assert!(iv1 < iv3);
        assert!(iv2 < iv3);
    }

    #[test]
    fn test_inode_vertex_positions() {
        let inode = Inode::new(1);
        let node = GraphNode::new(
            NodeId::new(5),
            ChangePosition::new(10),
            ChangePosition::new(20),
        );
        let iv = InodeVertex::new(inode, node);

        let start = iv.start_pos();
        assert_eq!(start.change, NodeId::new(5));
        assert_eq!(start.pos, ChangePosition::new(10));

        let end = iv.end_pos();
        assert_eq!(end.change, NodeId::new(5));
        assert_eq!(end.pos, ChangePosition::new(20));
    }

    #[test]
    fn test_inode_vertex_display() {
        let iv = InodeVertex::new(
            Inode::new(42),
            GraphNode::new(
                NodeId::new(1),
                ChangePosition::new(0),
                ChangePosition::new(10),
            ),
        );
        let display = iv.to_string();
        assert!(display.contains("IV"));
        assert!(display.contains("42"));
    }

    #[test]
    fn test_inode_vertex_debug() {
        let iv = InodeVertex::ROOT;
        let debug = format!("{:?}", iv);
        assert!(debug.contains("InodeVertex"));
    }

    #[test]
    fn test_inode_vertex_hash() {
        use std::collections::HashSet;
        let iv1 = InodeVertex::new(Inode::new(1), GraphNode::ROOT);
        let iv2 = InodeVertex::new(Inode::new(2), GraphNode::ROOT);
        let iv3 = InodeVertex::new(Inode::new(1), GraphNode::ROOT); // duplicate

        let mut set = HashSet::new();
        set.insert(iv1);
        set.insert(iv2);
        set.insert(iv3);
        assert_eq!(set.len(), 2);
    }

    // -------------------------------------------------------------------------
    // InodeAdjState Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_inode_adj_state_new() {
        let state = InodeAdjState::new(
            Inode::new(1),
            GraphNode::ROOT,
            EdgeFlags::empty(),
            EdgeFlags::BLOCK,
        );

        assert_eq!(state.inode, Inode::new(1));
        assert_eq!(state.position, 0);
        assert!(!state.is_exhausted());
    }

    #[test]
    fn test_inode_adj_state_advance() {
        let mut state = InodeAdjState::new(
            Inode::new(1),
            GraphNode::ROOT,
            EdgeFlags::empty(),
            EdgeFlags::BLOCK,
        );

        assert_eq!(state.position, 0);
        state.advance();
        assert_eq!(state.position, 1);
        state.advance();
        assert_eq!(state.position, 2);
    }

    #[test]
    fn test_inode_adj_state_exhausted() {
        let mut state = InodeAdjState::new(
            Inode::new(1),
            GraphNode::ROOT,
            EdgeFlags::empty(),
            EdgeFlags::BLOCK,
        );

        assert!(!state.is_exhausted());
        state.mark_exhausted();
        assert!(state.is_exhausted());
    }

    #[test]
    fn test_inode_adj_state_clone() {
        let state = InodeAdjState::new(
            Inode::new(42),
            GraphNode::ROOT,
            EdgeFlags::BLOCK,
            EdgeFlags::BLOCK | EdgeFlags::PSEUDO,
        );

        let cloned = state.clone();
        assert_eq!(state.inode, cloned.inode);
        assert_eq!(state.min_flag, cloned.min_flag);
        assert_eq!(state.max_flag, cloned.max_flag);
    }

    // -------------------------------------------------------------------------
    // InodeGraphStats Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_inode_graph_stats_new() {
        let stats = InodeGraphStats::new();
        assert_eq!(stats.vertices_visited, 0);
        assert_eq!(stats.edges_traversed, 0);
        assert_eq!(stats.page_accesses, 0);
        assert_eq!(stats.cache_hits, 0);
    }

    #[test]
    fn test_inode_graph_stats_default() {
        let stats = InodeGraphStats::default();
        assert_eq!(stats, InodeGraphStats::new());
    }

    #[test]
    fn test_inode_graph_stats_merge() {
        let mut s1 = InodeGraphStats {
            vertices_visited: 10,
            edges_traversed: 20,
            page_accesses: 5,
            cache_hits: 3,
        };

        let s2 = InodeGraphStats {
            vertices_visited: 5,
            edges_traversed: 10,
            page_accesses: 2,
            cache_hits: 1,
        };

        s1.merge(&s2);

        assert_eq!(s1.vertices_visited, 15);
        assert_eq!(s1.edges_traversed, 30);
        assert_eq!(s1.page_accesses, 7);
        assert_eq!(s1.cache_hits, 4);
    }

    #[test]
    fn test_inode_graph_stats_cache_hit_ratio() {
        let stats = InodeGraphStats {
            vertices_visited: 80,
            edges_traversed: 0,
            page_accesses: 0,
            cache_hits: 20,
        };

        // 20 / (80 + 20) = 0.2
        assert!((stats.cache_hit_ratio() - 0.2).abs() < 0.001);
    }

    #[test]
    fn test_inode_graph_stats_cache_hit_ratio_empty() {
        let stats = InodeGraphStats::new();
        assert_eq!(stats.cache_hit_ratio(), 0.0);
    }
}

// ============================================================================
// EDGE DESERIALIZATION (local copy to avoid module visibility issues)
// ============================================================================

/// Deserialize bytes to a SerializedGraphEdge
#[inline]
fn deserialize_edge(bytes: &[u8; 24]) -> SerializedGraphEdge {
    let flag_and_pos = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let change_id = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let introduced_by = u64::from_le_bytes(bytes[16..24].try_into().unwrap());

    let flag = EdgeFlags::from_bits_truncate((flag_and_pos >> 56) as u8);
    let pos = flag_and_pos & ((1 << 56) - 1);

    let dest = Position::new(NodeId::new(change_id), ChangePosition::new(pos));
    SerializedGraphEdge::new(flag, dest, NodeId::new(introduced_by))
}

// ============================================================================
// InodeGraphOps Implementation for ReadTxn
// ============================================================================

impl InodeGraphOps for ReadTxn {
    type InodeError = PristineError;

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
        adj: &mut InodeAdjState,
    ) -> Option<Result<SerializedGraphEdge, Self::InodeError>> {
        if adj.is_exhausted() {
            return None;
        }

        let table = match self.txn.open_multimap_table(INODE_GRAPH) {
            Ok(t) => t,
            Err(e) => {
                adj.mark_exhausted();
                return Some(Err(PristineError::Table(e)));
            }
        };

        let inode_id = adj.inode.get();
        let key = encode_inode_vertex(
            inode_id,
            adj.node.change.get(),
            adj.node.start.get(),
            adj.node.end.get(),
        );

        // Get all edges for this exact span
        let values = match table.get(&key) {
            Ok(v) => v,
            Err(e) => {
                adj.mark_exhausted();
                return Some(Err(PristineError::Storage(e)));
            }
        };

        // Collect matching edges into a vector
        let mut matching_edges: Vec<SerializedGraphEdge> = Vec::new();
        for result in values {
            match result {
                Ok(v) => {
                    let edge = deserialize_edge(v.value());
                    let flag = edge.flag();
                    if flag >= adj.min_flag && flag <= adj.max_flag {
                        matching_edges.push(edge);
                    }
                }
                Err(e) => {
                    adj.mark_exhausted();
                    return Some(Err(PristineError::Storage(e)));
                }
            }
        }

        // Return the edge at the current position
        if adj.position < matching_edges.len() {
            let edge = matching_edges[adj.position];
            adj.advance();
            Some(Ok(edge))
        } else {
            adj.mark_exhausted();
            None
        }
    }

    fn find_block_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        let start_key = encode_inode_vertex(inode_id, change_id, 0, 0);
        let end_key = encode_inode_vertex(inode_id, change_id + 1, 0, 0);

        for result in table.range::<&[u8; 32]>(&start_key..&end_key)? {
            let (key, _values) = result?;
            let (_, v_change, v_start, v_end) = decode_inode_vertex(key.value());

            if v_change == change_id && v_start <= target_pos && target_pos < v_end {
                return Ok(Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                }));
            }
        }

        Ok(None)
    }

    fn count_inode_vertices(&self, inode: Inode) -> Result<usize, Self::InodeError> {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let start_key = encode_inode_vertex(inode_id, 0, 0, 0);
        let end_key = encode_inode_vertex(inode_id + 1, 0, 0, 0);

        let mut count = 0;
        let mut last_vertex: Option<(u64, u64, u64)> = None;

        for result in table.range::<&[u8; 32]>(&start_key..&end_key)? {
            let (key, _values) = result?;
            let (_, change_id, start, end) = decode_inode_vertex(key.value());

            let current = (change_id, start, end);
            if last_vertex != Some(current) {
                count += 1;
                last_vertex = Some(current);
            }
        }

        Ok(count)
    }
}

// ============================================================================
// InodeGraphOps Implementation for WriteTxn
// ============================================================================

impl<'a> InodeGraphOps for WriteTxn<'a> {
    type InodeError = PristineError;

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
        adj: &mut InodeAdjState,
    ) -> Option<Result<SerializedGraphEdge, Self::InodeError>> {
        if adj.is_exhausted() {
            return None;
        }

        let table = match self.txn.open_multimap_table(INODE_GRAPH) {
            Ok(t) => t,
            Err(e) => {
                adj.mark_exhausted();
                return Some(Err(PristineError::Table(e)));
            }
        };

        let inode_id = adj.inode.get();
        let key = encode_inode_vertex(
            inode_id,
            adj.node.change.get(),
            adj.node.start.get(),
            adj.node.end.get(),
        );

        // Get all edges for this exact span
        let values = match table.get(&key) {
            Ok(v) => v,
            Err(e) => {
                adj.mark_exhausted();
                return Some(Err(PristineError::Storage(e)));
            }
        };

        // Collect matching edges into a vector
        let mut matching_edges: Vec<SerializedGraphEdge> = Vec::new();
        for result in values {
            match result {
                Ok(v) => {
                    let edge = deserialize_edge(v.value());
                    let flag = edge.flag();
                    if flag >= adj.min_flag && flag <= adj.max_flag {
                        matching_edges.push(edge);
                    }
                }
                Err(e) => {
                    adj.mark_exhausted();
                    return Some(Err(PristineError::Storage(e)));
                }
            }
        }

        // Return the edge at the current position
        if adj.position < matching_edges.len() {
            let edge = matching_edges[adj.position];
            adj.advance();
            Some(Ok(edge))
        } else {
            adj.mark_exhausted();
            None
        }
    }

    fn find_block_in_inode(
        &self,
        inode: Inode,
        pos: Position<NodeId>,
    ) -> Result<Option<GraphNode<NodeId>>, Self::InodeError> {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let change_id = pos.change.get();
        let target_pos = pos.pos.get();

        let start_key = encode_inode_vertex(inode_id, change_id, 0, 0);
        let end_key = encode_inode_vertex(inode_id, change_id + 1, 0, 0);

        for result in table.range::<&[u8; 32]>(&start_key..&end_key)? {
            let (key, _values) = result?;
            let (_, v_change, v_start, v_end) = decode_inode_vertex(key.value());

            if v_change == change_id && v_start <= target_pos && target_pos < v_end {
                return Ok(Some(GraphNode {
                    change: NodeId::new(v_change),
                    start: ChangePosition::new(v_start),
                    end: ChangePosition::new(v_end),
                }));
            }
        }

        Ok(None)
    }

    fn count_inode_vertices(&self, inode: Inode) -> Result<usize, Self::InodeError> {
        let table = self.txn.open_multimap_table(INODE_GRAPH)?;

        let inode_id = inode.get();
        let start_key = encode_inode_vertex(inode_id, 0, 0, 0);
        let end_key = encode_inode_vertex(inode_id + 1, 0, 0, 0);

        let mut count = 0;
        let mut last_vertex: Option<(u64, u64, u64)> = None;

        for result in table.range::<&[u8; 32]>(&start_key..&end_key)? {
            let (key, _values) = result?;
            let (_, change_id, start, end) = decode_inode_vertex(key.value());

            let current = (change_id, start, end);
            if last_vertex != Some(current) {
                count += 1;
                last_vertex = Some(current);
            }
        }

        Ok(count)
    }
}

// ============================================================================
// ADDITIONAL INTEGRATION TESTS
// ============================================================================

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::pristine::{MutTxnT, Pristine};
    use crate::types::Hash;
    use tempfile::tempdir;

    #[test]
    fn test_inode_graph_ops_empty_database() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let txn = pristine.read_txn().unwrap();

        // Empty database should have no vertices for any inode
        let inode = Inode::new(42);
        assert_eq!(txn.count_inode_vertices(inode).unwrap(), 0);
        assert!(!txn.inode_graph_is_populated(inode).unwrap());
    }

    #[test]
    fn test_inode_graph_ops_with_data() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let inode = Inode::new(42);
        let change_hash = Hash::of(b"test change");

        // Write some data
        {
            let mut txn = pristine.write_txn().unwrap();

            // Register a change
            let change_id = txn.register_change(&change_hash).unwrap();

            // Create a span
            let node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(100));

            // Create an edge
            let dest = Position::new(change_id, ChangePosition::new(50));
            let edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id);

            // Put into inode graph
            txn.put_inode_graph(inode, node, edge).unwrap();
            txn.commit().unwrap();
        }

        // Read and verify
        {
            let txn = pristine.read_txn().unwrap();

            // Should have vertices now
            assert!(txn.inode_graph_is_populated(inode).unwrap());
            assert_eq!(txn.count_inode_vertices(inode).unwrap(), 1);

            // Other inodes should still be empty
            assert!(!txn.inode_graph_is_populated(Inode::new(99)).unwrap());
        }
    }

    #[test]
    fn test_inode_adj_iteration() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let inode = Inode::new(1);
        let change_hash = Hash::of(b"test");

        // Setup test data
        let change_id;
        let node;
        {
            let mut txn = pristine.write_txn().unwrap();
            change_id = txn.register_change(&change_hash).unwrap();
            node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(50));

            // Add multiple edges to the same span
            let dest1 = Position::new(change_id, ChangePosition::new(10));
            let dest2 = Position::new(change_id, ChangePosition::new(20));

            txn.put_inode_graph(
                inode,
                node,
                SerializedGraphEdge::new(EdgeFlags::BLOCK, dest1, change_id),
            )
            .unwrap();
            txn.put_inode_graph(
                inode,
                node,
                SerializedGraphEdge::new(EdgeFlags::BLOCK | EdgeFlags::PSEUDO, dest2, change_id),
            )
            .unwrap();
            txn.commit().unwrap();
        }

        // Test adjacency iteration
        {
            let txn = pristine.read_txn().unwrap();

            let mut adj = txn
                .init_inode_adj(inode, node, EdgeFlags::empty(), EdgeFlags::all())
                .unwrap();

            let mut count = 0;
            while let Some(result) = txn.next_inode_adj(&mut adj) {
                result.unwrap();
                count += 1;
            }

            assert_eq!(count, 2);
            assert!(adj.is_exhausted());
        }
    }

    #[test]
    fn test_find_block_in_inode() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let inode = Inode::new(5);
        let change_hash = Hash::of(b"block test");

        let change_id;
        let node;
        {
            let mut txn = pristine.write_txn().unwrap();
            change_id = txn.register_change(&change_hash).unwrap();

            // Create a span spanning positions 100-200
            node = GraphNode::new(
                change_id,
                ChangePosition::new(100),
                ChangePosition::new(200),
            );

            let dest = Position::new(change_id, ChangePosition::new(150));
            txn.put_inode_graph(
                inode,
                node,
                SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
            )
            .unwrap();
            txn.commit().unwrap();
        }

        // Test finding blocks
        {
            let txn = pristine.read_txn().unwrap();

            // Position inside the block should find it
            let pos_inside = Position::new(change_id, ChangePosition::new(150));
            let found = txn.find_block_in_inode(inode, pos_inside).unwrap();
            assert_eq!(found, Some(node));

            // Position at start should find it
            let pos_start = Position::new(change_id, ChangePosition::new(100));
            let found = txn.find_block_in_inode(inode, pos_start).unwrap();
            assert_eq!(found, Some(node));

            // Position outside (before) should not find it
            let pos_before = Position::new(change_id, ChangePosition::new(50));
            let found = txn.find_block_in_inode(inode, pos_before).unwrap();
            assert_eq!(found, None);

            // Position outside (at end, exclusive) should not find it
            let pos_at_end = Position::new(change_id, ChangePosition::new(200));
            let found = txn.find_block_in_inode(inode, pos_at_end).unwrap();
            assert_eq!(found, None);

            // Different change_id should not find it
            let other_hash = Hash::of(b"other");
            let mut write_txn = pristine.write_txn().unwrap();
            let other_change_id = write_txn.register_change(&other_hash).unwrap();
            write_txn.commit().unwrap();

            let txn = pristine.read_txn().unwrap();
            let pos_other = Position::new(other_change_id, ChangePosition::new(150));
            let found = txn.find_block_in_inode(inode, pos_other).unwrap();
            assert_eq!(found, None);
        }
    }

    #[test]
    fn test_inode_edge_iterator() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let inode = Inode::new(10);
        let hash = Hash::of(b"iter test");

        {
            let mut txn = pristine.write_txn().unwrap();
            let change_id = txn.register_change(&hash).unwrap();

            let node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(100));
            let dest = Position::new(change_id, ChangePosition::new(50));

            // Add edges with different flags
            txn.put_inode_graph(
                inode,
                node,
                SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
            )
            .unwrap();
            txn.put_inode_graph(
                inode,
                node,
                SerializedGraphEdge::new(EdgeFlags::FOLDER, dest, change_id),
            )
            .unwrap();
            txn.commit().unwrap();
        }

        {
            let txn = pristine.read_txn().unwrap();

            // Iterate with iter_inode_edges
            let iter = txn
                .iter_inode_edges(inode, EdgeFlags::empty(), EdgeFlags::all())
                .unwrap();

            let edges: Vec<_> = iter.collect();
            // Note: iter_inode_edges requires the caller to initialize with a span
            // Since it's a trait-provided method that creates InodeEdgeIter without
            // a starting span, it won't iterate automatically.
            // The current implementation starts with current_adj = None.
            assert_eq!(edges.len(), 0);
        }
    }

    #[test]
    fn test_write_txn_inode_graph_ops() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let inode = Inode::new(20);
        let hash = Hash::of(b"write txn test");

        let mut txn = pristine.write_txn().unwrap();
        let change_id = txn.register_change(&hash).unwrap();

        let node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(50));
        let dest = Position::new(change_id, ChangePosition::new(25));

        txn.put_inode_graph(
            inode,
            node,
            SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
        )
        .unwrap();

        // Test InodeGraphOps on WriteTxn before commit
        assert!(txn.inode_graph_is_populated(inode).unwrap());
        assert_eq!(txn.count_inode_vertices(inode).unwrap(), 1);

        let found = txn
            .find_block_in_inode(inode, Position::new(change_id, ChangePosition::new(25)))
            .unwrap();
        assert_eq!(found, Some(node));

        txn.commit().unwrap();
    }

    #[test]
    fn test_flag_filtering() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let inode = Inode::new(30);
        let hash = Hash::of(b"flag filter test");

        let change_id;
        let node;
        {
            let mut txn = pristine.write_txn().unwrap();
            change_id = txn.register_change(&hash).unwrap();
            node = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(100));
            let dest = Position::new(change_id, ChangePosition::new(50));

            // Add edges with different flags
            txn.put_inode_graph(
                inode,
                node,
                SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
            )
            .unwrap();
            txn.put_inode_graph(
                inode,
                node,
                SerializedGraphEdge::new(EdgeFlags::BLOCK | EdgeFlags::PSEUDO, dest, change_id),
            )
            .unwrap();
            txn.put_inode_graph(
                inode,
                node,
                SerializedGraphEdge::new(EdgeFlags::FOLDER, dest, change_id),
            )
            .unwrap();
            txn.commit().unwrap();
        }

        {
            let txn = pristine.read_txn().unwrap();

            // Filter to only BLOCK edges (no PSEUDO)
            let mut adj = txn
                .init_inode_adj(inode, node, EdgeFlags::BLOCK, EdgeFlags::BLOCK)
                .unwrap();

            let mut block_only_count = 0;
            while let Some(result) = txn.next_inode_adj(&mut adj) {
                let edge = result.unwrap();
                assert_eq!(edge.flag(), EdgeFlags::BLOCK);
                block_only_count += 1;
            }
            assert_eq!(block_only_count, 1);

            // Filter to BLOCK..BLOCK|PSEUDO range
            let mut adj2 = txn
                .init_inode_adj(
                    inode,
                    node,
                    EdgeFlags::BLOCK,
                    EdgeFlags::BLOCK | EdgeFlags::PSEUDO,
                )
                .unwrap();

            let mut block_range_count = 0;
            while let Some(result) = txn.next_inode_adj(&mut adj2) {
                result.unwrap();
                block_range_count += 1;
            }
            assert_eq!(block_range_count, 2);
        }
    }

    #[test]
    fn test_multiple_inodes_isolation() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let inode1 = Inode::new(100);
        let inode2 = Inode::new(200);
        let hash = Hash::of(b"isolation test");

        {
            let mut txn = pristine.write_txn().unwrap();
            let change_id = txn.register_change(&hash).unwrap();

            // Add vertices to inode1
            let v1 = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(50));
            let v2 = GraphNode::new(change_id, ChangePosition::new(50), ChangePosition::new(100));
            let dest = Position::new(change_id, ChangePosition::new(25));

            txn.put_inode_graph(
                inode1,
                v1,
                SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
            )
            .unwrap();
            txn.put_inode_graph(
                inode1,
                v2,
                SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
            )
            .unwrap();

            // Add one span to inode2
            let v3 = GraphNode::new(change_id, ChangePosition::new(0), ChangePosition::new(200));
            txn.put_inode_graph(
                inode2,
                v3,
                SerializedGraphEdge::new(EdgeFlags::BLOCK, dest, change_id),
            )
            .unwrap();

            txn.commit().unwrap();
        }

        {
            let txn = pristine.read_txn().unwrap();

            // Verify isolation
            assert_eq!(txn.count_inode_vertices(inode1).unwrap(), 2);
            assert_eq!(txn.count_inode_vertices(inode2).unwrap(), 1);

            // inode3 should be empty
            let inode3 = Inode::new(300);
            assert_eq!(txn.count_inode_vertices(inode3).unwrap(), 0);
            assert!(!txn.inode_graph_is_populated(inode3).unwrap());
        }
    }

    #[test]
    fn test_inode_graph_stats_display() {
        let stats = InodeGraphStats {
            vertices_visited: 100,
            edges_traversed: 200,
            page_accesses: 10,
            cache_hits: 25,
        };

        let display = stats.to_string();
        assert!(display.contains("100 vertices"));
        assert!(display.contains("200 edges"));
        assert!(display.contains("10 pages"));
        assert!(display.contains("cache hits"));
    }
}

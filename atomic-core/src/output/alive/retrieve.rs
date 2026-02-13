//! Graph retrieval from the pristine database
//!
//! This module handles building an [`AliveGraph`] by traversing the repository
//! graph starting from a file's inode position. It discovers all alive (non-deleted)
//! vertices that make up the file's content.
//!
//! # Overview
//!
//! Graph retrieval is the first step in outputting a file's content:
//!
//! 1. Start from the file's inode position (stored in the tree)
//! 2. Follow forward edges to discover content vertices
//! 3. Skip deleted vertices (unless `include_deleted` is set)
//! 4. Mark zombie vertices (deleted but with live connections)
//! 5. Build the `AliveGraph` with all discovered vertices and edges
//!
//! # Algorithm
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         Retrieve Algorithm                               │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  1. Initialize                        2. Process Stack                  │
//! │  ┌─────────────────────────┐         ┌─────────────────────────┐        │
//! │  │ - Create empty graph    │         │ while stack not empty:  │        │
//! │  │ - Add DUMMY at index 0  │ ──────▶ │   - Pop span          │        │
//! │  │ - Add root at index 1   │         │   - Get adjacent edges  │        │
//! │  │ - Push root to stack    │         │   - For each forward:   │        │
//! │  │ - Create position cache │         │     - Find/add span   │        │
//! │  └─────────────────────────┘         │     - Add to children   │        │
//! │                                      │     - Push if new       │        │
//! │                                      └─────────────────────────┘        │
//! │                                                                         │
//! │  3. Span Classification                                               │
//! │  ┌─────────────────────────────────────────────────────────────┐        │
//! │  │ For each position:                                          │        │
//! │  │ - Find block containing position                            │        │
//! │  │ - Check if alive (has non-deleted parent edges)             │        │
//! │  │ - Check if zombie (deleted but has live parents)            │        │
//! │  │ - Skip if not alive and not include_deleted                 │        │
//! │  └─────────────────────────────────────────────────────────────┘        │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Performance
//!
//! The retrieval algorithm is O(V + E) where V is the number of vertices and
//! E is the number of edges in the file's subgraph. A position cache prevents
//! revisiting the same positions.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::alive::{retrieve_graph, RetrieveOptions};
//!
//! // Basic retrieval (exclude deleted content)
//! let graph = retrieve_graph(&txn, file_position, RetrieveOptions::default())?;
//!
//! // Include deleted content (for showing conflicts)
//! let options = RetrieveOptions::new().include_deleted(true);
//! let graph_with_deleted = retrieve_graph(&txn, file_position, options)?;
//! ```

use super::graph::AliveGraph;
use super::vertex::{AliveVertex, VertexFlags, VertexId};
use crate::pristine::{GraphTxnT, PristineError};
use crate::types::{EdgeFlags, GraphNode, NodeId, Position, SerializedGraphEdge};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// ============================================================================
// RETRIEVE OPTIONS
// ============================================================================

/// Options for graph retrieval.
///
/// These options control what content is included in the retrieved graph.
/// The `change_filter` option enables state-based content retrieval, which
/// is essential for showing what a specific change modified.
///
/// # State-Based Retrieval
///
/// When reviewing a specific change, you want to see:
/// 1. The file content BEFORE the change (parent state)
/// 2. The file content AFTER the change (current state)
///
/// This is achieved by setting `change_filter` to only include vertices
/// from changes applied up to a certain point:
///
/// ```text
/// Change Sequence:  [0]  [1]  [2]  [3]  [4]  [5]  ...
///                    │    │    │    │    │    │
///                    ▼    ▼    ▼    ▼    ▼    ▼
/// Parent State: ────────────────────┘    │
/// (filter includes 0-3)                  │
///                                        │
/// Current State: ────────────────────────┘
/// (filter includes 0-4)
/// ```
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::alive::{retrieve_graph, RetrieveOptions};
/// use std::collections::HashSet;
///
/// // Retrieve content at a specific state (e.g., before change at seq 5)
/// let changes_before: HashSet<NodeId> = get_changes_up_to_sequence(&txn, &stack, 5)?;
/// let options = RetrieveOptions::new().with_change_filter(changes_before);
/// let parent_graph = retrieve_graph(&txn, file_pos, options)?;
///
/// // Retrieve content after the change (includes change at seq 5)
/// let changes_after: HashSet<NodeId> = get_changes_up_to_sequence(&txn, &stack, 6)?;
/// let options = RetrieveOptions::new().with_change_filter(changes_after);
/// let current_graph = retrieve_graph(&txn, file_pos, options)?;
/// ```
#[derive(Debug, Clone, Default)]
pub struct RetrieveOptions {
    /// Include deleted vertices in the graph.
    ///
    /// When true, vertices that have been deleted will be included and
    /// marked appropriately. This is useful for showing conflict content.
    pub include_deleted: bool,

    /// Maximum number of vertices to retrieve.
    ///
    /// If set, retrieval will stop after this many vertices. This can
    /// prevent runaway retrieval on corrupted or very large files.
    pub max_vertices: Option<usize>,

    /// Optional filter to only include vertices from specific changes.
    ///
    /// When set, only vertices whose `change_id` is in this set (or is ROOT)
    /// will be included in the retrieved graph. This enables state-based
    /// content retrieval for showing what a specific change modified.
    ///
    /// # Usage
    ///
    /// - To get content at parent state: filter = changes applied BEFORE the change
    /// - To get content at current state: filter = changes applied UP TO AND INCLUDING the change
    ///
    /// The filter is wrapped in `Arc` for efficient cloning and sharing.
    pub change_filter: Option<Arc<HashSet<NodeId>>>,
}

impl RetrieveOptions {
    /// Create new options with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to include deleted vertices.
    ///
    /// When true, vertices marked as deleted will be included in the graph.
    /// This is useful for showing conflict content or full history.
    pub fn include_deleted(mut self, include: bool) -> Self {
        self.include_deleted = include;
        self
    }

    /// Set the maximum number of vertices to retrieve.
    ///
    /// This provides a safety limit for retrieval. If the graph exceeds
    /// this size, retrieval will stop and the result will be truncated.
    pub fn max_vertices(mut self, max: usize) -> Self {
        self.max_vertices = Some(max);
        self
    }

    /// Set a change filter for state-based content retrieval.
    ///
    /// Only vertices from changes in this set (or ROOT) will be included.
    /// This enables retrieving file content at a specific historical state.
    ///
    /// # Arguments
    ///
    /// * `filter` - Set of change NodeIds to include
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Get changes applied before sequence 5
    /// let changes = get_changes_up_to_sequence(&txn, &stack, 5)?;
    /// let options = RetrieveOptions::new().with_change_filter(changes);
    /// let graph = retrieve_graph(&txn, pos, options)?;
    /// ```
    pub fn with_change_filter(mut self, filter: HashSet<NodeId>) -> Self {
        self.change_filter = Some(Arc::new(filter));
        self
    }

    /// Set a change filter from an existing Arc (avoids cloning).
    ///
    /// Use this when you want to share the same filter across multiple
    /// retrieval operations for efficiency.
    pub fn with_change_filter_arc(mut self, filter: Arc<HashSet<NodeId>>) -> Self {
        self.change_filter = Some(filter);
        self
    }

    /// Check if a change ID passes the filter.
    ///
    /// Returns true if:
    /// - No filter is set (all changes pass)
    /// - The change_id is ROOT (NodeId(0), always passes)
    /// - The change_id is in the filter set
    pub fn passes_filter(&self, change_id: NodeId) -> bool {
        match &self.change_filter {
            None => true, // No filter, all pass
            Some(filter) => {
                // ROOT always passes (it's the origin of the graph)
                if change_id == NodeId::ROOT {
                    return true;
                }
                filter.contains(&change_id)
            }
        }
    }

    /// Check if a filter is active.
    pub fn has_filter(&self) -> bool {
        self.change_filter.is_some()
    }

    /// Get the edge flags to use for adjacency queries.
    ///
    /// When a change filter is set, we always include DELETED edges because
    /// we need to traverse them to find content that was deleted by changes
    /// OUTSIDE our filter (which means the content was still alive at the
    /// target state).
    pub(crate) fn edge_flags(&self) -> (EdgeFlags, EdgeFlags) {
        let min = EdgeFlags::empty();
        // When we have a change filter, always include DELETED edges so we can
        // find content that was deleted after our target state
        let max = if self.include_deleted || self.change_filter.is_some() {
            EdgeFlags::PSEUDO | EdgeFlags::BLOCK | EdgeFlags::DELETED
        } else {
            EdgeFlags::PSEUDO | EdgeFlags::BLOCK
        };
        (min, max)
    }

    /// Check if a span should be considered "alive" at the target state.
    ///
    /// When using a change filter for state-based retrieval, a span is alive if:
    /// 1. The span was created by a change in the filter set
    /// 2. Any DELETED edges TO this span were introduced by changes OUTSIDE the filter
    ///
    /// This handles the case where content was deleted by a later change - at the
    /// target state, that deletion hadn't happened yet.
    ///
    /// # Arguments
    ///
    /// * `edge` - The edge we followed to reach the span
    /// * `introduced_by` - The change that introduced this edge
    ///
    /// # Returns
    ///
    /// True if the span should be considered alive at the target state.
    pub fn is_alive_at_target_state(&self, edge: &SerializedGraphEdge) -> bool {
        // If no filter, use normal aliveness check
        if self.change_filter.is_none() {
            return !edge.flag().contains(EdgeFlags::DELETED);
        }

        // With a filter, check if the DELETED edge was introduced by a change in the filter
        if edge.flag().contains(EdgeFlags::DELETED) {
            let introduced_by = edge.introduced_by();
            // If the deletion was introduced by a change OUTSIDE our filter,
            // then at the target state, this deletion hadn't happened yet,
            // so the span is still alive
            if !self.passes_filter(introduced_by) {
                return true; // Deletion is "in the future" - span is alive
            }
            // Deletion was introduced by a change in the filter - span is deleted
            return false;
        }

        // Non-deleted edge - span is alive
        true
    }

    /// Check if a vertex is alive at the target state by examining all its parent edges.
    ///
    /// This is more thorough than `is_alive_at_target_state` which only checks a single edge.
    /// A vertex is considered deleted at the target state if ANY of its parent edges have
    /// the DELETED flag introduced by a change IN our filter.
    ///
    /// The key insight is:
    /// - A vertex needs at least one "live" parent edge from a change in our filter
    /// - If there's a DELETED edge from a change in our filter, the vertex is deleted
    /// - DELETED edges from changes OUTSIDE our filter are ignored (deletion is "in the future")
    ///
    /// # Arguments
    ///
    /// * `txn` - Transaction for graph lookups
    /// * `vertex` - The vertex to check
    ///
    /// # Returns
    ///
    /// True if the vertex is alive at the target state.
    pub fn is_vertex_alive_at_target<T: GraphTxnT>(
        &self,
        txn: &T,
        vertex: GraphNode<NodeId>,
    ) -> Result<bool, PristineError> {
        // If no filter, use normal aliveness check
        if self.change_filter.is_none() {
            return is_vertex_alive(txn, &vertex);
        }

        // Root is always alive
        if vertex.is_root() {
            return Ok(true);
        }

        // Check all parent edges to this vertex
        // We need to look for:
        // 1. Non-deleted parent edges (from changes in filter) - means vertex is alive
        // 2. DELETED parent edges from changes IN our filter - means vertex was deleted
        let parent_flags = EdgeFlags::PARENT;
        let max_flags = EdgeFlags::all(); // Include deleted edges

        let adj = txn.iter_adjacent(vertex, parent_flags, max_flags)?;

        let mut has_live_parent_in_filter = false;
        let mut deleted_by_filter_change = false;

        for edge_result in adj {
            let edge = edge_result?;
            let flag = edge.flag();

            // Skip pseudo-only edges
            let pseudo_flag = EdgeFlags::PSEUDO | EdgeFlags::PARENT;
            if (flag & pseudo_flag) == EdgeFlags::PSEUDO {
                continue;
            }

            let introduced_by = edge.introduced_by();

            if flag.contains(EdgeFlags::DELETED) {
                // This is a deletion edge - check who introduced the deletion
                if self.passes_filter(introduced_by) {
                    // Deletion was introduced by a change IN our filter
                    // This vertex is deleted at the target state
                    deleted_by_filter_change = true;
                } else {
                    // Deletion was introduced by a change OUTSIDE our filter
                    // This means at our target state, the deletion hadn't happened yet
                    // So this edge counts as a LIVE parent (ignore the DELETED flag)
                    if flag.contains(EdgeFlags::BLOCK) || vertex.is_empty() {
                        has_live_parent_in_filter = true;
                    }
                }
            } else if flag.contains(EdgeFlags::BLOCK) || vertex.is_empty() {
                // Non-deleted parent edge
                has_live_parent_in_filter = true;
            }
        }

        // Vertex is alive if:
        // 1. It has a live parent edge, AND
        // 2. It wasn't deleted by a change in our filter
        //
        // Note: We already verified the vertex's change is in the filter before calling this,
        // so we just need to check if there's any live parent (meaning it was connected)
        // and if it was deleted by something in our filter.
        Ok(has_live_parent_in_filter && !deleted_by_filter_change)
    }
}

impl PartialEq for RetrieveOptions {
    fn eq(&self, other: &Self) -> bool {
        self.include_deleted == other.include_deleted
            && self.max_vertices == other.max_vertices
            && match (&self.change_filter, &other.change_filter) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b) || *a == *b,
                _ => false,
            }
    }
}

impl Eq for RetrieveOptions {}

// ============================================================================
// RETRIEVE RESULT
// ============================================================================

/// Result of a graph retrieval operation.
#[derive(Debug)]
pub struct RetrieveResult {
    /// The retrieved graph.
    pub graph: AliveGraph,

    /// Whether retrieval was truncated due to max_vertices.
    pub truncated: bool,

    /// Number of positions visited (may be more than vertices if some skipped).
    pub positions_visited: usize,

    /// Number of edges traversed.
    pub edges_traversed: usize,
}

impl RetrieveResult {
    /// Create a new retrieve result.
    fn new(graph: AliveGraph) -> Self {
        Self {
            graph,
            truncated: false,
            positions_visited: 0,
            edges_traversed: 0,
        }
    }
}

// ============================================================================
// RETRIEVE FUNCTION
// ============================================================================

/// Retrieve the alive graph for a file starting from a position.
///
/// This function traverses the graph from the given starting position,
/// collecting all alive vertices and their edges into an `AliveGraph`.
///
/// # Arguments
///
/// * `txn` - The transaction providing graph access
/// * `start_pos` - Starting position (typically the file's inode position)
/// * `options` - Retrieval options
///
/// # Returns
///
/// A `RetrieveResult` containing the graph and statistics.
///
/// # Errors
///
/// Returns an error if database access fails.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::alive::{retrieve_graph, RetrieveOptions};
///
/// let result = retrieve_graph(&txn, file_pos, RetrieveOptions::default())?;
/// println!("Retrieved {} vertices", result.graph.len_vertices());
/// ```
pub fn retrieve_graph<T: GraphTxnT>(
    txn: &T,
    start_pos: Position<NodeId>,
    options: RetrieveOptions,
) -> Result<RetrieveResult, PristineError> {
    let mut result = RetrieveResult::new(AliveGraph::new());

    // Span cache to avoid revisiting - keyed by the actual span, not position.
    // This is important because a single position (e.g., position 9) might refer to
    // different vertices: an empty inode span V[9:9] or a content span V[9:23].
    // Using the resolved span as the key ensures we visit each unique span once.
    let mut cache: HashMap<GraphNode<NodeId>, VertexId> = HashMap::new();

    // Add dummy span at index 0
    result.graph.push_vertex(AliveVertex::DUMMY);
    cache.insert(GraphNode::BOTTOM, VertexId::DUMMY);

    // Add the root span (inode span) at index 1
    // But only if it passes the change filter
    let root_vertex = start_pos.inode_node();

    // Check if root span passes the change filter
    if !options.passes_filter(start_pos.change) {
        // Root span is filtered out - return empty graph
        return Ok(result);
    }

    let root_alive = AliveVertex::new(root_vertex);
    result.graph.push_vertex(root_alive);
    cache.insert(root_vertex, VertexId::new(1));

    // DFS traversal stack
    let mut stack = vec![VertexId::new(1)];

    let (min_flag, max_flag) = options.edge_flags();

    while let Some(vid) = stack.pop() {
        // Check span limit
        if let Some(max) = options.max_vertices {
            if result.graph.len_vertices() >= max {
                result.truncated = true;
                break;
            }
        }

        // Mark where this span's children start.
        // We need to set this on the SPECIFIC span we're processing (vid),
        // not just the "last" span, because we may have pushed new vertices
        // during previous iterations.
        let children_start = result.graph.len_children();
        {
            let current_vertex = result.graph.get_vertex_mut(vid).unwrap();
            current_vertex.children = children_start;
        }

        // Get the node to traverse
        let node = result.graph.get_vertex(vid).node;

        // Get adjacent edges from the graph
        let adj = txn.iter_adjacent(node, min_flag, max_flag)?;

        // Collect children for this span
        let mut children_to_add: Vec<(Option<SerializedGraphEdge>, VertexId)> = Vec::new();

        for edge_result in adj {
            let edge = edge_result?;
            result.edges_traversed += 1;

            // Skip parent edges (we want forward edges only)
            if edge.flag().intersects(EdgeFlags::PARENT) {
                continue;
            }

            let dest_pos = edge.dest();

            // First resolve the position to an actual span using find_block.
            // This handles the case where position 9 could refer to either an
            // inode span V[9:9] or a content span V[9:23].
            let resolved_vertex = match txn.find_block(dest_pos) {
                Ok(v) => v,
                Err(_) => continue, // Position doesn't resolve to a span
            };

            // Check if this span passes the change filter
            // This is the key mechanism for state-based content retrieval:
            // only include vertices from changes that existed at the target state
            if !options.passes_filter(resolved_vertex.change) {
                continue; // Span is from a change not in the filter set
            }

            // Check if this span is "alive" at the target state.
            // This handles DELETED edges: if the deletion was introduced by a
            // change OUTSIDE our filter, then at the target state the deletion
            // hadn't happened yet, so the span is still alive.
            if !options.is_alive_at_target_state(&edge) {
                continue; // Span was deleted at the target state
            }

            // Check if we've already visited this resolved span
            let dest_vid = if let Some(&existing) = cache.get(&resolved_vertex) {
                existing
            } else {
                result.positions_visited += 1;

                // When we have a change filter and are doing state-based retrieval,
                // we need to check if the vertex is alive at the target state by
                // examining all its parent edges (not just the edge we followed).
                // Otherwise, use the normal create_alive_vertex check.
                let alive_vertex = if options.has_filter() {
                    // Check if this vertex is alive at the target state
                    if !options.is_vertex_alive_at_target(txn, resolved_vertex)? {
                        continue; // Vertex was deleted at the target state
                    }
                    AliveVertex::new(resolved_vertex)
                } else if let Some(av) = create_alive_vertex(txn, resolved_vertex)? {
                    av
                } else {
                    // Span is not alive, skip
                    continue;
                };

                let new_id = result.graph.push_vertex(alive_vertex);
                cache.insert(resolved_vertex, new_id);
                stack.push(new_id);
                new_id
            };

            // Collect this child (don't add yet, we'll add all at once)
            children_to_add.push((Some(edge), dest_vid));
        }

        // Add sentinel at end of children
        children_to_add.push((None, VertexId::DUMMY));

        // Now add all children and update the count for the span we're processing
        for (edge, child_vid) in children_to_add {
            result.graph.push_child(edge, child_vid);
        }

        // Update children count for the span we processed
        let children_end = result.graph.len_children();
        {
            let current_vertex = result.graph.get_vertex_mut(vid).unwrap();
            current_vertex.n_children = children_end - current_vertex.children;
        }
    }

    Ok(result)
}

/// Create an AliveVertex from an already-resolved span, if it's alive.
///
/// This function:
/// 1. Checks if the span is alive (has non-deleted edges)
/// 2. Checks if it's a zombie (deleted but has live connections)
///
/// # Arguments
///
/// * `txn` - Transaction for graph queries
/// * `span` - The already-resolved span to check
///
/// # Returns
///
/// - `Ok(Some(alive_vertex))` if the span is alive or zombie
/// - `Ok(None)` if the span is not alive and not a zombie
/// - `Err(_)` on database error
fn create_alive_vertex<T: GraphTxnT>(
    txn: &T,
    node: GraphNode<NodeId>,
) -> Result<Option<AliveVertex>, PristineError> {
    // Check if the node is alive
    if !is_vertex_alive(txn, &node)? {
        return Ok(None);
    }

    // Check if it's a zombie (deleted but with live parents)
    let is_zombie = is_vertex_zombie(txn, &node)?;

    let mut alive = AliveVertex::new(node);
    if is_zombie {
        alive.add_flags(VertexFlags::ZOMBIE);
    }

    Ok(Some(alive))
}

/// Create a new AliveVertex for a position, if it's alive.
///
/// This function:
/// 1. Finds the block (span) containing the position
/// 2. Checks if the span is alive (has non-deleted edges)
/// 3. Checks if it's a zombie (deleted but has live connections)
///
/// # Returns
///
/// - `Ok(Some(span))` if the position maps to an alive or zombie span
/// - `Ok(None)` if the span is not alive and not a zombie
/// - `Err(_)` on database error
#[allow(dead_code)]
fn new_vertex_at_position<T: GraphTxnT>(
    txn: &T,
    pos: Position<NodeId>,
) -> Result<Option<AliveVertex>, PristineError> {
    // Find the block containing this position
    let node = match txn.find_block(pos) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    create_alive_vertex(txn, node)
}

/// Check if a node is alive (not fully deleted).
///
/// A node is alive if it has at least one non-deleted edge.
fn is_vertex_alive<T: GraphTxnT>(txn: &T, node: &GraphNode<NodeId>) -> Result<bool, PristineError> {
    // Root node is always alive
    if node.is_root() {
        return Ok(true);
    }

    // Check for any parent edges that are not deleted
    let parent_flags = EdgeFlags::PARENT;
    let max_flags = EdgeFlags::all() - EdgeFlags::DELETED;

    let adj = txn.iter_adjacent(*node, parent_flags, max_flags)?;

    for edge_result in adj {
        let edge = edge_result?;
        let flag = edge.flag();

        // Skip pseudo-only edges
        let pseudo_flag = EdgeFlags::PSEUDO | EdgeFlags::PARENT;
        if (flag & pseudo_flag) == EdgeFlags::PSEUDO {
            continue;
        }

        // If it has a block edge or is empty, it's alive
        if flag.contains(EdgeFlags::BLOCK) || node.is_empty() {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Check if a node is a zombie (deleted but with live connections).
///
/// A zombie node is one that has both:
/// - A deleted parent edge (meaning it was deleted)
/// - A non-deleted parent edge (meaning something still references it)
fn is_vertex_zombie<T: GraphTxnT>(
    txn: &T,
    node: &GraphNode<NodeId>,
) -> Result<bool, PristineError> {
    // Check for deleted block parent edges
    let deleted_flags = EdgeFlags::PARENT | EdgeFlags::DELETED | EdgeFlags::BLOCK;

    let adj = txn.iter_adjacent(*node, deleted_flags, EdgeFlags::all())?;

    for edge_result in adj {
        let edge = edge_result?;
        if edge.flag().contains(deleted_flags) {
            return Ok(true);
        }
    }

    Ok(false)
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    // -------------------------------------------------------------------------
    // RetrieveOptions Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_retrieve_options_default() {
        let opts = RetrieveOptions::default();
        assert!(!opts.include_deleted);
        assert!(opts.max_vertices.is_none());
        assert!(opts.change_filter.is_none());
    }

    #[test]
    fn test_retrieve_options_new() {
        let opts = RetrieveOptions::new();
        assert!(!opts.include_deleted);
        assert!(!opts.has_filter());
    }

    #[test]
    fn test_retrieve_options_include_deleted() {
        let opts = RetrieveOptions::new().include_deleted(true);
        assert!(opts.include_deleted);
    }

    #[test]
    fn test_retrieve_options_max_vertices() {
        let opts = RetrieveOptions::new().max_vertices(100);
        assert_eq!(opts.max_vertices, Some(100));
    }

    #[test]
    fn test_retrieve_options_chaining() {
        let opts = RetrieveOptions::new()
            .include_deleted(true)
            .max_vertices(50);

        assert!(opts.include_deleted);
        assert_eq!(opts.max_vertices, Some(50));
    }

    #[test]
    fn test_retrieve_options_edge_flags_default() {
        let opts = RetrieveOptions::default();
        let (min, max) = opts.edge_flags();

        assert!(min.is_empty());
        assert!(max.contains(EdgeFlags::BLOCK));
        assert!(max.contains(EdgeFlags::PSEUDO));
        assert!(!max.contains(EdgeFlags::DELETED));
    }

    #[test]
    fn test_retrieve_options_edge_flags_include_deleted() {
        let opts = RetrieveOptions::new().include_deleted(true);
        let (min, max) = opts.edge_flags();

        assert!(min.is_empty());
        assert!(max.contains(EdgeFlags::DELETED));
    }

    #[test]
    fn test_retrieve_options_equality() {
        let opts1 = RetrieveOptions::new()
            .include_deleted(true)
            .max_vertices(100);
        let opts2 = RetrieveOptions::new()
            .include_deleted(true)
            .max_vertices(100);

        assert_eq!(opts1, opts2);
    }

    #[test]
    fn test_retrieve_options_clone() {
        let opts1 = RetrieveOptions::new()
            .include_deleted(true)
            .max_vertices(100);
        let opts2 = opts1.clone();

        assert_eq!(opts1, opts2);
    }

    #[test]
    fn test_retrieve_options_debug() {
        let opts = RetrieveOptions::new().include_deleted(true);
        let debug = format!("{:?}", opts);
        assert!(debug.contains("include_deleted"));
    }

    // ========================================================================
    // Change Filter Tests
    // ========================================================================

    #[test]
    fn test_retrieve_options_with_change_filter() {
        let mut filter = HashSet::new();
        filter.insert(NodeId::new(1));
        filter.insert(NodeId::new(2));

        let opts = RetrieveOptions::new().with_change_filter(filter);
        assert!(opts.has_filter());
    }

    #[test]
    fn test_retrieve_options_with_change_filter_arc() {
        let mut filter = HashSet::new();
        filter.insert(NodeId::new(1));
        let arc_filter = Arc::new(filter);

        let opts = RetrieveOptions::new().with_change_filter_arc(arc_filter.clone());
        assert!(opts.has_filter());
    }

    #[test]
    fn test_passes_filter_no_filter() {
        let opts = RetrieveOptions::new();

        // Without filter, all should pass
        assert!(opts.passes_filter(NodeId::ROOT));
        assert!(opts.passes_filter(NodeId::new(1)));
        assert!(opts.passes_filter(NodeId::new(100)));
    }

    #[test]
    fn test_passes_filter_root_always_passes() {
        let mut filter = HashSet::new();
        filter.insert(NodeId::new(1));

        let opts = RetrieveOptions::new().with_change_filter(filter);

        // ROOT should always pass even if not in filter
        assert!(opts.passes_filter(NodeId::ROOT));
    }

    #[test]
    fn test_passes_filter_in_set() {
        let mut filter = HashSet::new();
        filter.insert(NodeId::new(1));
        filter.insert(NodeId::new(2));

        let opts = RetrieveOptions::new().with_change_filter(filter);

        assert!(opts.passes_filter(NodeId::new(1)));
        assert!(opts.passes_filter(NodeId::new(2)));
    }

    #[test]
    fn test_passes_filter_not_in_set() {
        let mut filter = HashSet::new();
        filter.insert(NodeId::new(1));

        let opts = RetrieveOptions::new().with_change_filter(filter);

        assert!(!opts.passes_filter(NodeId::new(2)));
        assert!(!opts.passes_filter(NodeId::new(100)));
    }

    #[test]
    fn test_passes_filter_empty_set() {
        let filter: HashSet<NodeId> = HashSet::new();
        let opts = RetrieveOptions::new().with_change_filter(filter);

        // Empty filter means only ROOT passes
        assert!(opts.passes_filter(NodeId::ROOT));
        assert!(!opts.passes_filter(NodeId::new(1)));
    }

    #[test]
    fn test_retrieve_options_equality_with_filter() {
        let mut filter1 = HashSet::new();
        filter1.insert(NodeId::new(1));

        let mut filter2 = HashSet::new();
        filter2.insert(NodeId::new(1));

        let opts1 = RetrieveOptions::new().with_change_filter(filter1);
        let opts2 = RetrieveOptions::new().with_change_filter(filter2);

        // Same contents should be equal
        assert_eq!(opts1, opts2);
    }

    #[test]
    fn test_retrieve_options_equality_different_filters() {
        let mut filter1 = HashSet::new();
        filter1.insert(NodeId::new(1));

        let mut filter2 = HashSet::new();
        filter2.insert(NodeId::new(2));

        let opts1 = RetrieveOptions::new().with_change_filter(filter1);
        let opts2 = RetrieveOptions::new().with_change_filter(filter2);

        // Different contents should not be equal
        assert_ne!(opts1, opts2);
    }

    #[test]
    fn test_retrieve_options_equality_one_with_filter() {
        let mut filter = HashSet::new();
        filter.insert(NodeId::new(1));

        let opts1 = RetrieveOptions::new().with_change_filter(filter);
        let opts2 = RetrieveOptions::new();

        // One with filter, one without should not be equal
        assert_ne!(opts1, opts2);
    }

    #[test]
    fn test_retrieve_options_clone_with_filter() {
        let mut filter = HashSet::new();
        filter.insert(NodeId::new(1));
        filter.insert(NodeId::new(2));

        let opts1 = RetrieveOptions::new()
            .include_deleted(true)
            .with_change_filter(filter);
        let opts2 = opts1.clone();

        assert_eq!(opts1, opts2);
        assert!(opts2.has_filter());
        assert!(opts2.passes_filter(NodeId::new(1)));
    }

    #[test]
    fn test_retrieve_options_shared_filter_arc() {
        let mut filter = HashSet::new();
        filter.insert(NodeId::new(1));
        let arc = Arc::new(filter);

        let opts1 = RetrieveOptions::new().with_change_filter_arc(arc.clone());
        let opts2 = RetrieveOptions::new().with_change_filter_arc(arc.clone());

        // Both should reference the same Arc
        assert!(Arc::ptr_eq(
            opts1.change_filter.as_ref().unwrap(),
            opts2.change_filter.as_ref().unwrap()
        ));
    }

    // -------------------------------------------------------------------------
    // RetrieveResult Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_retrieve_result_new() {
        let graph = AliveGraph::new();
        let result = RetrieveResult::new(graph);

        assert!(!result.truncated);
        assert_eq!(result.positions_visited, 0);
        assert_eq!(result.edges_traversed, 0);
    }

    #[test]
    fn test_retrieve_result_debug() {
        let graph = AliveGraph::new();
        let result = RetrieveResult::new(graph);
        let debug = format!("{:?}", result);
        assert!(debug.contains("RetrieveResult"));
    }

    // -------------------------------------------------------------------------
    // Position Helper Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_inode_vertex_from_position() {
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
        let inode_vertex = pos.inode_node();

        assert_eq!(inode_vertex.change, NodeId::new(42));
        assert_eq!(inode_vertex.start, ChangePosition::new(100));
        assert_eq!(inode_vertex.end, ChangePosition::new(100));
        assert!(inode_vertex.is_empty());
    }

    #[test]
    fn test_root_position() {
        let pos = Position::ROOT;
        assert_eq!(pos.change, NodeId::ROOT);
    }

    #[test]
    fn test_bottom_position() {
        let pos = Position::BOTTOM;
        assert_eq!(pos.change, NodeId::ROOT);
    }

    // -------------------------------------------------------------------------
    // Span Classification Tests (Unit Tests Without DB)
    // -------------------------------------------------------------------------

    #[test]
    fn test_alive_vertex_zombie_flag() {
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let mut alive = AliveVertex::new(node);
        assert!(!alive.is_zombie());

        alive.add_flags(VertexFlags::ZOMBIE);
        assert!(alive.is_zombie());
    }

    #[test]
    fn test_alive_vertex_empty() {
        let empty_vertex = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(0),
        );

        let alive = AliveVertex::new(empty_vertex);
        assert!(alive.is_empty());
        assert_eq!(alive.len(), 0);
    }

    #[test]
    fn test_alive_vertex_non_empty() {
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(50),
        );

        let alive = AliveVertex::new(node);
        assert!(!alive.is_empty());
        assert_eq!(alive.len(), 50);
    }

    // -------------------------------------------------------------------------
    // Edge Flag Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_parent_flag_detection() {
        let parent_flags = EdgeFlags::PARENT | EdgeFlags::BLOCK;
        assert!(parent_flags.intersects(EdgeFlags::PARENT));
    }

    #[test]
    fn test_deleted_flag_detection() {
        let deleted_flags = EdgeFlags::DELETED | EdgeFlags::BLOCK;
        assert!(deleted_flags.contains(EdgeFlags::DELETED));
    }

    #[test]
    fn test_block_flag_detection() {
        let block_flags = EdgeFlags::BLOCK;
        assert!(block_flags.contains(EdgeFlags::BLOCK));
        assert!(!block_flags.contains(EdgeFlags::DELETED));
    }

    #[test]
    fn test_pseudo_flag_detection() {
        let pseudo_flags = EdgeFlags::PSEUDO;
        assert!(pseudo_flags.contains(EdgeFlags::PSEUDO));
        assert!(!pseudo_flags.contains(EdgeFlags::BLOCK));
    }

    // -------------------------------------------------------------------------
    // Cache Tests (Simulated)
    // -------------------------------------------------------------------------

    #[test]
    fn test_position_cache() {
        let mut cache: HashMap<Position<NodeId>, VertexId> = HashMap::new();

        let pos1 = Position::new(NodeId::new(1), ChangePosition::new(0));
        let pos2 = Position::new(NodeId::new(1), ChangePosition::new(100));
        let pos3 = Position::new(NodeId::new(2), ChangePosition::new(0));

        cache.insert(pos1, VertexId::new(1));
        cache.insert(pos2, VertexId::new(2));
        cache.insert(pos3, VertexId::new(3));

        assert_eq!(cache.get(&pos1), Some(&VertexId::new(1)));
        assert_eq!(cache.get(&pos2), Some(&VertexId::new(2)));
        assert_eq!(cache.get(&pos3), Some(&VertexId::new(3)));

        // Duplicate insertion returns same ID
        assert!(cache.contains_key(&pos1));
    }

    #[test]
    fn test_position_cache_bottom() {
        let mut cache: HashMap<Position<NodeId>, VertexId> = HashMap::new();
        cache.insert(Position::BOTTOM, VertexId::DUMMY);

        assert_eq!(cache.get(&Position::BOTTOM), Some(&VertexId::DUMMY));
    }

    // -------------------------------------------------------------------------
    // Graph Building Tests (Unit Level)
    // -------------------------------------------------------------------------

    #[test]
    fn test_graph_building_basic() {
        let mut graph = AliveGraph::new();

        // Add dummy
        graph.push_vertex(AliveVertex::DUMMY);

        // Add root
        let root = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(0),
        );
        graph.push_vertex(AliveVertex::new(root));

        assert_eq!(graph.len_vertices(), 2);
        assert!(
            graph.get_vertex(VertexId::DUMMY).node.is_root()
                || graph.get_vertex(VertexId::DUMMY).is_dummy()
        );
    }

    #[test]
    fn test_graph_building_with_children() {
        let mut graph = AliveGraph::new();

        // Add dummy
        graph.push_vertex(AliveVertex::DUMMY);

        // Add root with children setup
        let root = GraphNode::ROOT;
        graph.push_vertex(AliveVertex::new(root));
        graph.set_last_children_start();

        // Add children
        graph.push_child_to_last(None, VertexId::new(2));
        graph.push_child_to_last(None, VertexId::DUMMY); // sentinel

        let children: Vec<_> = graph.children(VertexId::new(1)).collect();
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn test_graph_total_bytes() {
        let mut graph = AliveGraph::new();

        graph.push_vertex(AliveVertex::DUMMY);

        let v1 = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(100),
        );
        let v2 = GraphNode::new(
            NodeId::new(2),
            ChangePosition::new(0),
            ChangePosition::new(50),
        );

        graph.push_vertex(AliveVertex::new(v1));
        graph.push_vertex(AliveVertex::new(v2));

        assert_eq!(graph.total_bytes(), 150);
    }

    // -------------------------------------------------------------------------
    // Edge Cases
    // -------------------------------------------------------------------------

    #[test]
    fn test_empty_graph() {
        let graph = AliveGraph::new();
        assert!(graph.is_empty());
        assert_eq!(graph.len_vertices(), 0);
        assert_eq!(graph.total_bytes(), 0);
    }

    #[test]
    fn test_max_vertices_zero() {
        let opts = RetrieveOptions::new().max_vertices(0);
        assert_eq!(opts.max_vertices, Some(0));
    }

    #[test]
    fn test_max_vertices_large() {
        let opts = RetrieveOptions::new().max_vertices(usize::MAX);
        assert_eq!(opts.max_vertices, Some(usize::MAX));
    }

    #[test]
    fn test_retrieve_result_fields() {
        let graph = AliveGraph::new();
        let mut result = RetrieveResult::new(graph);

        result.truncated = true;
        result.positions_visited = 100;
        result.edges_traversed = 500;

        assert!(result.truncated);
        assert_eq!(result.positions_visited, 100);
        assert_eq!(result.edges_traversed, 500);
    }
}

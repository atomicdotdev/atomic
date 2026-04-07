//! Retrieval options and result types.
//!
//! This module contains [`RetrieveOptions`] for configuring graph retrieval
//! and [`RetrieveResult`] for returning the retrieved graph with statistics.

use super::super::graph::AliveGraph;
use crate::pristine::{GraphTxnT, PristineError};
use crate::types::{EdgeFlags, GraphNode, NodeId, SerializedGraphEdge};
use std::collections::HashSet;
use std::sync::Arc;

// RETRIEVE OPTIONS

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
            return super::classify::is_vertex_alive(txn, &vertex);
        }

        // Root is always alive
        if vertex.is_root() {
            return Ok(true);
        }

        // Check parent edges to determine if this vertex is alive at the
        // target state defined by the change filter.
        //
        // Key insight for Replacement operations and divergent stacks:
        //
        // When stack A and stack B both modify the same file, they each
        // create a Replacement that deletes the original content vertex
        // and adds a new one.  In the global graph the original vertex
        // ends up with TWO DELETED parent edges:
        //
        //   - One introduced by stack A's change (in A's filter)
        //   - One introduced by stack B's change (in B's filter)
        //
        // The original non-deleted parent edge from the creating change
        // was removed by `del_edge_with_reverse` during the first
        // Replacement apply.  So the vertex may have NO non-deleted
        // parent edges at all — only DELETED ones from different changes.
        //
        // A DELETED edge from OUTSIDE the filter means "this vertex was
        // alive, then a change we cannot see deleted it."  From our
        // stack's perspective that deletion has not happened yet, so the
        // vertex IS still alive — UNLESS our own filter also contains a
        // change that explicitly deleted it.
        //
        // Logic:
        //   - NON-deleted parent edge              → live parent
        //   - DELETED parent, introduced OUTSIDE filter → live parent
        //     (the deletion is "in the future" from our perspective)
        //   - DELETED parent, introduced IN filter  → marks vertex dead
        //
        // The vertex is alive when it has at least one live parent AND
        // was not deleted by an in-filter change.

        let mut has_live_parent = false;
        let mut deleted_by_filter_change = false;
        let mut edge_count = 0u32;

        let parent_flags = EdgeFlags::PARENT;
        let max_flags = EdgeFlags::all();
        let adj = txn.iter_adjacent(vertex, parent_flags, max_flags)?;

        for edge_result in adj {
            let edge = edge_result?;
            let flag = edge.flag();

            if !flag.contains(EdgeFlags::PARENT) {
                continue;
            }

            edge_count += 1;
            let introduced_by = edge.introduced_by();
            let in_filter = self.passes_filter(introduced_by);

            log::trace!(
                "is_vertex_alive_at_target: vertex=[{:?} {:?}:{:?}] edge #{} flag={:?} introduced_by={:?} in_filter={}",
                vertex.change, vertex.start, vertex.end,
                edge_count, flag, introduced_by, in_filter
            );

            if flag.contains(EdgeFlags::DELETED) {
                if in_filter {
                    // Deletion from a change in our filter → vertex is dead
                    deleted_by_filter_change = true;
                } else {
                    // Deletion from a change OUTSIDE our filter.
                    // From our perspective that deletion hasn't happened yet,
                    // so this edge still counts as a live connection.
                    if flag.contains(EdgeFlags::BLOCK) || vertex.is_empty() {
                        has_live_parent = true;
                    }
                }
            } else if flag.contains(EdgeFlags::BLOCK) || vertex.is_empty() {
                // Non-deleted parent edge → vertex is connected (alive)
                has_live_parent = true;
            }
        }

        let alive = has_live_parent && !deleted_by_filter_change;
        log::trace!(
            "is_vertex_alive_at_target: vertex=[{:?} {:?}:{:?}] edges={} has_live_parent={} deleted_by_filter={} → alive={}",
            vertex.change, vertex.start, vertex.end,
            edge_count, has_live_parent, deleted_by_filter_change, alive
        );

        // Vertex is alive if it has at least one live parent (non-deleted
        // or deleted-by-outside-change) AND was not explicitly deleted by
        // a change in our filter.
        Ok(alive)
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

// RETRIEVE RESULT

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

    /// Whether a change_filter was active during retrieval.
    ///
    /// When `true` and `graph.is_empty()`, it means the file has no
    /// content on the target stack (all vertices were filtered out).
    /// Callers can use this to distinguish "genuinely empty file" from
    /// "file belongs to a different stack".
    pub was_filtered: bool,
}

impl RetrieveResult {
    /// Create a new retrieve result.
    pub(super) fn new(graph: AliveGraph) -> Self {
        Self {
            graph,
            truncated: false,
            positions_visited: 0,
            edges_traversed: 0,
            was_filtered: false,
        }
    }
}

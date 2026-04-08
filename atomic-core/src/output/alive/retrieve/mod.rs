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

mod classify;
mod options;
#[cfg(test)]
mod tests;

pub use options::{RetrieveOptions, RetrieveResult};

use classify::create_alive_vertex;

use super::graph::AliveGraph;
use super::vertex::{AliveVertex, VertexId};
use crate::pristine::{GraphTxnT, PristineError};
use crate::types::{GraphNode, NodeId, Position, SerializedGraphEdge};
use std::collections::HashMap;

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
        result.was_filtered = options.has_filter();
        return Ok(result);
    }

    // Track whether a change filter is active so callers can distinguish
    // "genuinely empty file" from "file belongs to a different stack".
    result.was_filtered = options.has_filter();

    let root_alive = AliveVertex::new(root_vertex);
    result.graph.push_vertex(root_alive);
    cache.insert(root_vertex, VertexId::new(1));

    // DFS traversal stack
    let mut stack = vec![VertexId::new(1)];

    // Determine whether iter_forward should include deleted edges.
    // When a change filter is active we need to see deleted edges so we can
    // decide whether the deletion "has happened" from our view's perspective.
    let include_deleted = options.include_deleted_edges();

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

        // Get typed forward edges — parent edges are excluded at the type
        // level, so no manual `if PARENT { continue }` guard is needed.
        let forward_edges = txn.iter_forward(node, include_deleted)?;

        // Collect children for this span
        let mut children_to_add: Vec<(Option<SerializedGraphEdge>, VertexId)> = Vec::new();

        for edge in forward_edges {
            result.edges_traversed += 1;

            let dest_pos = edge.dest;

            // First resolve the position to an actual span using find_block.
            // This handles the case where position 9 could refer to either an
            // inode span V[9:9] or a content span V[9:23].
            let resolved_vertex = match txn.find_block(dest_pos) {
                Ok(v) => v,
                Err(_) => continue, // Position doesn't resolve to a span
            };

            // Check if this span passes the change filter.
            // This is the key mechanism for state-based content retrieval:
            // only include vertices from changes that existed at the target state.
            if !options.passes_filter(resolved_vertex.change) {
                continue; // Span is from a change not in the filter set
            }

            // Single-edge alive check using the typed EdgeKind.
            // With no filter, deleted edges are skipped.
            // With a filter, a deleted edge is skipped only if its introducing
            // change is IN the filter (meaning the deletion has happened from
            // our view's perspective).
            if !options.is_edge_alive(&edge) {
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
                    // Full vertex aliveness check using typed parent iteration
                    if !options.is_vertex_alive(txn, resolved_vertex)? {
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

            // Convert the typed ForwardEdge back to a SerializedGraphEdge
            // for storage in the AliveGraph children list, which uses the
            // wire format.
            let serialized =
                SerializedGraphEdge::new(edge.kind.to_flags(), edge.dest, edge.introduced_by);

            // Collect this child (don't add yet, we'll add all at once)
            children_to_add.push((Some(serialized), dest_vid));
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

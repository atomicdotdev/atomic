//! Graph content output.
//!
//! This module provides the [`output_graph_content`] function, which writes the
//! content of an alive graph to a [`VertexBuffer`]. It handles conflict detection
//! and marker insertion for cyclic, order, and zombie conflicts.
//!
//! # Overview
//!
//! After retrieving an alive graph and computing its SCC ordering, this module
//! outputs the content in the correct order, inserting conflict markers where
//! necessary:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                       Content Output Pipeline                            │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  AliveGraph         OrderResult           VertexBuffer                  │
//! │  ┌──────────┐      ┌──────────┐          ┌──────────┐                  │
//! │  │ Vertices │ ───► │ SCCs     │ ───────► │ Content  │                  │
//! │  │ Edges    │      │ Order    │          │ Markers  │                  │
//! │  │ Flags    │      │ Conflicts│          │          │                  │
//! │  └──────────┘      └──────────┘          └──────────┘                  │
//! │                                                                         │
//! │  For each SCC (in order):                                               │
//! │    1. Single span → output directly                                   │
//! │    2. Multiple vertices → output as cyclic conflict                     │
//! │    3. Zombie vertices → wrap in zombie markers                          │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Conflict Handling
//!
//! ## Cyclic Conflicts
//!
//! When an SCC contains multiple vertices, there's a cycle in the graph,
//! meaning no clear ordering exists. We output all vertices with conflict
//! markers:
//!
//! ```text
//! >>>>>>> 1 [cyclic]
//! Content from span A
//! ======= 1
//! Content from span B
//! <<<<<<< 1
//! ```
//!
//! ## Zombie Conflicts
//!
//! Zombie vertices are deleted content that still has live connections.
//! They're wrapped in special markers:
//!
//! ```text
//! >>>>>>> 1 [zombie]
//! Deleted but modified content
//! <<<<<<< 1
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::repo::output_graph_content;
//! use atomic_core::output::alive::{retrieve_graph, compute_order};
//!
//! // Retrieve and order the graph
//! let result = retrieve_graph(&txn, position, Default::default())?;
//! let order = compute_order(&mut result.graph);
//!
//! // Create a writer
//! let mut buffer = Vec::new();
//! let mut writer = ConflictWriter::new(&mut buffer, "file.rs", position);
//!
//! // Output the content
//! output_graph_content(&changes, &hash_fn, &result.graph, &order, &mut writer)?;
//! ```

use crate::change::ChangeStore;
use crate::output::alive::{AliveGraph, OrderResult};
use crate::output::traits::VertexBuffer;
use crate::types::{Hash, NodeId};

use super::error::{OutputError, OutputResult};

// ============================================================================
// OUTPUT GRAPH CONTENT
// ============================================================================

/// Output the content of an alive graph to a span buffer.
///
/// This function traverses the graph in SCC order (as computed by
/// [`compute_order`](crate::output::alive::compute_order)) and writes each
/// span's content to the buffer. Conflicts are handled by:
///
/// - **Cyclic conflicts**: Multi-span SCCs are output with conflict markers
/// - **Zombie content**: Deleted vertices with live edges get zombie markers
///
/// # Arguments
///
/// * `changes` - Change store for retrieving span content
/// * `hash_fn` - Function to convert NodeId to Hash (for conflict markers)
/// * `graph` - The alive graph containing vertices to output
/// * `order` - The computed SCC ordering
/// * `buffer` - The span buffer to write to
///
/// # Returns
///
/// `Ok(())` on success, or an error if content retrieval or writing fails.
///
/// # Errors
///
/// Returns an error if:
/// - Content cannot be retrieved from the change store
/// - Writing to the buffer fails
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::repo::output_graph_content;
/// use atomic_core::change::MemoryChangeStore;
///
/// let changes = MemoryChangeStore::new();
/// let hash_fn = |id: NodeId| txn.get_external(id).ok().flatten();
///
/// output_graph_content(&changes, hash_fn, &graph, &order, &mut writer)?;
/// ```
///
/// # Algorithm
///
/// 1. Iterate over SCCs in topological order
/// 2. For each SCC:
///    - If single span: output content directly
///    - If multiple vertices: begin cyclic conflict, output each, end conflict
/// 3. For zombie vertices: wrap in zombie conflict markers
/// 4. Track and report any content retrieval errors
pub fn output_graph_content<C, F, V>(
    changes: &C,
    hash_fn: F,
    graph: &AliveGraph,
    order: &OrderResult,
    buffer: &mut V,
) -> OutputResult<()>
where
    C: ChangeStore,
    F: Fn(NodeId) -> Option<Hash>,
    V: VertexBuffer,
{
    // Track conflict IDs
    let mut conflict_id: usize = 0;

    // Track zombie state
    let mut in_zombie: Option<usize> = None;

    // Process SCCs in reverse order (Tarjan produces reverse topological order,
    // so we iterate in reverse to get forward topological order for correct output)
    for scc in order.sccs.iter().rev() {
        // Skip empty SCCs (shouldn't happen, but be safe)
        if scc.is_empty() {
            continue;
        }

        // Check if this is a cyclic conflict (multi-span SCC)
        let is_cyclic = scc.len() > 1;

        if is_cyclic {
            conflict_id += 1;
            buffer
                .begin_cyclic_conflict(conflict_id)
                .map_err(OutputError::io)?;
        }

        // Output each span in the SCC
        for (i, &vertex_id) in scc.iter().enumerate() {
            // Get span data
            let vertex_data = match graph.try_get_vertex(vertex_id) {
                Some(v) => v,
                None => continue,
            };

            let node = vertex_data.node;

            // Handle zombie state transitions
            let is_zombie = vertex_data.is_zombie();

            if is_zombie && in_zombie.is_none() {
                // Entering zombie region
                conflict_id += 1;
                in_zombie = Some(conflict_id);

                let hash = hash_fn(node.change);
                let hashes: Vec<Hash> = hash.into_iter().collect();
                let hashes_ref: Option<&[Hash]> = if hashes.is_empty() {
                    None
                } else {
                    Some(&hashes)
                };

                buffer
                    .begin_zombie_conflict(conflict_id, hashes_ref)
                    .map_err(OutputError::io)?;
            } else if !is_zombie {
                // Exiting zombie region if we were in one
                if let Some(zombie_id) = in_zombie.take() {
                    buffer
                        .end_zombie_conflict(zombie_id)
                        .map_err(OutputError::io)?;
                }
            }

            // For cyclic conflicts, add separator between vertices
            if is_cyclic && i > 0 {
                let hash = hash_fn(node.change);
                let hashes: Vec<Hash> = hash.into_iter().collect();
                let hashes_ref: Option<&[Hash]> = if hashes.is_empty() {
                    None
                } else {
                    Some(&hashes)
                };

                buffer
                    .conflict_next(conflict_id, hashes_ref)
                    .map_err(OutputError::io)?;
            }

            // Skip empty vertices
            let vertex_len = node.end.get() - node.start.get();
            if vertex_len == 0 {
                continue;
            }

            // Output the node content
            let get_contents = |buf: &mut [u8]| -> Result<(), std::io::Error> {
                changes
                    .get_contents(&hash_fn, node, buf)
                    .map(|_| ())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
            };

            buffer
                .output_line(node, get_contents)
                .map_err(OutputError::io)?;
        }

        // End cyclic conflict if we started one
        if is_cyclic {
            // Close any open zombie first
            if let Some(zombie_id) = in_zombie.take() {
                buffer
                    .end_zombie_conflict(zombie_id)
                    .map_err(OutputError::io)?;
            }

            buffer
                .end_cyclic_conflict(conflict_id)
                .map_err(OutputError::io)?;
        }
    }

    // Close any remaining zombie conflict
    if let Some(zombie_id) = in_zombie {
        buffer
            .end_zombie_conflict(zombie_id)
            .map_err(OutputError::io)?;
    }

    Ok(())
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{Change, ChangeHeader, MemoryChangeStore};
    use crate::output::alive::{AliveGraph, AliveVertex, OrderResult, VertexFlags, VertexId};
    use crate::output::repo::ConflictWriter;
    use crate::types::{ChangePosition, GraphNode, Position};

    /// Create a test span
    fn make_vertex(change: u64, start: u64, end: u64) -> GraphNode<NodeId> {
        GraphNode::new(
            NodeId::new(change),
            ChangePosition::new(start),
            ChangePosition::new(end),
        )
    }

    /// Create a test change with content
    fn make_change(content: &[u8]) -> Change {
        let mut change = Change::empty(ChangeHeader::new("test"));
        change.contents = content.to_vec();
        change
    }

    /// Create a minimal alive graph with one node
    fn make_simple_graph(node: GraphNode<NodeId>) -> AliveGraph {
        let mut graph = AliveGraph::new();
        // Push dummy vertex first (required at index 0)
        graph.push_vertex(AliveVertex::DUMMY);
        // Push our actual node
        graph.push_vertex(AliveVertex::new(node));
        graph
    }

    /// Create a simple order result with one SCC containing one vertex
    /// Note: index 0 is DUMMY, so our vertex is at index 1
    fn make_simple_order() -> OrderResult {
        OrderResult {
            sccs: vec![vec![VertexId(1)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        }
    }

    // ------------------------------------------------------------------------
    // Basic Output Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_empty_graph() {
        let graph = AliveGraph::new();
        let order = OrderResult {
            sccs: Vec::new(),
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        let changes = MemoryChangeStore::new();
        let hash_fn = |_: NodeId| None;

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());
        }
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_output_single_vertex() {
        let content = b"Hello, world!";
        let node = make_vertex(1, 0, content.len() as u64);
        let graph = make_simple_graph(node);
        let order = make_simple_order();

        let changes = MemoryChangeStore::new();
        let change = make_change(content);
        let hash = change.hash().unwrap();
        changes.insert(hash, change);

        let hash_fn = |id: NodeId| {
            if id.get() == 1 {
                Some(hash)
            } else {
                None
            }
        };

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());
        }
        assert_eq!(&buffer, content);
    }

    #[test]
    fn test_output_empty_vertex_skipped() {
        // Empty node (start == end)
        let node = make_vertex(1, 0, 0);
        let graph = make_simple_graph(node);
        let order = make_simple_order();

        let changes = MemoryChangeStore::new();
        let hash_fn = |_: NodeId| None;

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());
        }
        assert!(buffer.is_empty()); // Empty node produces no output
    }

    // ------------------------------------------------------------------------
    // Cyclic Conflict Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_cyclic_conflict() {
        let content1 = b"Side A\n";
        let content2 = b"Side B\n";

        let vertex1 = make_vertex(1, 0, content1.len() as u64);
        let vertex2 = make_vertex(2, 0, content2.len() as u64);

        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);
        graph.push_vertex(AliveVertex::new(vertex1));
        graph.push_vertex(AliveVertex::new(vertex2));

        // Both vertices in same SCC = cyclic conflict (indices 1 and 2, since 0 is DUMMY)
        let order = OrderResult {
            sccs: vec![vec![VertexId(1), VertexId(2)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 1,
            forward_edges: Vec::new(),
        };

        let changes = MemoryChangeStore::new();

        let change1 = make_change(content1);
        let hash1 = change1.hash().unwrap();
        changes.insert(hash1, change1);

        let change2 = make_change(content2);
        let hash2 = change2.hash().unwrap();
        changes.insert(hash2, change2);

        let hash_fn = |id: NodeId| match id.get() {
            1 => Some(hash1),
            2 => Some(hash2),
            _ => None,
        };

        let mut buffer = Vec::new();
        let has_conflicts;
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());

            // Check conflicts while writer is still alive
            has_conflicts = writer.has_conflicts();
        }

        let output = String::from_utf8(buffer).unwrap();

        // Should have conflict markers
        assert!(output.contains(">>>>>>>"));
        assert!(output.contains("======="));
        assert!(output.contains("<<<<<<<"));

        // Should have both sides
        assert!(output.contains("Side A"));
        assert!(output.contains("Side B"));

        // Should have recorded a conflict
        assert!(has_conflicts);
    }

    // ------------------------------------------------------------------------
    // Multiple SCC Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_multiple_sccs() {
        let content1 = b"First\n";
        let content2 = b"Second\n";

        let vertex1 = make_vertex(1, 0, content1.len() as u64);
        let vertex2 = make_vertex(2, 0, content2.len() as u64);

        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);
        graph.push_vertex(AliveVertex::new(vertex1));
        graph.push_vertex(AliveVertex::new(vertex2));

        // Two separate SCCs (no conflict) - indices 1 and 2 since 0 is DUMMY
        let order = OrderResult {
            sccs: vec![vec![VertexId(1)], vec![VertexId(2)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        let changes = MemoryChangeStore::new();

        let change1 = make_change(content1);
        let hash1 = change1.hash().unwrap();
        changes.insert(hash1, change1);

        let change2 = make_change(content2);
        let hash2 = change2.hash().unwrap();
        changes.insert(hash2, change2);

        let hash_fn = |id: NodeId| match id.get() {
            1 => Some(hash1),
            2 => Some(hash2),
            _ => None,
        };

        let mut buffer = Vec::new();
        let has_conflicts;
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());

            // Check conflicts while writer is still alive
            has_conflicts = writer.has_conflicts();
        }

        let output = String::from_utf8(buffer).unwrap();

        // Both contents should appear, no conflict markers
        assert!(output.contains("First"));
        assert!(output.contains("Second"));
        assert!(!output.contains(">>>>>>>"));

        // No conflicts
        assert!(!has_conflicts);
    }

    // ------------------------------------------------------------------------
    // Zombie Conflict Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_zombie_vertex() {
        let content = b"Zombie content\n";
        let node = make_vertex(1, 0, content.len() as u64);

        let mut graph = AliveGraph::new();
        graph.push_vertex(AliveVertex::DUMMY);
        let mut alive_vertex = AliveVertex::new(node);
        alive_vertex.mark_zombie(); // Mark as zombie
        graph.push_vertex(alive_vertex);

        let order = make_simple_order();

        let changes = MemoryChangeStore::new();
        let change = make_change(content);
        let hash = change.hash().unwrap();
        changes.insert(hash, change);

        let hash_fn = |id: NodeId| {
            if id.get() == 1 {
                Some(hash)
            } else {
                None
            }
        };

        let mut buffer = Vec::new();
        let has_conflicts;
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());

            // Check conflicts while writer is still alive
            has_conflicts = writer.has_conflicts();
        }

        let output = String::from_utf8(buffer).unwrap();

        // Should have zombie markers
        assert!(output.contains(">>>>>>>"));
        assert!(output.contains("<<<<<<<"));
        assert!(output.contains("Zombie content"));

        // Should have recorded a zombie conflict
        assert!(has_conflicts);
    }

    // ------------------------------------------------------------------------
    // Empty SCC Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_skips_empty_scc() {
        let content = b"Content\n";
        let node = make_vertex(1, 0, content.len() as u64);
        let graph = make_simple_graph(node);

        // Order with an empty SCC (shouldn't happen, but handle gracefully)
        let order = OrderResult {
            sccs: vec![vec![], vec![VertexId(1)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        let changes = MemoryChangeStore::new();
        let change = make_change(content);
        let hash = change.hash().unwrap();
        changes.insert(hash, change);

        let hash_fn = |id: NodeId| {
            if id.get() == 1 {
                Some(hash)
            } else {
                None
            }
        };

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());
        }
        assert_eq!(&buffer, content);
    }

    // ------------------------------------------------------------------------
    // Missing Span Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_skips_missing_vertex() {
        let graph = AliveGraph::new(); // Empty graph

        // Order references a span that doesn't exist
        let order = OrderResult {
            sccs: vec![vec![VertexId(999)]],
            conflict_tree: Default::default(),
            cyclic_conflicts: 0,
            forward_edges: Vec::new(),
        };

        let changes = MemoryChangeStore::new();
        let hash_fn = |_: NodeId| None;

        let mut buffer = Vec::new();
        {
            let pos = Position::ROOT;
            let mut writer = ConflictWriter::new(&mut buffer, "test.rs", pos);

            let result = output_graph_content(&changes, hash_fn, &graph, &order, &mut writer);
            assert!(result.is_ok());
        }
        assert!(buffer.is_empty()); // Missing span produces no output
    }
}

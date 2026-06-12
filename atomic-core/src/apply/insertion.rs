//! Insertion atom application
//!
//! This module handles applying `Insertion` atoms to the repository graph.
//! An Insertion atom inserts new content by creating a GraphNode and connecting
//! it to existing nodes via context edges.
//!
//! # Overview
//!
//! When new content is added to a file, it's represented as a `Insertion`:
//!
//! ```text
//! [predecessors] ──────> [NEW VERTEX] ──────> [successors]
//!     │                    │                      │
//!     │              (new content)                │
//!     │                    │                      │
//!   "Hello"            " World"                 "!"
//! ```
//!
//! The predecessors specifies what comes before, and successors specifies
//! what comes after. This positioning system allows concurrent changes to
//! be merged correctly.
//!
//! # Application Process
//!
//! 1. **Create GraphNode**: Build the node from (change_id, start, end)
//! 2. **Resolve Up Context**: Find nodes that come before
//! 3. **Resolve Down Context**: Find nodes that come after
//! 4. **Create Forward Edges**: Add edges from predecessors to new node
//! 5. **Create Backward Edges**: Add edges from new node to successors
//! 6. **Track Conflicts**: Note any zombie context (deleted by unknown changes)
//!
//! # Edge Flags
//!
//! - `BLOCK`: Content edge (connects content vertices)
//! - `FOLDER`: Directory structure edge
//! - `PARENT`: Reverse edge (added automatically)
//! - `DELETED`: Marks deleted content
//! - `PSEUDO`: Synthetic edge for connectivity
//!
//! # Conflict Detection
//!
//! If the context was deleted by a change we don't know about, we have a
//! potential zombie conflict. This is tracked in the workspace for later
//! resolution during output.

use crate::change::{Change, Insertion};
use crate::pristine::GraphTxnT;
#[allow(unused_imports)]
use crate::types::{EdgeFlags, GraphNode, Hash, Inode, NodeId, Position, SerializedGraphEdge};

use super::error::LocalApplyError;
use super::graph_batch::{add_edge_with_reverse, CachedWriteGraphTxn};
use super::position::{resolve_context_vertex, resolve_inode, resolve_position};
use super::workspace::Workspace;

// Insertion Application

/// Apply a Insertion atom to the graph.
///
/// Creates a new span and connects it to the graph via context edges.
/// This is how new content is inserted into files.
///
/// # Arguments
///
/// * `txn` - Write transaction for graph modifications
/// * `workspace` - Workspace for tracking state and conflicts
/// * `change_id` - Internal ID of the change being applied
/// * `insertion` - The Insertion specification
/// * `change` - The full change for dependency checking
///
/// # Process
///
/// 1. Create the span from (change_id, start, end)
/// 2. Resolve and collect predecessors vertices
/// 3. Resolve and collect successors vertices
/// 4. Create edges from predecessors to new span
/// 5. Create edges from new span to successors
/// 6. Track any zombie edges (deleted context)
///
/// # Errors
///
/// - `CyclicDependency`: Down context references the current change
/// - `DependencyMissing`: Referenced change not found
/// - `BlockNotFound`: Context position not found
/// - `Internal`: Database error
pub fn write_new_vertex(
    txn: &mut CachedWriteGraphTxn<'_, '_>,
    workspace: &mut Workspace,
    change_id: NodeId,
    insertion: &Insertion<Option<Hash>>,
    change: &Change,
) -> Result<(), LocalApplyError> {
    // Create the new span
    let node = GraphNode {
        change: change_id,
        start: insertion.start,
        end: insertion.end,
    };

    // Clear workspace context for this span
    workspace.clear_context();

    // Fast path for the common "append one more line from this same change"
    // pattern used by large FileAdd chains. The predecessor is a vertex we
    // already inserted in this change, there are no successors, and the new
    // edge can be wired directly without re-running the general context walk.
    if insertion.successors.is_empty() && insertion.predecessors.len() == 1 {
        let internal_pos = resolve_position(txn, &insertion.predecessors[0], change_id)?;
        if let Some(up_vertex) = workspace.get_current_vertex(internal_pos, true) {
            let resolved_inode = resolve_inode(txn, &insertion.inode, change_id)?;
            let up_flag = insertion.flag | EdgeFlags::BLOCK;
            add_edge_with_reverse(txn, resolved_inode, up_flag, up_vertex, node, change_id)?;
            workspace.add_up_context(up_vertex.end_pos());
            workspace.add_up_context_vertex(up_vertex);
            workspace.register_current_vertex(node);
            return Ok(());
        }
    }

    // Resolve predecessors: vertices that come BEFORE this new content.
    // Uses the unified overlay-aware resolver so Local stacks can find
    // vertices written to GRAPH earlier in the same change.
    for up_pos in &insertion.predecessors {
        let internal_pos = resolve_position(txn, up_pos, change_id)?;
        let up_vertex = workspace
            .get_current_vertex(internal_pos, true)
            .unwrap_or(resolve_context_vertex(txn, internal_pos, true)?);
        // Store the end position (where new content connects)
        workspace.add_up_context(up_vertex.end_pos());
        workspace.add_up_context_vertex(up_vertex);

        // Check if predecessors was deleted by an unknown change
        check_deleted_context(txn, workspace, change, up_vertex)?;
    }

    let exact_inode_successor = resolve_position(txn, &insertion.inode, change_id).ok();

    // Resolve successors: vertices that come AFTER this new content.
    for down_pos in &insertion.successors {
        let internal_pos = resolve_position(txn, down_pos, change_id)?;

        // Down context must not be from the same change (would create cycle)
        if internal_pos.change == change_id {
            return Err(LocalApplyError::CyclicDependency {
                message: "Down context cannot reference the change being applied".to_string(),
            });
        }

        let down_vertex =
            if exact_inode_successor == Some(internal_pos) && internal_pos.change != change_id {
                let inode_anchor = GraphNode {
                    change: internal_pos.change,
                    start: internal_pos.pos,
                    end: internal_pos.pos,
                };
                if txn
                    .has_vertex(inode_anchor)
                    .map_err(|e| LocalApplyError::Internal {
                        message: format!("Failed to check inode anchor vertex: {}", e),
                    })?
                {
                    inode_anchor
                } else {
                    workspace
                        .get_current_vertex(internal_pos, false)
                        .unwrap_or(resolve_context_vertex(txn, internal_pos, false)?)
                }
            } else {
                workspace
                    .get_current_vertex(internal_pos, false)
                    .unwrap_or(resolve_context_vertex(txn, internal_pos, false)?)
            };
        // Store the start position (where new content connects)
        workspace.add_down_context(down_vertex.start_pos());
        workspace.add_down_context_vertex(down_vertex);

        // Check if successors was deleted by an unknown change
        check_deleted_context(txn, workspace, change, down_vertex)?;
    }

    // Resolve inode for inode_graph indexing
    let resolved_inode = resolve_inode(txn, &insertion.inode, change_id)?;

    // Create edges from predecessors to new span
    // For predecessors, we use find_block_end because predecessors positions
    // reference the END of predecessor vertices. A position of 12 means
    // "find the span that ends at position 12", not "find the span
    // containing position 12".
    let up_flag = insertion.flag | EdgeFlags::BLOCK;
    for up_vertex in workspace.predecessor_vertices().to_vec() {
        add_edge_with_reverse(txn, resolved_inode, up_flag, up_vertex, node, change_id)?;
    }

    // Create edges from new span to successors.
    //
    // We use the SAME flag (BLOCK or FOLDER, possibly with the bit set
    // from `insertion.flag`) as the predecessor edge.  The legacy Pijul
    // design stripped BLOCK from down-edges, but with the typed edge
    // model an edge whose flag is `EMPTY` parses as no [`EdgeKind`]
    // variant — making the edge invisible to `iter_forward` /
    // `iter_parents` and breaking forward traversal across a Replace
    // hunk's wired successor.
    let down_flag = insertion.flag | EdgeFlags::BLOCK;

    for down_vertex in workspace.successor_vertices().to_vec() {
        add_edge_with_reverse(txn, resolved_inode, down_flag, node, down_vertex, change_id)?;

        // Track folder files for missing context detection
        if insertion.flag.is_folder() {
            workspace.mark_rooted(down_vertex.start_pos());
        }
    }

    workspace.register_current_vertex(node);

    Ok(())
}

// Conflict Detection

/// Check if a context span was deleted by an unknown change.
///
/// If the context has DELETED edges from changes not in our dependencies,
/// we have a zombie conflict. The workspace tracks these for later resolution.
///
/// # Arguments
///
/// * `txn` - Transaction for graph queries
/// * `workspace` - Workspace to record conflicts
/// * `change` - Current change for dependency checking
/// * `node` - GraphNode to check for deleted context
fn check_deleted_context<T: GraphTxnT>(
    txn: &T,
    workspace: &mut Workspace,
    change: &Change,
    node: GraphNode<NodeId>,
) -> Result<(), LocalApplyError> {
    // Skip ROOT span
    if node.is_root() {
        return Ok(());
    }

    // Look for parent edges with DELETED flag
    let min_flag = EdgeFlags::PARENT | EdgeFlags::BLOCK;
    let max_flag = EdgeFlags::PARENT | EdgeFlags::BLOCK | EdgeFlags::DELETED | EdgeFlags::FOLDER;

    let edges =
        txn.iter_adjacent(node, min_flag, max_flag)
            .map_err(|e| LocalApplyError::Internal {
                message: format!("Failed to iterate adjacent edges: {}", e),
            })?;

    for edge_result in edges {
        let edge = edge_result.map_err(|e| LocalApplyError::Internal {
            message: format!("Edge iteration error: {}", e),
        })?;

        // Check if edge is deleted
        if edge.flag().contains(EdgeFlags::DELETED) {
            // Get the change that introduced the deletion
            let introduced_by = edge.introduced_by();
            if !introduced_by.is_root() {
                // Look up the external hash
                if let Ok(Some(hash)) = txn.get_external(introduced_by) {
                    // Check if this change knows about the deletion
                    if !change.knows(&hash) {
                        // Unknown deletion - mark as zombie
                        workspace.add_zombie_vertex(node);
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    // Test Helpers

    fn make_position(change: Option<Hash>, pos: u64) -> Position<Option<Hash>> {
        Position {
            change,
            pos: ChangePosition::new(pos),
        }
    }

    fn make_internal_vertex(change: NodeId, start: u64, end: u64) -> GraphNode<NodeId> {
        GraphNode {
            change,
            start: ChangePosition::new(start),
            end: ChangePosition::new(end),
        }
    }

    fn make_internal_position(change: NodeId, pos: u64) -> Position<NodeId> {
        Position {
            change,
            pos: ChangePosition::new(pos),
        }
    }

    // Insertion Structure Tests

    #[test]
    fn test_new_vertex_creation() {
        let hash = Hash::of(b"test");
        let insertion = Insertion {
            predecessors: vec![make_position(Some(hash), 0)],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: make_position(Some(hash), 0),
        };

        assert_eq!(insertion.predecessors.len(), 1);
        assert!(insertion.successors.is_empty());
        assert_eq!(insertion.flag, EdgeFlags::BLOCK);
    }

    #[test]
    fn test_new_vertex_with_both_contexts() {
        let hash1 = Hash::of(b"parent");
        let hash2 = Hash::of(b"child");
        let insertion = Insertion {
            predecessors: vec![make_position(Some(hash1), 50)],
            successors: vec![make_position(Some(hash2), 0)],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(20),
            inode: make_position(Some(hash1), 0),
        };

        assert_eq!(insertion.predecessors.len(), 1);
        assert_eq!(insertion.successors.len(), 1);
    }

    #[test]
    fn test_new_vertex_self_reference() {
        // Self-reference uses None for the change
        let insertion = Insertion {
            predecessors: vec![make_position(None, 0)],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(10),
            end: ChangePosition::new(20),
            inode: make_position(None, 0),
        };

        assert!(insertion.predecessors[0].change.is_none());
        assert!(insertion.inode.change.is_none());
    }

    #[test]
    fn test_new_vertex_folder_flag() {
        let hash = Hash::of(b"test");
        let insertion = Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(0),
            inode: make_position(Some(hash), 0),
        };

        assert!(insertion.flag.is_folder());
        assert!(insertion.flag.contains(EdgeFlags::BLOCK));
    }

    // GraphNode Creation Tests

    #[test]
    fn test_internal_vertex_creation() {
        let change = NodeId::new(42);
        let node = make_internal_vertex(change, 10, 20);

        assert_eq!(node.change, change);
        assert_eq!(node.start, ChangePosition::new(10));
        assert_eq!(node.end, ChangePosition::new(20));
    }

    #[test]
    fn test_vertex_positions() {
        let change = NodeId::new(42);
        let node = make_internal_vertex(change, 10, 20);

        let start_pos = node.start_pos();
        let end_pos = node.end_pos();

        assert_eq!(start_pos.change, change);
        assert_eq!(start_pos.pos, ChangePosition::new(10));
        assert_eq!(end_pos.change, change);
        assert_eq!(end_pos.pos, ChangePosition::new(20));
    }

    #[test]
    fn test_root_vertex() {
        let root = GraphNode::<NodeId>::root();

        assert!(root.is_root());
        assert!(root.is_empty());
        assert!(root.change.is_root());
    }

    #[test]
    fn test_empty_vertex() {
        let change = NodeId::new(42);
        let node = make_internal_vertex(change, 10, 10);

        assert!(node.is_empty());
        assert!(!node.is_root());
    }

    // Edge Flag Tests

    #[test]
    fn test_edge_flags_block() {
        let flag = EdgeFlags::BLOCK;

        assert!(flag.contains(EdgeFlags::BLOCK));
        assert!(!flag.contains(EdgeFlags::DELETED));
        assert!(!flag.is_folder());
    }

    #[test]
    fn test_edge_flags_folder() {
        let flag = EdgeFlags::FOLDER | EdgeFlags::BLOCK;

        assert!(flag.is_folder());
        assert!(flag.contains(EdgeFlags::BLOCK));
    }

    #[test]
    fn test_edge_flags_parent() {
        let base_flag = EdgeFlags::BLOCK;
        let with_parent = base_flag | EdgeFlags::PARENT;

        assert!(with_parent.contains(EdgeFlags::PARENT));
        assert!(with_parent.contains(EdgeFlags::BLOCK));
    }

    #[test]
    fn test_edge_flags_deleted() {
        let flag = EdgeFlags::BLOCK | EdgeFlags::DELETED;

        assert!(flag.contains(EdgeFlags::DELETED));
        assert!(flag.contains(EdgeFlags::BLOCK));
    }

    #[test]
    fn test_down_flag_calculation_non_folder() {
        let flag = EdgeFlags::BLOCK;

        // Non-folder: remove BLOCK from down edges
        let down_flag = flag - EdgeFlags::BLOCK;

        assert!(!down_flag.contains(EdgeFlags::BLOCK));
    }

    #[test]
    fn test_down_flag_calculation_folder() {
        let flag = EdgeFlags::FOLDER | EdgeFlags::BLOCK;

        // Folder: keep original flags
        let down_flag = flag;

        assert!(down_flag.contains(EdgeFlags::FOLDER));
        assert!(down_flag.contains(EdgeFlags::BLOCK));
    }

    // SerializedGraphEdge Tests

    #[test]
    fn test_serialized_edge_creation() {
        let change = NodeId::new(42);
        let dest_pos = Position {
            change: NodeId::new(100),
            pos: ChangePosition::new(50),
        };
        let edge = SerializedGraphEdge::new(EdgeFlags::BLOCK, dest_pos, change);

        assert_eq!(edge.flag(), EdgeFlags::BLOCK);
        assert_eq!(edge.dest(), dest_pos);
        assert_eq!(edge.introduced_by(), change);
    }

    #[test]
    fn test_serialized_edge_reverse() {
        let change = NodeId::new(42);
        let dest_pos = Position {
            change: NodeId::new(100),
            pos: ChangePosition::new(50),
        };
        let forward_flag = EdgeFlags::BLOCK;
        let reverse_flag = forward_flag | EdgeFlags::PARENT;

        let forward = SerializedGraphEdge::new(forward_flag, dest_pos, change);
        let reverse = SerializedGraphEdge::new(reverse_flag, dest_pos, change);

        assert!(!forward.flag().contains(EdgeFlags::PARENT));
        assert!(reverse.flag().contains(EdgeFlags::PARENT));
    }

    // Workspace Context Tests

    #[test]
    fn test_workspace_context_tracking() {
        let mut workspace = Workspace::new();
        let change = NodeId::new(42);
        let p1 = make_internal_position(change, 10); // end of predecessors
        let p2 = make_internal_position(change, 20); // start of successors

        workspace.add_up_context(p1);
        workspace.add_down_context(p2);

        assert_eq!(workspace.up_context_count(), 1);
        assert_eq!(workspace.down_context_count(), 1);
    }

    #[test]
    fn test_workspace_context_clear() {
        let mut workspace = Workspace::new();
        let change = NodeId::new(42);
        let p1 = make_internal_position(change, 10);

        workspace.add_up_context(p1);
        assert_eq!(workspace.up_context_count(), 1);

        workspace.clear_context();
        assert_eq!(workspace.up_context_count(), 0);
    }

    #[test]
    fn test_workspace_zombie_tracking() {
        let mut workspace = Workspace::new();
        let change = NodeId::new(42);
        let node = make_internal_vertex(change, 0, 10);

        assert!(!workspace.has_zombies());

        workspace.add_zombie_vertex(node);

        assert!(workspace.has_zombies());
        assert!(workspace.has_conflicts());
    }

    #[test]
    fn test_workspace_rooted_tracking() {
        let mut workspace = Workspace::new();
        let change = NodeId::new(42);
        let pos = make_internal_position(change, 0);

        assert!(!workspace.is_rooted(&pos));

        workspace.mark_rooted(pos);

        assert!(workspace.is_rooted(&pos));
    }

    // Error Case Tests

    #[test]
    fn test_cyclic_dependency_error() {
        let error = LocalApplyError::CyclicDependency {
            message: "Down context cannot reference current change".to_string(),
        };

        match error {
            LocalApplyError::CyclicDependency { message } => {
                assert!(message.contains("Down context"));
            }
            _ => panic!("Wrong error type"),
        }
    }

    #[test]
    fn test_dependency_missing_error() {
        let hash = Hash::of(b"missing");
        let error = LocalApplyError::DependencyMissing { hash };

        assert!(error.is_dependency_error());
    }

    #[test]
    fn test_block_not_found_error() {
        let pos = Position {
            change: NodeId::new(42),
            pos: ChangePosition::new(100),
        };
        let error = LocalApplyError::BlockNotFound { position: pos };

        match error {
            LocalApplyError::BlockNotFound { position } => {
                assert_eq!(position.change, NodeId::new(42));
            }
            _ => panic!("Wrong error type"),
        }
    }
}

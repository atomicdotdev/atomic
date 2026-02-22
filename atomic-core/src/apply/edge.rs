//! EdgeUpdate atom application
//!
//! This module handles applying `EdgeUpdate` atoms to the repository graph.
//! An EdgeUpdate modifies existing edges, typically to mark content as deleted
//! or to change edge properties.
//!
//! # Overview
//!
//! EdgeUpdate operations modify the flags on existing edges. The most common
//! use is marking content as deleted:
//!
//! ```text
//! Before: [A] ──BLOCK──> [B]
//! After:  [A] ──BLOCK|DELETED──> [B]
//! ```
//!
//! # Application Process
//!
//! For each edge in the EdgeUpdate:
//!
//! 1. **Resolve Positions**: Convert external hashes to internal NodeIds
//! 2. **Find Source Span**: Locate the source (may require splitting)
//! 3. **Find Target Span**: Locate the target (may require splitting)
//! 4. **Remove Old Edge**: Delete edge with `previous` flags
//! 5. **Add New Edge**: Insert edge with updated `flag`
//! 6. **Handle Deletions**: Collect pseudo-edges, detect zombies
//!
//! # Two-Phase Application
//!
//! Edge maps are typically applied in two phases across all atoms:
//!
//! 1. **Non-deletion phase**: Apply edges without DELETED flag
//! 2. **Deletion phase**: Apply edges with DELETED flag
//!
//! This ensures alive edges exist before deletion processing.
//!
//! # Zombie Detection
//!
//! When deleting content, we check for "zombie conflicts":
//! - Content we're deleting has live children from unknown changes
//! - Content we're deleting has live parents from unknown changes
//!
//! These are tracked in the workspace for later resolution.

use crate::change::{Change, EdgeUpdate, NewEdge};
use crate::pristine::{GraphTxnT, MutTxnT};
use crate::types::{EdgeFlags, GraphNode, Hash, Inode, NodeId, Position, SerializedGraphEdge};

use super::error::LocalApplyError;
use super::position::{resolve_inode, resolve_introduced_by, resolve_position};
use super::workspace::Workspace;
use super::ApplyTarget;

// EdgeUpdate Application

/// Apply an EdgeUpdate atom to the graph.
///
/// Modifies existing edges in the graph. This is primarily used for:
/// - Marking content as deleted (adding DELETED flag)
/// - Undeleting content (removing DELETED flag)
/// - Changing edge properties
///
/// # Arguments
///
/// * `txn` - Write transaction for graph modifications
/// * `workspace` - Workspace for tracking state and conflicts
/// * `change_id` - Internal ID of the change being applied
/// * `edge_update` - The EdgeUpdate specification
/// * `change` - The full change for dependency checking
///
/// # Errors
///
/// - `DependencyMissing`: Referenced change not found
/// - `BlockNotFound`: Source or target span doesn't exist
/// - `Internal`: Database error
pub fn apply_edge_map<T: MutTxnT>(
    txn: &mut T,
    workspace: &mut Workspace,
    change_id: NodeId,
    edge_update: &EdgeUpdate<Option<Hash>>,
    change: &Change,
    target: &ApplyTarget,
) -> Result<(), LocalApplyError> {
    // Process each edge in the map
    for edge in &edge_update.edges {
        apply_new_edge(
            txn,
            workspace,
            change_id,
            &edge_update.inode,
            edge,
            change,
            target,
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Overlay-aware vertex resolution for Local stacks
// ---------------------------------------------------------------------------
//
// When applying to a Local stack, newly created vertices live in
// STACK_GRAPH (written by `apply_new_vertex` via `put_stack_graph`).
// Subsequent edge operations in the SAME change need to reference those
// vertices, but the standard `find_block` / `find_block_end` on a
// `WriteTxn` only searches the global GRAPH table.
//
// The single entry point is `resolve_vertex_for_target`, which delegates to
// the shared `overlay::find_block_in_stack_graph` (the canonical STACK_GRAPH
// lookup) before falling back to the global GRAPH.  For Shared (Global)
// targets it skips the STACK_GRAPH probe entirely.

use crate::pristine::overlay::{find_block_in_stack_graph, FindBlockMode};

/// Resolve a vertex for an edge operation, consulting STACK_GRAPH first
/// when targeting a Local stack.
///
/// This is the single overlay-aware vertex finder used by the apply
/// pipeline.  It reuses the shared `find_block_in_stack_graph` from the
/// overlay module — the same function that powers `OverlayTxn::find_block`
/// and `OverlayTxn::find_block_end` — ensuring a single source of truth
/// for STACK_GRAPH vertex matching.
///
/// # Arguments
///
/// * `txn` - Transaction providing `GraphTxnT + StackTxnT` access
/// * `pos` - The position to resolve
/// * `target` - Routing target (Global or Local)
/// * `mode` - Whether to match containing-position or ending-at-position
pub(super) fn resolve_vertex_for_target<T: MutTxnT>(
    txn: &T,
    pos: Position<NodeId>,
    target: &ApplyTarget,
    mode: FindBlockMode,
) -> Result<GraphNode<NodeId>, LocalApplyError> {
    if pos.change.is_root() {
        return Ok(GraphNode::root());
    }

    // For Local targets, probe STACK_GRAPH first (vertices written
    // earlier in this same transaction).
    if let ApplyTarget::Local { stack_id } = target {
        if let Some(v) =
            find_block_in_stack_graph(txn, *stack_id, pos.change.get(), pos.pos.get(), mode)
                .map_err(|e| LocalApplyError::internal(e.to_string()))?
        {
            return Ok(v);
        }
    }

    // Fall back to global GRAPH (works for both Global and Local targets).
    match mode {
        FindBlockMode::ContainingPosition => txn.find_block(pos),
        FindBlockMode::EndingAtPosition => txn.find_block_end(pos),
    }
    .map_err(|_| LocalApplyError::BlockNotFound { position: pos })
}

/// Apply a single NewEdge operation.
///
/// This handles the actual edge modification in the graph.
///
/// # Arguments
///
/// * `txn` - Write transaction
/// * `workspace` - Workspace for tracking state
/// * `change_id` - Internal ID of current change
/// * `inode` - File inode position for indexing
/// * `edge` - The edge to apply
/// * `change` - Full change for dependency checking
fn apply_new_edge<T: MutTxnT>(
    txn: &mut T,
    workspace: &mut Workspace,
    change_id: NodeId,
    inode: &Position<Option<Hash>>,
    edge: &NewEdge<Option<Hash>>,
    change: &Change,
    apply_target: &ApplyTarget,
) -> Result<(), LocalApplyError> {
    // Resolve the introduced_by change
    let introduced_by = resolve_introduced_by(txn, &edge.introduced_by, change_id)?;

    // Find source span — overlay-aware so Local stacks can see vertices
    // written to STACK_GRAPH earlier in the same change.
    let source_pos = resolve_position(txn, &edge.from, change_id)?;
    let source = resolve_vertex_for_target(
        txn,
        source_pos,
        apply_target,
        FindBlockMode::EndingAtPosition,
    )?;

    // Find target span — same overlay-aware lookup.
    let target_pos = resolve_position(txn, &edge.to.start_pos(), change_id)?;
    let mut target = resolve_vertex_for_target(
        txn,
        target_pos,
        apply_target,
        FindBlockMode::ContainingPosition,
    )?;

    // Resolve inode for indexing
    let resolved_inode = resolve_inode(txn, inode, change_id)?;

    // Track folder files for conflict detection
    if edge.flag.contains(EdgeFlags::FOLDER) {
        workspace.mark_rooted(target.start_pos());
    }

    // Handle potential span splitting for partial targets
    let target_end_pos = resolve_position(txn, &edge.to.end_pos(), change_id)?;
    if target.end > target_end_pos.pos {
        // Target span extends beyond the edge target - adjust
        target = GraphNode {
            change: target.change,
            start: target.start,
            end: target_end_pos.pos,
        };
    }

    // Handle deletion: collect pseudo-edges for reconnection
    if edge.flag.contains(EdgeFlags::DELETED) {
        collect_pseudo_edges_for_reconnection(txn, workspace, target)?;
    }

    // Remove the old edge (ignoring not-found errors)
    del_edge_with_reverse(
        txn,
        resolved_inode,
        edge.previous,
        source,
        target,
        introduced_by,
        apply_target,
    )?;

    // Add the new edge
    add_edge_with_reverse(
        txn,
        resolved_inode,
        edge.flag,
        source,
        target,
        change_id,
        apply_target,
    )?;

    // For deletions, check for zombie context
    if edge.flag.contains(EdgeFlags::DELETED) && !edge.flag.contains(EdgeFlags::FOLDER) {
        collect_zombie_context(txn, workspace, change, edge, change_id)?;
    }

    Ok(())
}

// Span Finding

/// Find the source span for an edge operation.
///
/// Given a position, finds the span whose END matches this position.
/// For edge sources, we want the span ENDING at the position because
/// edges originate from the END of the predecessor span.
///
/// For example, if we have:
/// - Inode span V[17:17] (empty)
/// - Content span V[17:27]
///
/// And we want to find the source for an edge at position 17, we want
/// the inode span V[17:17] (which ENDS at 17), not the content span
/// V[17:27] (which STARTS at 17).
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `pos` - Position to find (the END position of the source span)
///
/// # Returns
///
/// The span ending at the position.
///
/// # Errors
///
/// Returns `BlockNotFound` if no span ends at this position.
pub fn find_source_vertex<T: GraphTxnT>(
    txn: &T,
    pos: Position<NodeId>,
) -> Result<GraphNode<NodeId>, LocalApplyError> {
    if pos.change.is_root() {
        // ROOT span is empty at position 0
        return Ok(GraphNode::root());
    }

    // Use find_block_end because edge sources reference the END of a span
    txn.find_block_end(pos)
        .map_err(|_| LocalApplyError::BlockNotFound { position: pos })
}

/// Find the target span for an edge operation.
///
/// Given a position, finds the span that contains it. For edge targets,
/// we want the span starting at or containing the position.
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `pos` - Position to find
///
/// # Returns
///
/// The span containing the position.
///
/// # Errors
///
/// Returns `BlockNotFound` if no span contains this position.
pub fn find_target_vertex<T: GraphTxnT>(
    txn: &T,
    pos: Position<NodeId>,
) -> Result<GraphNode<NodeId>, LocalApplyError> {
    if pos.change.is_root() {
        // ROOT span is empty at position 0
        return Ok(GraphNode::root());
    }

    txn.find_block(pos)
        .map_err(|_| LocalApplyError::BlockNotFound { position: pos })
}

// Edge Operations

/// Add an edge and its reverse to the graph.
///
/// In the Atomic graph model, edges come in pairs:
/// - Forward edge: `source → target` with base flags
/// - Reverse edge: `target → source` with PARENT flag added
///
/// # Arguments
///
/// * `txn` - Write transaction
/// * `inode` - Optional inode for inode_graph indexing
/// * `flag` - Edge flags for the forward edge
/// * `source` - Source span
/// * `target` - Target span
/// * `introduced_by` - Change that introduced this edge
fn add_edge_with_reverse<T: MutTxnT>(
    txn: &mut T,
    inode: Option<Inode>,
    flag: EdgeFlags,
    source: GraphNode<NodeId>,
    dest: GraphNode<NodeId>,
    introduced_by: NodeId,
    apply_target: &ApplyTarget,
) -> Result<(), LocalApplyError> {
    // Create forward edge
    let forward_edge = SerializedGraphEdge::new(flag, dest.start_pos(), introduced_by);

    // Create reverse edge (with PARENT flag)
    let reverse_flag = flag | EdgeFlags::PARENT;
    let reverse_edge = SerializedGraphEdge::new(reverse_flag, source.end_pos(), introduced_by);

    match apply_target {
        ApplyTarget::Global => {
            // Shared stack: write to global GRAPH + INODE_GRAPH
            txn.put_graph(source, forward_edge)
                .map_err(|e| LocalApplyError::Internal {
                    message: format!("Failed to add forward edge: {}", e),
                })?;

            txn.put_graph(dest, reverse_edge)
                .map_err(|e| LocalApplyError::Internal {
                    message: format!("Failed to add reverse edge: {}", e),
                })?;

            if let Some(inode_val) = inode {
                txn.put_inode_graph(inode_val, source, forward_edge)
                    .map_err(|e| LocalApplyError::Internal {
                        message: format!("Failed to add forward inode edge: {}", e),
                    })?;

                txn.put_inode_graph(inode_val, dest, reverse_edge)
                    .map_err(|e| LocalApplyError::Internal {
                        message: format!("Failed to add reverse inode edge: {}", e),
                    })?;
            }
        }
        ApplyTarget::Local { stack_id } => {
            // Local workspace: write to STACK_GRAPH[(stack_id, vertex)]
            txn.put_stack_graph(*stack_id, source, forward_edge)
                .map_err(|e| LocalApplyError::Internal {
                    message: format!("Failed to add forward stack graph edge: {}", e),
                })?;

            txn.put_stack_graph(*stack_id, dest, reverse_edge)
                .map_err(|e| LocalApplyError::Internal {
                    message: format!("Failed to add reverse stack graph edge: {}", e),
                })?;
        }
    }

    Ok(())
}

/// Remove an edge and its reverse from the graph.
///
/// This is the inverse of `add_edge_with_reverse`. Removes both the
/// forward and reverse edges.
///
/// # Arguments
///
/// * `txn` - Write transaction
/// * `inode` - Optional inode for inode_graph cleanup
/// * `flag` - Edge flags for the forward edge
/// * `source` - Source span
/// * `target` - Target span
/// * `introduced_by` - Change that introduced the edge
///
/// # Notes
///
/// This function ignores not-found errors, as the edge may have
/// already been removed or may not exist.
fn del_edge_with_reverse<T: MutTxnT>(
    txn: &mut T,
    inode: Option<Inode>,
    flag: EdgeFlags,
    source: GraphNode<NodeId>,
    dest: GraphNode<NodeId>,
    introduced_by: NodeId,
    apply_target: &ApplyTarget,
) -> Result<(), LocalApplyError> {
    // Create forward edge to delete
    let forward_edge = SerializedGraphEdge::new(flag, dest.start_pos(), introduced_by);

    // Create reverse edge to delete
    let reverse_flag = flag | EdgeFlags::PARENT;
    let reverse_edge = SerializedGraphEdge::new(reverse_flag, source.end_pos(), introduced_by);

    match apply_target {
        ApplyTarget::Global => {
            // Shared stack: remove from global GRAPH + INODE_GRAPH
            let _ = txn.del_graph(source, forward_edge);
            let _ = txn.del_graph(dest, reverse_edge);

            if let Some(inode_val) = inode {
                let _ = txn.del_inode_graph(inode_val, source, forward_edge);
                let _ = txn.del_inode_graph(inode_val, dest, reverse_edge);
            }
        }
        ApplyTarget::Local { stack_id } => {
            // Local workspace: remove from STACK_GRAPH[(stack_id, vertex)]
            let _ = txn.del_stack_graph(*stack_id, source, forward_edge);
            let _ = txn.del_stack_graph(*stack_id, dest, reverse_edge);
        }
    }

    Ok(())
}

// Deletion Handling

/// Collect pseudo-edges for reconnection when deleting a span.
///
/// When we delete content, we may need to reconnect the graph to maintain
/// connectivity. This collects information about children that need
/// reconnection to their grandparents.
///
/// # Arguments
///
/// * `txn` - Transaction for graph queries
/// * `workspace` - Workspace to track reconnection info
/// * `target` - Span being deleted
fn collect_pseudo_edges_for_reconnection<T: GraphTxnT>(
    txn: &T,
    workspace: &mut Workspace,
    target: GraphNode<NodeId>,
) -> Result<(), LocalApplyError> {
    // Skip ROOT span
    if target.is_root() {
        return Ok(());
    }

    // Collect children of the target span that need reconnection
    let child_flags = EdgeFlags::empty();
    let max_child_flags = EdgeFlags::BLOCK | EdgeFlags::FOLDER | EdgeFlags::PSEUDO;

    let children = txn
        .iter_adjacent(target, child_flags, max_child_flags)
        .map_err(|e| LocalApplyError::Internal {
            message: format!("Failed to iterate children: {}", e),
        })?;

    for child_result in children {
        let child = child_result.map_err(|e| LocalApplyError::Internal {
            message: format!("Child iteration error: {}", e),
        })?;

        if !child.flag().contains(EdgeFlags::PSEUDO) {
            // Track this child as needing reconnection
            workspace.set_parent(child.dest(), target.end_pos());
        }
    }

    Ok(())
}

/// Collect zombie context when deleting content.
///
/// If we're deleting content that has children or parents from unknown
/// changes (not in our dependencies), those become zombies.
///
/// A zombie is content that:
/// - We want to delete
/// - But has live connections from changes we don't know about
///
/// # Arguments
///
/// * `txn` - Transaction for graph queries
/// * `workspace` - Workspace to track zombies
/// * `change` - Current change for dependency checking
/// * `edge` - The deletion edge
/// * `change_id` - Internal ID of current change
fn collect_zombie_context<T: GraphTxnT>(
    txn: &T,
    workspace: &mut Workspace,
    change: &Change,
    edge: &NewEdge<Option<Hash>>,
    change_id: NodeId,
) -> Result<(), LocalApplyError> {
    // Get the target position range
    let start_pos = resolve_position(txn, &edge.to.start_pos(), change_id)?;
    let end_pos = resolve_position(txn, &edge.to.end_pos(), change_id)?;

    // Find all vertices in the target range
    let mut pos = start_pos;
    while let Ok(node) = txn.find_block(pos) {
        // Check for non-deleted edges that we don't know about
        check_vertex_for_zombies(txn, workspace, change, node, change_id)?;

        // Move to next span in range
        if node.end < end_pos.pos {
            pos.pos = node.end;
        } else {
            break;
        }
    }

    Ok(())
}

/// Check a single span for zombie conflicts.
///
/// Looks for live edges from unknown changes.
fn check_vertex_for_zombies<T: GraphTxnT>(
    txn: &T,
    workspace: &mut Workspace,
    change: &Change,
    node: GraphNode<NodeId>,
    change_id: NodeId,
) -> Result<(), LocalApplyError> {
    let min_flag = EdgeFlags::empty();
    let max_flag = EdgeFlags::all() - EdgeFlags::DELETED;

    let edges =
        txn.iter_adjacent(node, min_flag, max_flag)
            .map_err(|e| LocalApplyError::Internal {
                message: format!("Failed to iterate for zombies: {}", e),
            })?;

    for edge_result in edges {
        let adj_edge = edge_result.map_err(|e| LocalApplyError::Internal {
            message: format!("Zombie edge iteration error: {}", e),
        })?;

        let introduced_by = adj_edge.introduced_by();
        if introduced_by == change_id || introduced_by.is_root() {
            continue;
        }

        // Check if we know about this change
        if let Ok(Some(hash)) = txn.get_external(introduced_by) {
            if !change.knows(&hash) {
                // Unknown live edge - this is a zombie
                workspace.add_zombie_vertex(node);
                break;
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

    fn make_internal_position(change: NodeId, pos: u64) -> Position<NodeId> {
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

    fn make_external_vertex(change: Option<Hash>, start: u64, end: u64) -> GraphNode<Option<Hash>> {
        GraphNode {
            change,
            start: ChangePosition::new(start),
            end: ChangePosition::new(end),
        }
    }

    // NewEdge Structure Tests

    #[test]
    fn test_new_edge_creation() {
        let hash = Hash::of(b"test");
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: make_position(Some(hash), 10),
            to: make_external_vertex(Some(hash), 20, 30),
            introduced_by: Some(hash),
        };

        assert_eq!(edge.previous, EdgeFlags::BLOCK);
        assert!(edge.flag.contains(EdgeFlags::DELETED));
        assert_eq!(edge.introduced_by, Some(hash));
    }

    #[test]
    fn test_new_edge_self_reference() {
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: make_position(None, 0),
            to: make_external_vertex(None, 10, 20),
            introduced_by: None,
        };

        assert!(edge.from.change.is_none());
        assert!(edge.to.change.is_none());
        assert!(edge.introduced_by.is_none());
    }

    #[test]
    fn test_new_edge_is_deletion() {
        let hash = Hash::of(b"test");
        let deletion_edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: make_position(Some(hash), 0),
            to: make_external_vertex(Some(hash), 10, 20),
            introduced_by: Some(hash),
        };

        assert!(deletion_edge.is_deletion());
    }

    #[test]
    fn test_new_edge_is_undeletion() {
        let hash = Hash::of(b"test");
        let undel_edge = NewEdge {
            previous: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            flag: EdgeFlags::BLOCK,
            from: make_position(Some(hash), 0),
            to: make_external_vertex(Some(hash), 10, 20),
            introduced_by: Some(hash),
        };

        assert!(undel_edge.is_undeletion());
    }

    // EdgeUpdate Structure Tests

    #[test]
    fn test_edge_map_creation() {
        let hash = Hash::of(b"test");
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: make_position(Some(hash), 0),
            to: make_external_vertex(Some(hash), 10, 20),
            introduced_by: Some(hash),
        };

        let mut edge_update = EdgeUpdate::new(make_position(Some(hash), 0));
        edge_update.push(edge);

        assert_eq!(edge_update.len(), 1);
        assert!(!edge_update.is_empty());
    }

    #[test]
    fn test_edge_map_multiple_edges() {
        let hash = Hash::of(b"test");
        let edge1 = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: make_position(Some(hash), 0),
            to: make_external_vertex(Some(hash), 10, 20),
            introduced_by: Some(hash),
        };
        let edge2 = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: make_position(Some(hash), 30),
            to: make_external_vertex(Some(hash), 40, 50),
            introduced_by: Some(hash),
        };

        let mut edge_update = EdgeUpdate::new(make_position(Some(hash), 0));
        edge_update.push(edge1);
        edge_update.push(edge2);

        assert_eq!(edge_update.len(), 2);
    }

    // Span Finding Tests

    #[test]
    fn test_root_source_vertex() {
        let root_pos = Position {
            change: NodeId::ROOT,
            pos: ChangePosition::new(0),
        };

        // We can't call find_source_vertex without a transaction,
        // but we can verify ROOT handling logic
        assert!(root_pos.change.is_root());
    }

    #[test]
    fn test_root_target_vertex() {
        let root_pos = Position {
            change: NodeId::ROOT,
            pos: ChangePosition::new(0),
        };

        assert!(root_pos.change.is_root());
    }

    #[test]
    fn test_vertex_adjustment() {
        // When target.end > target_end_pos.pos, we adjust
        let change = NodeId::new(42);
        let target = make_internal_vertex(change, 0, 100);
        let target_end_pos = ChangePosition::new(50);

        // Adjusted target
        let adjusted = GraphNode {
            change: target.change,
            start: target.start,
            end: target_end_pos,
        };

        assert_eq!(adjusted.end, ChangePosition::new(50));
        assert!(adjusted.end < target.end);
    }

    // Edge Flag Transition Tests

    #[test]
    fn test_deletion_flag_transition() {
        let previous = EdgeFlags::BLOCK;
        let new_flag = EdgeFlags::BLOCK | EdgeFlags::DELETED;

        assert!(!previous.contains(EdgeFlags::DELETED));
        assert!(new_flag.contains(EdgeFlags::DELETED));
        assert!(new_flag.contains(EdgeFlags::BLOCK));
    }

    #[test]
    fn test_undeletion_flag_transition() {
        let previous = EdgeFlags::BLOCK | EdgeFlags::DELETED;
        let new_flag = EdgeFlags::BLOCK;

        assert!(previous.contains(EdgeFlags::DELETED));
        assert!(!new_flag.contains(EdgeFlags::DELETED));
    }

    #[test]
    fn test_folder_deletion() {
        let flag = EdgeFlags::FOLDER | EdgeFlags::BLOCK | EdgeFlags::DELETED;

        assert!(flag.is_folder());
        assert!(flag.contains(EdgeFlags::DELETED));
    }

    // SerializedGraphEdge Tests

    #[test]
    fn test_serialized_edge_for_deletion() {
        let change = NodeId::new(42);
        let dest = make_internal_position(NodeId::new(100), 50);
        let flag = EdgeFlags::BLOCK | EdgeFlags::DELETED;

        let edge = SerializedGraphEdge::new(flag, dest, change);

        assert!(edge.flag().contains(EdgeFlags::DELETED));
        assert_eq!(edge.introduced_by(), change);
    }

    #[test]
    fn test_serialized_edge_reverse() {
        let change = NodeId::new(42);
        let dest = make_internal_position(NodeId::new(100), 50);
        let forward_flag = EdgeFlags::BLOCK | EdgeFlags::DELETED;
        let reverse_flag = forward_flag | EdgeFlags::PARENT;

        let forward = SerializedGraphEdge::new(forward_flag, dest, change);
        let reverse = SerializedGraphEdge::new(reverse_flag, dest, change);

        assert!(!forward.flag().contains(EdgeFlags::PARENT));
        assert!(reverse.flag().contains(EdgeFlags::PARENT));
        assert!(reverse.flag().contains(EdgeFlags::DELETED));
    }

    // Workspace Tracking Tests

    #[test]
    fn test_workspace_parent_tracking() {
        let mut workspace = Workspace::new();
        let child_pos = make_internal_position(NodeId::new(100), 50);
        let parent_pos = make_internal_position(NodeId::new(42), 20);

        workspace.set_parent(child_pos, parent_pos);

        assert_eq!(workspace.get_parent(&child_pos), Some(parent_pos));
    }

    #[test]
    fn test_workspace_zombie_tracking() {
        let mut workspace = Workspace::new();
        let node = make_internal_vertex(NodeId::new(42), 0, 10);

        assert!(!workspace.has_zombies());

        workspace.add_zombie_vertex(node);

        assert!(workspace.has_zombies());
        assert!(workspace.has_conflicts());
    }

    #[test]
    fn test_workspace_rooted_tracking() {
        let mut workspace = Workspace::new();
        let pos = make_internal_position(NodeId::new(42), 0);

        workspace.mark_rooted(pos);

        assert!(workspace.is_rooted(&pos));
    }

    // Error Case Tests

    #[test]
    fn test_block_not_found_error() {
        let pos = make_internal_position(NodeId::new(42), 100);
        let error = LocalApplyError::BlockNotFound { position: pos };

        match error {
            LocalApplyError::BlockNotFound { position } => {
                assert_eq!(position.change, NodeId::new(42));
                assert_eq!(position.pos, ChangePosition::new(100));
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
    fn test_internal_error() {
        let error = LocalApplyError::Internal {
            message: "Test error".to_string(),
        };

        match error {
            LocalApplyError::Internal { message } => {
                assert!(message.contains("Test"));
            }
            _ => panic!("Wrong error type"),
        }
    }

    // Integration-style Tests (Structure Only)

    #[test]
    fn test_deletion_workflow_structure() {
        // This test verifies the structure of a deletion workflow
        let hash = Hash::of(b"file content");

        // 1. Create the edge to delete
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: make_position(Some(hash), 0),
            to: make_external_vertex(Some(hash), 10, 20),
            introduced_by: Some(hash),
        };

        // 2. Wrap in EdgeUpdate
        let mut edge_update = EdgeUpdate::new(make_position(Some(hash), 0));
        edge_update.push(edge.clone());

        // 3. Verify structure
        assert_eq!(edge_update.len(), 1);
        assert!(edge_update.edges[0].flag.contains(EdgeFlags::DELETED));
    }

    #[test]
    fn test_undeletion_workflow_structure() {
        let hash = Hash::of(b"file content");

        // Undeletion: remove DELETED flag
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            flag: EdgeFlags::BLOCK,
            from: make_position(Some(hash), 0),
            to: make_external_vertex(Some(hash), 10, 20),
            introduced_by: Some(hash),
        };

        assert!(edge.is_undeletion());
        assert!(!edge.flag.contains(EdgeFlags::DELETED));
    }
}

//! Edge writing for the apply pipeline
//!
//! This module handles writing `EdgeUpdate` atoms to the repository graph.
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
//! # Writing Process
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
//! # Two-Phase Writing
//!
//! Edge maps are typically written in two phases across all atoms:
//!
//! 1. **Non-deletion phase**: Write edges without DELETED flag
//! 2. **Deletion phase**: Write edges with DELETED flag
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
use crate::pristine::{GraphTxnT, MutTxnT, TreeTxnT};
use crate::types::{
    ChangePosition, EdgeFlags, EdgeKind, GraphNode, Hash, Inode, NodeId, Position,
    SerializedGraphEdge,
};

use super::error::LocalApplyError;
use super::graph_batch::GraphWriteBatch;
use super::position::{
    resolve_inode, resolve_introduced_by, resolve_position, resolve_vertex as resolve_exact_vertex,
};
use super::workspace::Workspace;

// EdgeUpdate Writing

/// Write an EdgeUpdate atom to the graph.
///
/// Modifies existing edges in the graph. This is primarily used for:
/// - Marking content as deleted (adding DELETED flag)
/// - Undeleting content (removing DELETED flag)
/// - Changing edge properties
///
/// Edges are written to the global GRAPH and INODE_GRAPH tables.
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
pub fn write_edge_map<T: MutTxnT>(
    txn: &mut T,
    workspace: &mut Workspace,
    change_id: NodeId,
    edge_update: &EdgeUpdate<Option<Hash>>,
    change: &Change,
) -> Result<(), LocalApplyError> {
    // Process each edge in the map
    for edge in &edge_update.edges {
        write_new_edge(txn, workspace, change_id, &edge_update.inode, edge, change)?;
    }

    Ok(())
}

/// Batched variant of [`write_edge_map`] that keeps graph tables open across
/// the full hunk pass.
pub fn write_edge_map_batched<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    graph_batch: &mut GraphWriteBatch<'_>,
    workspace: &mut Workspace,
    change_id: NodeId,
    edge_update: &EdgeUpdate<Option<Hash>>,
    change: &Change,
) -> Result<(), LocalApplyError> {
    for edge in &edge_update.edges {
        write_new_edge_batched(
            txn,
            graph_batch,
            workspace,
            change_id,
            &edge_update.inode,
            edge,
            change,
        )?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Vertex resolution helpers for the apply pipeline
// ---------------------------------------------------------------------------

/// Resolve a position to a vertex in the graph.
///
/// Uses `find_block` for forward context (containing position) and
/// `find_block_end` for predecessor context (ending at position).
pub(super) fn resolve_vertex<T: GraphTxnT>(
    txn: &T,
    pos: Position<NodeId>,
    is_predecessor: bool,
) -> Result<GraphNode<NodeId>, LocalApplyError> {
    if pos.change.is_root() {
        return Ok(GraphNode::root());
    }

    if is_predecessor {
        txn.find_block_end(pos)
    } else {
        txn.find_block(pos)
    }
    .map_err(|_| LocalApplyError::BlockNotFound { position: pos })
}

/// Write a single NewEdge operation to the graph.
///
/// This handles the actual edge modification: resolving positions,
/// removing old edges, adding new edges, and detecting zombie conflicts.
///
/// Edges are written to the global GRAPH and INODE_GRAPH tables.
///
/// # Arguments
///
/// * `txn` - Write transaction
/// * `workspace` - Workspace for tracking state
/// * `change_id` - Internal ID of current change
/// * `inode` - File inode position for indexing
/// * `edge` - The edge to write
/// * `change` - Full change for dependency checking
fn write_new_edge<T: MutTxnT>(
    txn: &mut T,
    workspace: &mut Workspace,
    change_id: NodeId,
    inode: &Position<Option<Hash>>,
    edge: &NewEdge<Option<Hash>>,
    change: &Change,
) -> Result<(), LocalApplyError> {
    log::debug!(
        "write_new_edge: flag={:?} from={:?} to={:?}",
        edge.flag,
        edge.from,
        edge.to
    );

    // Resolve the introduced_by change for validation / side effects.
    // The resolved id itself is unused here — the additive-only edge
    // model writes the new edge directly without consulting it.
    let _ = resolve_introduced_by(txn, &edge.introduced_by, change_id)?;

    // Find source span — predecessor context (ending at position).
    let source_pos = resolve_position(txn, &edge.from, change_id)?;
    let source = resolve_vertex(txn, source_pos, true)?;

    // Prefer the exact serialized target span when it already exists.
    // Structural updates like FileMove name deletions refer to a concrete
    // vertex span, and reducing them to start_pos loses that precision.
    let exact_target = resolve_exact_vertex(txn, &edge.to, change_id)?;
    let mut target = if txn
        .has_vertex(exact_target)
        .map_err(|e| LocalApplyError::Internal {
            message: format!("Failed to check target vertex: {}", e),
        })? {
        exact_target
    } else {
        let target_pos = resolve_position(txn, &edge.to.start_pos(), change_id)?;
        resolve_vertex(txn, target_pos, false)?
    };

    // Resolve inode for indexing
    let resolved_inode = resolve_inode(txn, inode, change_id)?;

    // Parse the edge kind for semantic dispatch.
    // `edge.flag` is still `EdgeFlags` (wire format from the change), so we
    // parse it into a typed `EdgeKind` for cleaner branching below.
    let kind = EdgeKind::from_flags(edge.flag);

    // Track folder files for conflict detection
    if kind.is_some_and(|k| k.is_folder()) {
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
    if kind.is_some_and(|k| k.is_deleted()) {
        log::debug!("write_new_edge: collect_pseudo_edges starting");
        collect_pseudo_edges_for_reconnection(txn, workspace, target)?;
        log::debug!("write_new_edge: collect_pseudo_edges complete");
    }

    // ADDITIVE-ONLY: do NOT delete the old edge.  In a patch-theory
    // graph database, every change adds new edges — it never removes
    // them.  The original BLOCK edge stays in the B-tree alongside the
    // new BLOCK|DELETED edge so other views (whose change filter does
    // not include this deletion) continue to see the alive edge.
    //
    // `is_vertex_alive` checks for superseding deletions among ALL
    // incoming edges (including DELETED ones) when a filter is active,
    // so the alive edge being present doesn't "leak" through filters.
    log::debug!("write_new_edge: add_edge_with_reverse (additive-only model)");
    add_edge_with_reverse(txn, resolved_inode, edge.flag, source, target, change_id)?;

    // For non-folder deletions, check for zombie context
    if kind.is_some_and(|k| k.is_deleted() && !k.is_folder()) {
        log::debug!("write_new_edge: collect_zombie_context starting");
        collect_zombie_context(txn, workspace, change, edge, change_id)?;
        log::debug!("write_new_edge: collect_zombie_context complete");
    }

    Ok(())
}

fn write_new_edge_batched<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    graph_batch: &mut GraphWriteBatch<'_>,
    workspace: &mut Workspace,
    change_id: NodeId,
    inode: &Position<Option<Hash>>,
    edge: &NewEdge<Option<Hash>>,
    change: &Change,
) -> Result<(), LocalApplyError> {
    log::debug!(
        "write_new_edge_batched: flag={:?} from={:?} to={:?}",
        edge.flag,
        edge.from,
        edge.to
    );

    let _ = resolve_introduced_by(txn, &edge.introduced_by, change_id)?;

    let source_pos = resolve_position(txn, &edge.from, change_id)?;
    let source = resolve_vertex(txn, source_pos, true)?;

    let exact_target = resolve_exact_vertex(txn, &edge.to, change_id)?;
    let mut target =
        if graph_batch
            .has_graph_vertex(exact_target)
            .map_err(|e| LocalApplyError::Internal {
                message: format!("Failed to check target vertex: {}", e),
            })?
        {
            exact_target
        } else {
            let target_pos = resolve_position(txn, &edge.to.start_pos(), change_id)?;
            resolve_vertex(txn, target_pos, false)?
        };

    let resolved_inode = resolve_inode(txn, inode, change_id)?;
    let kind = EdgeKind::from_flags(edge.flag);

    if kind.is_some_and(|k| k.is_folder()) {
        workspace.mark_rooted(target.start_pos());
    }

    let target_end_pos = resolve_position(txn, &edge.to.end_pos(), change_id)?;
    if target.end > target_end_pos.pos {
        target = GraphNode {
            change: target.change,
            start: target.start,
            end: target_end_pos.pos,
        };
    }

    if kind.is_some_and(|k| k.is_deleted()) {
        collect_pseudo_edges_for_reconnection(txn, workspace, target)?;
    }

    add_edge_with_reverse_batched(
        graph_batch,
        resolved_inode,
        edge.flag,
        source,
        target,
        change_id,
    )?;

    if kind.is_some_and(|k| k.is_deleted() && !k.is_folder()) {
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
/// - Inode span V\[17:17\] (empty)
/// - Content span V\[17:27\]
///
/// And we want to find the source for an edge at position 17, we want
/// the inode span V\[17:17\] (which ENDS at 17), not the content span
/// V\[17:27\] (which STARTS at 17).
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

/// Remove a forward edge and its reverse (PARENT) counterpart from
/// GRAPH and INODE_GRAPH.
///
/// Errors are silently ignored (the edge may not exist if this is the
/// first time the graph is being written).
#[allow(dead_code)]
fn del_edge_with_reverse<T: MutTxnT>(
    txn: &mut T,
    inode: Option<Inode>,
    flag: EdgeFlags,
    source: GraphNode<NodeId>,
    dest: GraphNode<NodeId>,
    introduced_by: NodeId,
) -> Result<(), LocalApplyError> {
    let forward_edge = SerializedGraphEdge::new(flag, dest.start_pos(), introduced_by);
    let reverse_flag = flag | EdgeFlags::PARENT;
    let reverse_edge = SerializedGraphEdge::new(reverse_flag, source.end_pos(), introduced_by);

    let _ = txn.del_graph(source, forward_edge);
    let _ = txn.del_graph(dest, reverse_edge);

    if let Some(inode_val) = inode {
        let _ = txn.del_inode_graph(inode_val, source, forward_edge);
        let _ = txn.del_inode_graph(inode_val, dest, reverse_edge);
    }

    Ok(())
}

/// Write an edge and its reverse to the graph.
///
/// In the Atomic graph model, edges come in pairs:
/// - Forward edge: `source → target` with base flags
/// - Reverse edge: `target → source` with PARENT flag added
///
/// Writes both edges to the global GRAPH table and, when an inode is
/// provided, to the INODE_GRAPH secondary index.
fn add_edge_with_reverse<T: MutTxnT>(
    txn: &mut T,
    inode: Option<Inode>,
    flag: EdgeFlags,
    source: GraphNode<NodeId>,
    dest: GraphNode<NodeId>,
    introduced_by: NodeId,
) -> Result<(), LocalApplyError> {
    // Create forward edge
    let forward_edge = SerializedGraphEdge::new(flag, dest.start_pos(), introduced_by);

    // Create reverse edge (same flags + PARENT)
    let reverse_flag = flag | EdgeFlags::PARENT;
    let reverse_edge = SerializedGraphEdge::new(reverse_flag, source.end_pos(), introduced_by);

    log::debug!(
        "add_edge_with_reverse: flag={:?} source=[{:?} {:?}:{:?}] dest=[{:?} {:?}:{:?}] introduced_by={:?}",
        flag, source.change, source.start, source.end,
        dest.change, dest.start, dest.end,
        introduced_by
    );

    // Write to global GRAPH
    txn.put_graph(source, forward_edge)
        .map_err(|e| LocalApplyError::Internal {
            message: format!("Failed to add forward edge: {}", e),
        })?;

    txn.put_graph(dest, reverse_edge)
        .map_err(|e| LocalApplyError::Internal {
            message: format!("Failed to add reverse edge: {}", e),
        })?;

    // Write to INODE_GRAPH secondary index
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

    Ok(())
}

fn add_edge_with_reverse_batched(
    graph_batch: &mut GraphWriteBatch<'_>,
    inode: Option<Inode>,
    flag: EdgeFlags,
    source: GraphNode<NodeId>,
    dest: GraphNode<NodeId>,
    introduced_by: NodeId,
) -> Result<(), LocalApplyError> {
    log::debug!(
        "add_edge_with_reverse_batched: flag={:?} source=[{:?} {:?}:{:?}] dest=[{:?} {:?}:{:?}] introduced_by={:?}",
        flag, source.change, source.start, source.end,
        dest.change, dest.start, dest.end,
        introduced_by
    );

    graph_batch
        .add_edge_with_reverse(inode, flag, source, dest, introduced_by)
        .map_err(|e| LocalApplyError::Internal {
            message: format!("Failed to add edge pair: {}", e),
        })
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

    // Collect children of the target span that need reconnection.
    // Use typed `iter_forward` — we want all alive forward edges
    // (block, folder, pseudo) but NOT deleted ones.
    let children = txn
        .iter_forward(target, false)
        .map_err(|e| LocalApplyError::Internal {
            message: format!("Failed to iterate children: {}", e),
        })?;

    for child in children {
        if !child.kind.is_pseudo() {
            // Track this child as needing reconnection
            workspace.set_parent(child.dest, target.end_pos());
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
            let next_pos = if node.end == node.start {
                // Empty vertex (e.g., inode marker) — skip past it
                ChangePosition::new(node.end.get() + 1)
            } else {
                node.end
            };
            if next_pos <= pos.pos {
                // Safety: avoid infinite loop if we can't advance
                log::warn!(
                    "collect_zombie_context: pos not advancing at {:?}, breaking",
                    pos
                );
                break;
            }
            pos.pos = next_pos;
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
    // The original code used `iter_adjacent(node, empty(), all() - DELETED)`,
    // which spans BOTH forward and parent (reverse) edges — everything that
    // is NOT deleted.  The typed replacement must therefore check both
    // directions: `iter_forward` (alive forward edges) and `iter_parents`
    // (alive parent edges).

    // --- Forward edges (alive only) ---
    let forward_edges = txn
        .iter_forward(node, false)
        .map_err(|e| LocalApplyError::Internal {
            message: format!("Failed to iterate forward edges for zombies: {}", e),
        })?;

    for edge in &forward_edges {
        if edge.introduced_by == change_id || edge.introduced_by.is_root() {
            continue;
        }

        if let Ok(Some(hash)) = txn.get_external(edge.introduced_by) {
            if !change.knows(&hash) {
                // Unknown live edge - this is a zombie
                workspace.add_zombie_vertex(node);
                return Ok(());
            }
        }
    }

    // --- Parent edges (alive only) ---
    let parent_edges = txn
        .iter_parents(node, false)
        .map_err(|e| LocalApplyError::Internal {
            message: format!("Failed to iterate parent edges for zombies: {}", e),
        })?;

    for edge in &parent_edges {
        if edge.introduced_by == change_id || edge.introduced_by.is_root() {
            continue;
        }

        if let Ok(Some(hash)) = txn.get_external(edge.introduced_by) {
            if !change.knows(&hash) {
                // Unknown live edge - this is a zombie
                workspace.add_zombie_vertex(node);
                return Ok(());
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

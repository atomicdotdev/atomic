//! Vertex classification helpers for graph retrieval.
//!
//! This module provides functions to determine whether a graph vertex is
//! alive, dead, or a zombie (deleted but with live connections). These
//! classifications drive the DFS traversal in [`super::retrieve_graph`].

use super::super::vertex::{AliveVertex, VertexFlags};
use crate::pristine::{GraphTxnT, PristineError};
use crate::types::{GraphNode, NodeId, ParentEdgeKind, Position};

/// Create an AliveVertex from an already-resolved span, if it's alive.
///
/// This function:
/// 1. Checks if the span is alive (has non-deleted edges)
/// 2. Checks if it's a zombie (deleted but has live connections)
///
/// # Arguments
///
/// * `txn` - Transaction for graph queries
/// * `node` - The already-resolved span to check
///
/// # Returns
///
/// - `Ok(Some(alive_vertex))` if the span is alive or zombie
/// - `Ok(None)` if the span is not alive and not a zombie
/// - `Err(_)` on database error
pub(super) fn create_alive_vertex<T: GraphTxnT>(
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
pub(super) fn new_vertex_at_position<T: GraphTxnT>(
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
/// A node is alive if it has at least one non-deleted, non-pseudo-only
/// parent edge. For empty vertices (inodes), any non-deleted parent
/// — including pseudo parents — proves aliveness.
///
/// Uses typed [`ParentEdgeKind`] matching instead of raw `EdgeFlags`
/// bitflag checks, so every case is visible and the compiler rejects
/// missing arms.
pub(super) fn is_vertex_alive<T: GraphTxnT>(
    txn: &T,
    node: &GraphNode<NodeId>,
) -> Result<bool, PristineError> {
    // Root node is always alive
    if node.is_root() {
        return Ok(true);
    }

    // Iterate non-deleted parent edges only
    let parents = txn.iter_parents(*node, false)?;

    for parent in &parents {
        match parent.kind {
            // Real (non-pseudo) parent edges prove aliveness for any vertex
            ParentEdgeKind::Block | ParentEdgeKind::Folder => return Ok(true),

            // Pseudo parents prove aliveness only for empty vertices (inodes)
            ParentEdgeKind::PseudoBlock | ParentEdgeKind::PseudoFolder => {
                if node.is_empty() {
                    return Ok(true);
                }
                // For content vertices, pseudo parents alone don't prove aliveness
            }

            // BlockDeleted/FolderDeleted are excluded by include_deleted=false,
            // but handle them explicitly for exhaustiveness
            ParentEdgeKind::BlockDeleted | ParentEdgeKind::FolderDeleted => {}
        }
    }

    Ok(false)
}

/// Check if a node is a zombie (deleted but with live connections).
///
/// A zombie node has at least one deleted block parent edge, meaning
/// something explicitly deleted it. Uses typed [`ParentEdgeKind`]
/// matching for clarity.
pub(super) fn is_vertex_zombie<T: GraphTxnT>(
    txn: &T,
    node: &GraphNode<NodeId>,
) -> Result<bool, PristineError> {
    // Include deleted edges so we can see deletion markers
    let parents = txn.iter_parents(*node, true)?;

    for parent in &parents {
        if parent.kind == ParentEdgeKind::BlockDeleted {
            return Ok(true); // Has a deleted block parent = zombie
        }
    }

    Ok(false)
}

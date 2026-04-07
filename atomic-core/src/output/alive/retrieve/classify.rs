//! Vertex classification helpers for graph retrieval.
//!
//! This module provides functions to determine whether a graph vertex is
//! alive, dead, or a zombie (deleted but with live connections). These
//! classifications drive the DFS traversal in [`super::retrieve_graph`].

use super::super::vertex::{AliveVertex, VertexFlags};
use crate::pristine::{GraphTxnT, PristineError};
use crate::types::{EdgeFlags, GraphNode, NodeId, Position};

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
/// A node is alive if it has at least one non-deleted edge.
pub(super) fn is_vertex_alive<T: GraphTxnT>(
    txn: &T,
    node: &GraphNode<NodeId>,
) -> Result<bool, PristineError> {
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
pub(super) fn is_vertex_zombie<T: GraphTxnT>(
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

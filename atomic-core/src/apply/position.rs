//! Position resolution for change application
//!
//! This module provides functions for converting between external positions
//! (using content hashes) and internal positions (using repository-local NodeIds).
//!
//! # Overview
//!
//! Changes are serialized with external `Hash` references, but the graph uses
//! internal `NodeId` references. This module bridges that gap.
//!
//! # Position Types
//!
//! - **External Position**: `Position<Option<Hash>>` - Uses content hashes
//!   - `Some(hash)` references another change
//!   - `None` references the change being applied (self-reference)
//!
//! - **Internal Position**: `Position<NodeId>` - Uses repository-local IDs
//!   - Obtained by looking up hashes in the internal/external tables
//!
//! # Example
//!
//! ```rust,ignore
//! // Resolve an external position from a serialized change
//! let external_pos = Position { change: Some(hash), pos: ChangePosition::new(42) };
//! let internal_pos = resolve_position(&txn, &external_pos, current_change_id)?;
//!
//! // Now we can use it with the graph
//! let found = txn.find_block(internal_pos)?;
//! ```

use crate::pristine::{GraphTxnT, MutTxnT, TreeTxnT};

use super::edge::FindBlockMode;
use crate::types::{GraphNode, Hash, Inode, NodeId, Position};

use super::error::LocalApplyError;

// Position Resolution

/// Convert an external position (with Hash) to an internal position (with NodeId).
///
/// This is the bridge between the serialized change format (using hashes) and
/// the internal graph format (using NodeIds).
///
/// # Arguments
///
/// * `txn` - Transaction for looking up internal IDs
/// * `pos` - External position with optional hash
/// * `current_change` - NodeId of the change being applied (used for self-references)
///
/// # Returns
///
/// The internal position with NodeId.
///
/// # Special Cases
///
/// - When `pos.change` is `None`, it refers to the change being applied itself.
///   In this case, `current_change` is used as the NodeId.
/// - When `pos.change` is `Some(Hash::NONE)`, it refers to the ROOT position.
///   This is the virtual root span that all top-level files reference.
///
/// # Errors
///
/// Returns `LocalApplyError::DependencyMissing` if the referenced change
/// is not registered in the repository.
pub fn resolve_position<T: GraphTxnT>(
    txn: &T,
    pos: &Position<Option<Hash>>,
    current_change: NodeId,
) -> Result<Position<NodeId>, LocalApplyError> {
    let change_id = match pos.change {
        Some(hash) if hash.is_none() => {
            // Hash::NONE (all zeros) represents the ROOT position.
            // This is a virtual span that doesn't exist in the database
            // but serves as the parent for all top-level files.
            NodeId::ROOT
        }
        Some(hash) => {
            // External reference - look up the internal ID
            txn.get_internal(&hash)
                .map_err(|e| LocalApplyError::Internal {
                    message: format!("Failed to resolve position: {}", e),
                })?
                .ok_or(LocalApplyError::DependencyMissing { hash })?
        }
        None => {
            // Self-reference - use the current change ID
            current_change
        }
    };

    Ok(Position {
        change: change_id,
        pos: pos.pos,
    })
}

/// Convert an external span (with Hash) to an internal span (with NodeId).
///
/// Similar to `resolve_position`, but for vertices which include start and end.
///
/// # Arguments
///
/// * `txn` - Transaction for looking up internal IDs
/// * `span` - External span with optional hash
/// * `current_change` - NodeId of the change being applied
///
/// # Returns
///
/// The internal span with NodeId.
///
/// # Special Cases
///
/// - When `node.change` is `None`, it refers to the change being applied itself.
/// - When `node.change` is `Some(Hash::NONE)`, it refers to the ROOT span.
///
/// # Errors
///
/// Returns `LocalApplyError::DependencyMissing` if the referenced change
/// is not registered in the repository.
pub fn resolve_vertex<T: GraphTxnT>(
    txn: &T,
    node: &GraphNode<Option<Hash>>,
    current_change: NodeId,
) -> Result<GraphNode<NodeId>, LocalApplyError> {
    let change_id = match node.change {
        Some(hash) if hash.is_none() => {
            // Hash::NONE represents the ROOT span
            NodeId::ROOT
        }
        Some(hash) => txn
            .get_internal(&hash)
            .map_err(|e| LocalApplyError::Internal {
                message: format!("Failed to resolve node: {}", e),
            })?
            .ok_or(LocalApplyError::DependencyMissing { hash })?,
        None => current_change,
    };

    Ok(GraphNode {
        change: change_id,
        start: node.start,
        end: node.end,
    })
}

/// Resolve an inode position to an actual Inode value.
///
/// Given a position referencing a file's inode span, looks up the actual
/// Inode value from the position→inode mapping.
///
/// # Arguments
///
/// * `txn` - Transaction for lookups
/// * `inode_pos` - Position of the inode span
/// * `current_change` - NodeId of the change being applied
///
/// # Returns
///
/// * `Ok(Some(inode))` - The resolved inode
/// * `Ok(None)` - Position is ROOT or inode not found
/// * `Err(_)` - Lookup failed
///
/// # Notes
///
/// ROOT positions don't have an associated inode, so this returns `None`
/// for those cases.
pub fn resolve_inode<T: GraphTxnT + TreeTxnT>(
    txn: &T,
    inode_pos: &Position<Option<Hash>>,
    current_change: NodeId,
) -> Result<Option<Inode>, LocalApplyError> {
    let internal_pos = resolve_position(txn, inode_pos, current_change)?;

    // ROOT positions don't have an associated inode
    if internal_pos.change.is_root() {
        return Ok(None);
    }

    // Look up the Inode from the position
    txn.position_inode(internal_pos)
        .map_err(|e| LocalApplyError::Internal {
            message: format!("Failed to resolve inode: {}", e),
        })
}

/// Resolve the `introduced_by` field of an edge.
///
/// Returns the NodeId of the change that introduced the original edge.
/// This is used when modifying edges to track provenance.
///
/// # Arguments
///
/// * `txn` - Transaction for lookups
/// * `introduced_by` - Optional hash of the introducing change
/// * `current_change` - NodeId of the change being applied
///
/// # Returns
///
/// The NodeId of the introducing change.
pub fn resolve_introduced_by<T: GraphTxnT>(
    txn: &T,
    introduced_by: &Option<Hash>,
    current_change: NodeId,
) -> Result<NodeId, LocalApplyError> {
    match introduced_by {
        Some(hash) if hash.is_none() => {
            // Hash::NONE represents the ROOT node
            Ok(NodeId::ROOT)
        }
        Some(hash) => txn
            .get_internal(hash)
            .map_err(|e| LocalApplyError::Internal {
                message: format!("Failed to resolve introduced_by: {}", e),
            })?
            .ok_or(LocalApplyError::DependencyMissing { hash: *hash }),
        None => Ok(current_change),
    }
}

/// Resolve a context position to a span.
///
/// Given a position, finds the span that contains it or ends at it.
/// For predecessors, we use `find_block_end` to find the span that ENDS at
/// this position (since predecessors references the end of a predecessor).
/// For successors, we use `find_block` to find the span starting at
/// or containing this position.
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `pos` - Internal position to resolve
/// * `is_predecessor` - Whether this is an predecessors (affects lookup method)
///
/// # Returns
///
/// The span for the context, possibly adjusted for context type.
///
/// # Span Splitting
///
/// If the position falls in the middle of a span, the returned span
/// will be conceptually "split" - we return only the relevant portion.
/// The actual split would be performed during application if needed.
///
/// # Up vs Down Context
///
/// - **Up context**: References the END of a predecessor span. We use
///   `find_block_end` to find a span ending at this position.
/// - **Down context**: References the START of a successor span. We use
///   `find_block` to find a span containing this position.
pub fn resolve_context_vertex<T: GraphTxnT>(
    txn: &T,
    pos: Position<NodeId>,
    is_predecessor: bool,
) -> Result<GraphNode<NodeId>, LocalApplyError> {
    if pos.change.is_root() {
        return Ok(GraphNode::root());
    }

    let found = if is_predecessor {
        txn.find_block_end(pos)
    } else {
        txn.find_block(pos)
    }
    .map_err(|_| LocalApplyError::BlockNotFound { position: pos })?;

    Ok(adjust_for_mid_span(found, pos, is_predecessor))
}

// ---------------------------------------------------------------------------
// Mid-span adjustment
// ---------------------------------------------------------------------------

/// Adjust a found vertex when the context position falls mid-span.
///
/// Context positions may reference a byte offset inside a larger vertex.
/// When that happens the caller needs only the *relevant portion* of the
/// vertex — everything up to the position for predecessors, or everything
/// from the position onward for successors.
///
/// This helper is shared by both `resolve_context_vertex` and
/// `resolve_context_vertex_for_target` so the splitting logic has a
/// single definition.
#[inline]
fn adjust_for_mid_span(
    found: GraphNode<NodeId>,
    pos: Position<NodeId>,
    is_predecessor: bool,
) -> GraphNode<NodeId> {
    if is_predecessor {
        // Predecessor: return the portion up to `pos`
        if found.end > pos.pos && found.start < pos.pos {
            GraphNode {
                change: found.change,
                start: found.start,
                end: pos.pos,
            }
        } else {
            found
        }
    } else {
        // Successor: return the portion from `pos` onward
        if found.start < pos.pos && found.end > pos.pos {
            GraphNode {
                change: found.change,
                start: pos.pos,
                end: found.end,
            }
        } else {
            found
        }
    }
}

// ---------------------------------------------------------------------------
// Overlay-aware context resolution for the apply pipeline
// ---------------------------------------------------------------------------

/// Resolve a context vertex using the overlay-aware vertex finder.
///
/// This is the apply-pipeline counterpart of [`resolve_context_vertex`].
/// It delegates vertex lookup to `super::edge::resolve_vertex_for_target`,
/// which consults STACK_GRAPH (via the shared `overlay::find_block_in_stack_graph`)
/// before falling back to the global GRAPH.  For `ApplyTarget::Global` the
/// behaviour is identical to the non-overlay version.
///
/// The mid-span adjustment logic is shared with `resolve_context_vertex`
/// via `adjust_for_mid_span`, ensuring a single source of truth.
pub fn resolve_context_vertex_for_target<T: MutTxnT>(
    txn: &T,
    pos: Position<NodeId>,
    is_predecessor: bool,
    target: &super::ApplyTarget,
) -> Result<GraphNode<NodeId>, LocalApplyError> {
    use super::edge::resolve_vertex_for_target;

    if pos.change.is_root() {
        return Ok(GraphNode::root());
    }

    let mode = if is_predecessor {
        FindBlockMode::EndingAtPosition
    } else {
        FindBlockMode::ContainingPosition
    };

    let found = resolve_vertex_for_target(txn, pos, target, mode)?;
    Ok(adjust_for_mid_span(found, pos, is_predecessor))
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    // Test Helpers

    fn make_external_position(hash: Option<Hash>, pos: u64) -> Position<Option<Hash>> {
        Position {
            change: hash,
            pos: ChangePosition::new(pos),
        }
    }

    fn make_external_vertex(hash: Option<Hash>, start: u64, end: u64) -> GraphNode<Option<Hash>> {
        GraphNode {
            change: hash,
            start: ChangePosition::new(start),
            end: ChangePosition::new(end),
        }
    }

    // Position Helper Tests

    #[test]
    fn test_make_external_position_with_hash() {
        let hash = Hash::of(b"test change");
        let pos = make_external_position(Some(hash), 42);

        assert_eq!(pos.change, Some(hash));
        assert_eq!(pos.pos, ChangePosition::new(42));
    }

    #[test]
    fn test_make_external_position_self_reference() {
        let pos = make_external_position(None, 100);

        assert!(pos.change.is_none());
        assert_eq!(pos.pos, ChangePosition::new(100));
    }

    #[test]
    fn test_make_external_vertex_with_hash() {
        let hash = Hash::of(b"test change");
        let node = make_external_vertex(Some(hash), 10, 20);

        assert_eq!(node.change, Some(hash));
        assert_eq!(node.start, ChangePosition::new(10));
        assert_eq!(node.end, ChangePosition::new(20));
    }

    #[test]
    fn test_make_external_vertex_self_reference() {
        let node = make_external_vertex(None, 0, 50);

        assert!(node.change.is_none());
        assert_eq!(node.start, ChangePosition::new(0));
        assert_eq!(node.end, ChangePosition::new(50));
    }

    // Position Equality Tests

    #[test]
    fn test_external_position_equality() {
        let hash = Hash::of(b"test");
        let pos1 = make_external_position(Some(hash), 42);
        let pos2 = make_external_position(Some(hash), 42);
        let pos3 = make_external_position(Some(hash), 43);

        assert_eq!(pos1, pos2);
        assert_ne!(pos1, pos3);
    }

    #[test]
    fn test_external_vertex_equality() {
        let hash = Hash::of(b"test");
        let v1 = make_external_vertex(Some(hash), 10, 20);
        let v2 = make_external_vertex(Some(hash), 10, 20);
        let v3 = make_external_vertex(Some(hash), 10, 21);

        assert_eq!(v1, v2);
        assert_ne!(v1, v3);
    }

    // Internal Position Tests

    #[test]
    fn test_internal_position_from_self_reference() {
        // When change is None, the current_change NodeId should be used
        let pos = make_external_position(None, 42);
        let current = NodeId::new(123);

        // We can't test resolve_position without a real transaction,
        // but we can verify the input structure is correct
        assert!(pos.change.is_none());
        assert!(!current.is_root());
    }

    #[test]
    fn test_internal_vertex_from_self_reference() {
        let node = make_external_vertex(None, 10, 20);
        let current = NodeId::new(456);

        assert!(node.change.is_none());
        assert!(!current.is_root());
        assert_eq!(node.start, ChangePosition::new(10));
        assert_eq!(node.end, ChangePosition::new(20));
    }

    // ROOT Position Tests

    #[test]
    fn test_root_node_id() {
        let root = NodeId::ROOT;
        assert!(root.is_root());
    }

    #[test]
    fn test_root_vertex() {
        let root = GraphNode::<NodeId>::root();
        assert!(root.is_root());
        assert!(root.is_empty());
    }

    #[test]
    fn test_non_root_node_id() {
        let node = NodeId::new(42);
        assert!(!node.is_root());
    }

    // Hash Resolution Tests (Structure Only)

    #[test]
    fn test_hash_creates_unique_positions() {
        let hash1 = Hash::of(b"change 1");
        let hash2 = Hash::of(b"change 2");

        let pos1 = make_external_position(Some(hash1), 0);
        let pos2 = make_external_position(Some(hash2), 0);

        // Same position offset but different changes
        assert_ne!(pos1.change, pos2.change);
        assert_eq!(pos1.pos, pos2.pos);
    }

    #[test]
    fn test_position_with_different_offsets() {
        let hash = Hash::of(b"change");

        let pos1 = make_external_position(Some(hash), 0);
        let pos2 = make_external_position(Some(hash), 100);

        // Same change but different offsets
        assert_eq!(pos1.change, pos2.change);
        assert_ne!(pos1.pos, pos2.pos);
    }

    // Span Range Tests

    #[test]
    fn test_vertex_range_validation() {
        let hash = Hash::of(b"change");
        let node = make_external_vertex(Some(hash), 10, 20);

        // Verify start < end for valid span
        assert!(node.start < node.end);
    }

    #[test]
    fn test_empty_vertex() {
        let hash = Hash::of(b"change");
        let node = make_external_vertex(Some(hash), 10, 10);

        // Empty span has start == end
        assert_eq!(node.start, node.end);
    }

    #[test]
    fn test_vertex_length() {
        let hash = Hash::of(b"change");
        let node = make_external_vertex(Some(hash), 10, 25);

        // Length should be end - start = 15
        let len: u64 = node.end.get();
        let start: u64 = node.start.get();
        assert_eq!(len - start, 15);
    }

    // Context Direction Tests

    #[test]
    fn test_up_context_structure() {
        // Up context: vertices that come BEFORE new content
        // We connect FROM predecessors TO new span
        let hash = Hash::of(b"parent");
        let up_ctx = make_external_position(Some(hash), 50);

        // The position points to the END of the parent content
        assert_eq!(up_ctx.pos, ChangePosition::new(50));
    }

    #[test]
    fn test_down_context_structure() {
        // Down context: vertices that come AFTER new content
        // We connect FROM new span TO successors
        let hash = Hash::of(b"child");
        let down_ctx = make_external_position(Some(hash), 0);

        // The position points to the START of the child content
        assert_eq!(down_ctx.pos, ChangePosition::new(0));
    }

    // Error Case Tests (Structure Only)

    #[test]
    fn test_dependency_missing_error_structure() {
        let hash = Hash::of(b"missing change");
        let error = LocalApplyError::DependencyMissing { hash };

        // Verify error contains the hash
        match error {
            LocalApplyError::DependencyMissing { hash: h } => {
                assert_eq!(h, hash);
            }
            _ => panic!("Wrong error type"),
        }
    }

    #[test]
    fn test_block_not_found_error_structure() {
        let pos = Position {
            change: NodeId::new(42),
            pos: ChangePosition::new(100),
        };
        let error = LocalApplyError::BlockNotFound { position: pos };

        // Verify error contains the position
        match error {
            LocalApplyError::BlockNotFound { position: p } => {
                assert_eq!(p.change, NodeId::new(42));
                assert_eq!(p.pos, ChangePosition::new(100));
            }
            _ => panic!("Wrong error type"),
        }
    }

    #[test]
    fn test_internal_error_structure() {
        let error = LocalApplyError::Internal {
            message: "Test error message".to_string(),
        };

        match error {
            LocalApplyError::Internal { message } => {
                assert!(message.contains("Test error"));
            }
            _ => panic!("Wrong error type"),
        }
    }
}

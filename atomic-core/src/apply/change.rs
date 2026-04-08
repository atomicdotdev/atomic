//! Core change application functions
//!
//! This module provides the functions for applying a [`Change`] to the
//! repository graph. Application is the process of taking a change's
//! atoms (vertices and edges) and modifying the graph accordingly.
//!
//! # Overview
//!
//! Applying a change involves:
//!
//! 1. **Verify dependencies**: All required changes must be present
//! 2. **Register the change**: Get an internal [`NodeId`] for the change
//! 3. **Apply atoms**: Process each graph_op's atoms (vertices and edges)
//! 4. **Update stack state**: Add to change log and update Merkle state
//! 5. **Handle conflicts**: Track zombies and missing contexts
//!
//! # Design Notes
//!
//! Changes in Atomic use different type parameters at different stages:
//!
//! - `GraphOp<Option<Hash>>`: During recording (change hash not yet known)
//! - `GraphOp<Hash>`: After serialization (all hashes resolved)
//!
//! The apply functions work with the internal `NodeId` type after
//! resolving external hashes.
//!
//! [`Change`]: crate::change::Change
//! [`NodeId`]: crate::types::NodeId

use crate::change::Change;
use crate::pristine::{GraphTxnT, PristineError, ViewState, ViewTxnT};
use crate::types::{Hash, Merkle, NodeId};

use super::error::LocalApplyError;

/// Verify that all dependencies of a change are present in the repository.
///
/// Before a change can be applied, all changes it depends on must already
/// be registered in the repository. This function checks for any missing
/// dependencies.
///
/// # Arguments
///
/// * `txn` - The transaction to check against
/// * `change` - The change whose dependencies to verify
///
/// # Returns
///
/// A vector of missing dependency hashes. Empty if all dependencies are present.
///
/// # Example
///
/// ```rust,ignore
/// let missing = verify_dependencies(&txn, &change)?;
/// if !missing.is_empty() {
///     println!("Missing {} dependencies", missing.len());
///     for hash in &missing {
///         println!("  - {}", hash);
///     }
/// }
/// ```
pub fn verify_dependencies<T: GraphTxnT>(
    txn: &T,
    change: &Change,
) -> Result<Vec<Hash>, PristineError> {
    let mut missing = Vec::new();

    for dep_hash in change.dependencies() {
        if txn.get_internal(dep_hash)?.is_none() {
            missing.push(*dep_hash);
        }
    }

    Ok(missing)
}

/// Check if a change has already been applied to a stack.
///
/// # Arguments
///
/// * `txn` - The transaction to check against
/// * `stack` - The stack to check
/// * `change_id` - The internal ID of the change
///
/// # Returns
///
/// `true` if the change is already on the stack.
///
/// # Example
///
/// ```rust,ignore
/// let change_id = txn.get_internal(&change_hash)?.unwrap();
/// if is_change_on_view(&txn, &view, change_id)? {
///     println!("Change already applied");
/// }
/// ```
pub fn is_change_on_view<T: ViewTxnT>(
    txn: &T,
    view: &ViewState,
    change_id: NodeId,
) -> Result<bool, PristineError> {
    Ok(txn.get_change_seq(view, change_id)?.is_some())
}

/// Compute the new Merkle state after applying a change.
///
/// The Merkle state is computed incrementally:
/// `new_state = Hash(old_state || change_hash)`
///
/// This provides a unique identifier for the sequence of changes
/// applied to a stack.
///
/// # Arguments
///
/// * `current_state` - The stack's current Merkle state
/// * `change_hash` - The hash of the change being applied
///
/// # Returns
///
/// The new Merkle state after applying the change.
///
/// # Example
///
/// ```rust
/// use atomic_core::apply::compute_new_state;
/// use atomic_core::types::{Merkle, Hash};
///
/// let current = Merkle::ZERO;
/// let change_hash = Hash::of(b"my change");
/// let new_state = compute_new_state(&current, &change_hash);
///
/// // State changes after applying
/// assert_ne!(new_state, current);
///
/// // Same inputs produce same output (deterministic)
/// let new_state2 = compute_new_state(&current, &change_hash);
/// assert_eq!(new_state, new_state2);
/// ```
pub fn compute_new_state(current_state: &Merkle, change_hash: &Hash) -> Merkle {
    current_state.next(change_hash)
}

/// Describes a change that should be applied to a stack.
///
/// This is a helper struct that bundles all the information needed
/// to apply a change.
#[derive(Debug, Clone)]
pub struct ChangeToApply {
    /// The internal ID of the change (from register_change)
    pub change_id: NodeId,
    /// The hash of the change
    pub change_hash: Hash,
}

impl ChangeToApply {
    /// Create a new ChangeToApply.
    pub fn new(change_id: NodeId, change_hash: Hash) -> Self {
        Self {
            change_id,
            change_hash,
        }
    }
}

/// Result of applying a change to a stack.
///
/// This contains the updated state after successful application.
#[derive(Debug, Clone)]
pub struct ApplyResult {
    /// The new Merkle state of the stack
    pub new_state: Merkle,
    /// The sequence number of the applied change
    pub sequence: u64,
    /// Whether any conflicts were detected
    pub has_conflicts: bool,
}

impl ApplyResult {
    /// Create a new ApplyResult.
    pub fn new(new_state: Merkle, sequence: u64, has_conflicts: bool) -> Self {
        Self {
            new_state,
            sequence,
            has_conflicts,
        }
    }
}

/// Check if applying a change would succeed (without actually applying).
///
/// This performs validation checks:
/// - Change not already on stack
/// - All dependencies present
///
/// # Arguments
///
/// * `txn` - The transaction to check against
/// * `stack` - The stack to check
/// * `change_id` - The internal ID of the change
/// * `change_hash` - The hash of the change
/// * `change` - The change to validate
///
/// # Returns
///
/// `Ok(())` if the change can be applied, or an error describing why not.
pub fn validate_can_apply<T: ViewTxnT + GraphTxnT>(
    txn: &T,
    view: &ViewState,
    change_id: NodeId,
    change_hash: &Hash,
    change: &Change,
) -> Result<(), LocalApplyError> {
    // Check if already applied
    if is_change_on_view(txn, view, change_id).map_err(|e| LocalApplyError::Internal {
        message: format!("Failed to check view: {}", e),
    })? {
        return Err(LocalApplyError::ChangeAlreadyApplied { hash: *change_hash });
    }

    // Check dependencies
    let missing = verify_dependencies(txn, change).map_err(|e| LocalApplyError::Internal {
        message: format!("Failed to verify dependencies: {}", e),
    })?;

    if let Some(first_missing) = missing.first() {
        return Err(LocalApplyError::DependencyMissing {
            hash: *first_missing,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    // Test Helpers

    fn test_hash() -> Hash {
        Hash::of(b"test hash")
    }

    fn test_hash2() -> Hash {
        Hash::of(b"test hash 2")
    }

    // compute_new_state Tests

    #[test]
    fn test_compute_new_state_from_zero() {
        let current = Merkle::ZERO;
        let change_hash = test_hash();

        let new_state = compute_new_state(&current, &change_hash);

        assert_ne!(new_state, current);
    }

    #[test]
    fn test_compute_new_state_deterministic() {
        let current = Merkle::ZERO;
        let change_hash = test_hash();

        let state1 = compute_new_state(&current, &change_hash);
        let state2 = compute_new_state(&current, &change_hash);

        assert_eq!(state1, state2);
    }

    #[test]
    fn test_compute_new_state_different_changes() {
        let current = Merkle::ZERO;

        let state1 = compute_new_state(&current, &test_hash());
        let state2 = compute_new_state(&current, &test_hash2());

        assert_ne!(state1, state2);
    }

    #[test]
    fn test_compute_new_state_chain() {
        let state0 = Merkle::ZERO;
        let hash1 = test_hash();
        let hash2 = test_hash2();

        let state1 = compute_new_state(&state0, &hash1);
        let state2 = compute_new_state(&state1, &hash2);

        // Each state is unique
        assert_ne!(state0, state1);
        assert_ne!(state1, state2);
        assert_ne!(state0, state2);
    }

    #[test]
    fn test_compute_new_state_order_matters() {
        let state0 = Merkle::ZERO;
        let hash1 = test_hash();
        let hash2 = test_hash2();

        // Apply in order: hash1, hash2
        let state_a = compute_new_state(&state0, &hash1);
        let state_a_final = compute_new_state(&state_a, &hash2);

        // Apply in order: hash2, hash1
        let state_b = compute_new_state(&state0, &hash2);
        let state_b_final = compute_new_state(&state_b, &hash1);

        // Order matters - different final states
        assert_ne!(state_a_final, state_b_final);
    }

    // ChangeToApply Tests

    #[test]
    fn test_change_to_apply_new() {
        let change_id = NodeId::new(42);
        let change_hash = test_hash();

        let cta = ChangeToApply::new(change_id, change_hash);

        assert_eq!(cta.change_id, change_id);
        assert_eq!(cta.change_hash, change_hash);
    }

    #[test]
    fn test_change_to_apply_clone() {
        let cta = ChangeToApply::new(NodeId::new(1), test_hash());
        let cloned = cta.clone();

        assert_eq!(cta.change_id, cloned.change_id);
        assert_eq!(cta.change_hash, cloned.change_hash);
    }

    #[test]
    fn test_change_to_apply_debug() {
        let cta = ChangeToApply::new(NodeId::new(1), test_hash());
        let debug = format!("{:?}", cta);

        assert!(debug.contains("ChangeToApply"));
    }

    // ApplyResult Tests

    #[test]
    fn test_apply_result_new() {
        let state = Merkle::of(b"test state");
        let result = ApplyResult::new(state, 5, false);

        assert_eq!(result.new_state, state);
        assert_eq!(result.sequence, 5);
        assert!(!result.has_conflicts);
    }

    #[test]
    fn test_apply_result_with_conflicts() {
        let state = Merkle::ZERO;
        let result = ApplyResult::new(state, 0, true);

        assert!(result.has_conflicts);
    }

    #[test]
    fn test_apply_result_clone() {
        let result = ApplyResult::new(Merkle::ZERO, 10, false);
        let cloned = result.clone();

        assert_eq!(result.new_state, cloned.new_state);
        assert_eq!(result.sequence, cloned.sequence);
        assert_eq!(result.has_conflicts, cloned.has_conflicts);
    }

    #[test]
    fn test_apply_result_debug() {
        let result = ApplyResult::new(Merkle::ZERO, 0, false);
        let debug = format!("{:?}", result);

        assert!(debug.contains("ApplyResult"));
    }

    // Error Type Tests

    #[test]
    fn test_dependency_missing_error() {
        let hash = test_hash();
        let err = LocalApplyError::DependencyMissing { hash };

        assert!(err.is_dependency_error());
        assert!(!err.is_already_applied());
        assert!(err.is_recoverable());
    }

    #[test]
    fn test_change_already_applied_error() {
        let hash = test_hash();
        let err = LocalApplyError::ChangeAlreadyApplied { hash };

        assert!(!err.is_dependency_error());
        assert!(err.is_already_applied());
        assert!(err.is_recoverable());
    }

    #[test]
    fn test_internal_error() {
        let err = LocalApplyError::Internal {
            message: "test error".to_string(),
        };

        let display = format!("{}", err);
        assert!(display.contains("Internal error"));
        assert!(display.contains("test error"));
        assert!(!err.is_recoverable());
    }

    // Stack State Tests

    #[test]
    fn test_view_state_initial() {
        let view = ViewState::new(1, "main".to_string());

        assert_eq!(view.id, 1);
        assert_eq!(view.name, "main");
        assert_eq!(view.state, Merkle::ZERO);
        assert_eq!(view.change_count, 0);
        assert!(view.is_empty());
    }

    #[test]
    fn test_view_state_simulate_apply() {
        let mut view = ViewState::new(1, "feature".to_string());
        let change_hash = test_hash();

        // Simulate what apply does
        let new_state = compute_new_state(&view.state, &change_hash);
        view.state = new_state;
        view.change_count += 1;

        assert_eq!(view.change_count, 1);
        assert!(!view.is_empty());
        assert_eq!(view.state, new_state);
    }

    #[test]
    fn test_view_state_multiple_applies() {
        let mut view = ViewState::new(1, "develop".to_string());

        // Apply 3 changes
        for i in 0..3 {
            let change_hash = Hash::of(format!("change {}", i).as_bytes());
            view.state = compute_new_state(&view.state, &change_hash);
            view.change_count += 1;
        }

        assert_eq!(view.change_count, 3);
    }

    // Position and Span Tests (Structure Verification)

    #[test]
    fn test_position_creation() {
        use crate::types::Position;

        let node_id = NodeId::new(42);
        let pos = Position::new(node_id, ChangePosition::new(100));

        assert_eq!(pos.change, node_id);
        assert_eq!(pos.pos, ChangePosition::new(100));
    }

    #[test]
    fn test_vertex_creation() {
        use crate::types::GraphNode;

        let node_id = NodeId::new(1);
        let node = GraphNode::new(node_id, ChangePosition::new(0), ChangePosition::new(50));

        assert_eq!(node.change, node_id);
        assert_eq!(node.start, ChangePosition::new(0));
        assert_eq!(node.end, ChangePosition::new(50));
        assert_eq!(node.len(), 50);
    }

    #[test]
    fn test_vertex_is_empty() {
        use crate::types::GraphNode;

        let empty_vertex = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(10),
            ChangePosition::new(10),
        );
        let non_empty = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(10),
            ChangePosition::new(20),
        );

        assert!(empty_vertex.is_empty());
        assert!(!non_empty.is_empty());
    }

    // Edge Flags Tests

    #[test]
    fn test_edge_flags_basic() {
        use crate::types::EdgeFlags;

        let flags = EdgeFlags::BLOCK;
        assert!(flags.is_block());
        assert!(!flags.is_folder());
        assert!(!flags.is_deleted());
    }

    #[test]
    fn test_edge_flags_combined() {
        use crate::types::EdgeFlags;

        let flags = EdgeFlags::BLOCK | EdgeFlags::DELETED;
        assert!(flags.is_block());
        assert!(flags.is_deleted());
        assert!(!flags.is_alive());
    }

    #[test]
    fn test_edge_flags_folder() {
        use crate::types::EdgeFlags;

        let flags = EdgeFlags::FOLDER | EdgeFlags::BLOCK;
        assert!(flags.is_folder());
        assert!(flags.is_block());
    }

    // Serialized Edge Tests

    #[test]
    fn test_serialized_edge_creation() {
        use crate::types::{EdgeFlags, Position, SerializedGraphEdge};

        let dest = Position::new(NodeId::new(5), ChangePosition::new(100));
        let introduced_by = NodeId::new(10);
        let flags = EdgeFlags::BLOCK;

        let edge = SerializedGraphEdge::new(flags, dest, introduced_by);

        assert_eq!(edge.flag(), flags);
        assert_eq!(edge.dest(), dest);
        assert_eq!(edge.introduced_by(), introduced_by);
    }

    #[test]
    fn test_serialized_edge_with_deletion() {
        use crate::types::{EdgeFlags, Position, SerializedGraphEdge};

        let dest = Position::new(NodeId::new(1), ChangePosition::new(0));
        let flags = EdgeFlags::BLOCK | EdgeFlags::DELETED;

        let edge = SerializedGraphEdge::new(flags, dest, NodeId::new(1));

        assert!(edge.flag().is_deleted());
        assert!(edge.flag().is_block());
    }
}

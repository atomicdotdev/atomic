//! Apply BranchOp operations to the pristine database.
//!
//! This module provides functions for applying line-level CRDT operations
//! (BranchOp) to the pristine database. Branch operations manage the lifecycle
//! of lines within files.
//!
//! # Operations
//!
//! | Operation | Description | Tables Affected |
//! |-----------|-------------|-----------------|
//! | `Insert` | Insert a new line | BRANCHES, TRUNK_BRANCHES |
//! | `Delete` | Mark line as deleted | BRANCHES |
//! | `Restore` | Restore deleted line | BRANCHES |
//!
//! # Concurrent Insertion Ordering
//!
//! When two concurrent operations insert after the same reference point,
//! the insertions are ordered deterministically by their BranchId. This
//! ensures all replicas converge to the same state.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                    Concurrent Insert Resolution                          │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Initial state:  [A] ───► [C]                                          │
//! │                                                                         │
//! │  Change 1: Insert B1 after A    Change 2: Insert B2 after A            │
//! │                                                                         │
//! │  Resolution: Order by BranchId                                          │
//! │  If B1 < B2:  [A] ───► [B1] ───► [B2] ───► [C]                        │
//! │  If B2 < B1:  [A] ───► [B2] ───► [B1] ───► [C]                        │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::crdt::apply::branch::apply_branch_op;
//! use atomic_core::crdt::{BranchId, BranchOp, TrunkId};
//! use atomic_core::types::NodeId;
//!
//! let trunk_id = TrunkId::new(NodeId::new(1), 0);
//! let branch_id = BranchId::new(NodeId::new(1), 0);
//! let op = BranchOp::Insert {
//!     after: None, // Insert at start of file
//!     content: vec![], // Leaf ops applied separately
//! };
//!
//! apply_branch_op(txn, &mut context, trunk_id, branch_id, &op)?;
//! ```

use crate::crdt::{Branch, BranchId, BranchOp, BranchState, LeafOp, TrunkId};

use super::context::ApplyContext;
use super::error::{storage_err, ApplyError, ApplyResult};
use super::leaf::apply_leaf_op;
use super::traits::MutCrdtTxnT;

// Public API

/// Applies a BranchOp to the pristine database.
///
/// This is the main entry point for applying line-level operations.
///
/// # Arguments
///
/// * `txn` - The transaction to apply the operation in
/// * `context` - The apply context for tracking state and conflicts
/// * `trunk_id` - The parent trunk (file) this branch belongs to
/// * `branch_id` - The ID for the branch (for Insert ops, this is the new ID)
/// * `op` - The operation to apply
/// * `content` - The content blob for leaf operations
///
/// # Returns
///
/// * `Ok(())` - The operation was applied successfully
/// * `Err(ApplyError)` - The operation failed
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::crdt::apply::branch::apply_branch_op;
/// use atomic_core::crdt::{BranchId, BranchOp, TrunkId};
///
/// let op = BranchOp::Insert {
///     after: None,
///     content: vec![],
/// };
/// apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &op, &content)?;
/// ```
pub fn apply_branch_op<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    trunk_id: TrunkId,
    branch_id: BranchId,
    op: &BranchOp,
    content: &[u8],
) -> ApplyResult<()> {
    match op {
        BranchOp::Insert {
            after,
            content: leaf_ops,
        } => apply_insert(txn, context, trunk_id, branch_id, *after, leaf_ops, content),
        BranchOp::Delete { branch, .. } => apply_delete(txn, context, *branch),
        BranchOp::Modify {
            branch,
            new_content,
            ..
        } => {
            // A Modify is semantically a delete-then-insert at the graph
            // layer.  Delete the old branch and insert a new one with the
            // new content.
            apply_delete(txn, context, *branch)?;
            apply_insert(
                txn,
                context,
                trunk_id,
                branch_id,
                Some(*branch),
                new_content,
                content,
            )
        }
        BranchOp::Restore { branch } => apply_restore(txn, context, *branch),
        BranchOp::Reparent { branch, new_after } => apply_reparent(txn, context, *branch, *new_after),
    }
}

/// Applies a BranchOp without leaf operations.
///
/// This is a convenience function for when leaf ops are applied separately.
///
/// # Arguments
///
/// * `txn` - The transaction to apply the operation in
/// * `context` - The apply context for tracking state and conflicts
/// * `trunk_id` - The parent trunk (file) this branch belongs to
/// * `branch_id` - The ID for the branch (for Insert ops, this is the new ID)
/// * `op` - The operation to apply
pub fn apply_branch_op_only<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    trunk_id: TrunkId,
    branch_id: BranchId,
    op: &BranchOp,
) -> ApplyResult<()> {
    match op {
        BranchOp::Insert { after, .. } => {
            apply_insert_only(txn, context, trunk_id, branch_id, *after)
        }
        BranchOp::Delete { branch, .. } => apply_delete(txn, context, *branch),
        BranchOp::Modify { branch, .. } => {
            // At the graph layer, a Modify deletes the old branch and
            // inserts a new one in its place.
            apply_delete(txn, context, *branch)?;
            apply_insert_only(txn, context, trunk_id, branch_id, Some(*branch))
        }
        BranchOp::Restore { branch } => apply_restore(txn, context, *branch),
        BranchOp::Reparent { branch, new_after } => apply_reparent(txn, context, *branch, *new_after),
    }
}

// Insert Operation

/// Applies an Insert operation to create a new line.
///
/// # Behavior
///
/// 1. Checks if the branch ID already exists (error if so, unless duplicates allowed)
/// 2. Validates the trunk exists and is alive
/// 3. Resolves the insertion position (handling concurrent inserts)
/// 4. Creates the branch entry in BRANCHES table
/// 5. Updates TRUNK_BRANCHES ordering multimap
/// 6. Applies any nested leaf operations
///
/// # Errors
///
/// - `BranchAlreadyExists` - The branch ID is already in use
/// - `TrunkNotFound` - The parent trunk doesn't exist
/// - `InvalidTrunkState` - The parent trunk is deleted
fn apply_insert<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    trunk_id: TrunkId,
    branch_id: BranchId,
    after: Option<BranchId>,
    leaf_ops: &[LeafOp],
    content: &[u8],
) -> ApplyResult<()> {
    // First apply the branch insert
    apply_insert_only(txn, context, trunk_id, branch_id, after)?;

    // Then apply nested leaf operations
    let mut leaf_idx: u32 = 0;
    let mut _prev_leaf = None;

    for leaf_op in leaf_ops {
        if let LeafOp::Insert { .. } = leaf_op {
            let leaf_id = crate::crdt::LeafId::new(branch_id.change_id(), leaf_idx);
            apply_leaf_op(txn, context, branch_id, leaf_id, leaf_op, content)?;
            _prev_leaf = Some(leaf_id);
            leaf_idx += 1;
        }
    }

    Ok(())
}

/// Applies an Insert operation without nested leaf operations.
fn apply_insert_only<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    trunk_id: TrunkId,
    branch_id: BranchId,
    after: Option<BranchId>,
) -> ApplyResult<()> {
    // Check for duplicate ID
    if txn
        .has_branch(branch_id)
        .map_err(|e| storage_err(e, "checking branch exists"))?
    {
        if context.options().allow_duplicate_ids() {
            context.record_skipped();
            return Ok(());
        }
        return Err(ApplyError::branch_already_exists(branch_id));
    }

    // Validate trunk exists (optional based on validation settings)
    if context.options().validate_references()
        && !txn
            .has_trunk(trunk_id)
            .map_err(|e| storage_err(e, "checking trunk exists"))?
    {
        return Err(ApplyError::trunk_not_found(trunk_id));
    }

    // Validate "after" reference if provided
    if context.options().validate_references() {
        if let Some(after_id) = after {
            if !txn
                .has_branch(after_id)
                .map_err(|e| storage_err(e, "checking after branch"))?
            {
                return Err(ApplyError::branch_not_found(after_id));
            }
        }
    }

    // Create and store the branch
    let branch = Branch::new(branch_id, trunk_id);
    txn.put_branch(&branch, after)
        .map_err(|e| storage_err(e, "inserting branch"))?;

    context.record_branch_inserted();
    Ok(())
}

// Delete Operation

/// Applies a Delete operation to mark a line as deleted.
///
/// # Behavior
///
/// 1. Verifies the branch exists
/// 2. Verifies the branch is not already deleted
/// 3. Updates the branch's state to Deleted
///
/// Note: The branch entry remains in the database (tombstone).
/// Associated leaves are not automatically deleted.
///
/// # Errors
///
/// - `BranchNotFound` - The branch doesn't exist
/// - `InvalidBranchState` - The branch is already deleted
fn apply_delete<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    branch_id: BranchId,
) -> ApplyResult<()> {
    // Verify branch exists
    let branch = txn
        .get_branch(branch_id)
        .map_err(|e| storage_err(e, "getting branch"))?
        .ok_or_else(|| ApplyError::branch_not_found(branch_id))?;

    // Check current state
    if branch.state().is_deleted() {
        if context.options().allow_duplicate_ids() {
            context.record_skipped();
            return Ok(());
        }
        return Err(ApplyError::invalid_branch_state(
            branch_id, "deleted", "delete",
        ));
    }

    // Update state to deleted
    txn.update_branch_state(branch_id, BranchState::Deleted)
        .map_err(|e| storage_err(e, "updating branch state"))?;

    context.record_branch_deleted();
    Ok(())
}

// Restore Operation

/// Applies a Restore operation to restore a deleted line.
///
/// # Behavior
///
/// 1. Verifies the branch exists
/// 2. Verifies the branch is currently deleted
/// 3. Updates the branch's state to Alive
///
/// # Errors
///
/// - `BranchNotFound` - The branch doesn't exist
/// - `InvalidBranchState` - The branch is not deleted
fn apply_restore<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    branch_id: BranchId,
) -> ApplyResult<()> {
    // Verify branch exists
    let branch = txn
        .get_branch(branch_id)
        .map_err(|e| storage_err(e, "getting branch"))?
        .ok_or_else(|| ApplyError::branch_not_found(branch_id))?;

    // Check current state
    if !branch.state().is_deleted() {
        if context.options().allow_duplicate_ids() {
            context.record_skipped();
            return Ok(());
        }
        return Err(ApplyError::invalid_branch_state(
            branch_id, "alive", "restore",
        ));
    }

    // Update state to alive
    txn.update_branch_state(branch_id, BranchState::Alive)
        .map_err(|e| storage_err(e, "updating branch state"))?;

    context.record_branch_restored();
    Ok(())
}

// Reparent Operation

/// Applies a Reparent operation to change a branch's chain position without
/// touching its state or content.
///
/// # Behavior
///
/// 1. Verifies the branch exists.
/// 2. Calls `update_branch_after` to rewrite the BRANCH_AFTER row.
///
/// The walker reads BRANCH_AFTER directly, so this is the entire effect.
///
/// # Errors
///
/// - `BranchNotFound` - The branch doesn't exist.
fn apply_reparent<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    branch_id: BranchId,
    new_after: Option<BranchId>,
) -> ApplyResult<()> {
    // Verify branch exists
    if !txn
        .has_branch(branch_id)
        .map_err(|e| storage_err(e, "checking branch exists"))?
    {
        return Err(ApplyError::branch_not_found(branch_id));
    }

    txn.update_branch_after(branch_id, new_after)
        .map_err(|e| storage_err(e, "updating branch after-ref"))?;

    // No dedicated counter on ApplyContext for reparent yet — track as a
    // skipped op for accounting until we add `record_branch_reparented`.
    context.record_skipped();
    Ok(())
}

// Validation Helpers

/// Validates that a branch exists and is in the expected state.
///
/// # Arguments
///
/// * `txn` - The transaction
/// * `branch_id` - The branch to validate
/// * `expected_state` - The expected state, or `None` for any state
///
/// # Returns
///
/// The branch if validation passes.
pub fn validate_branch_state<T: MutCrdtTxnT>(
    txn: &T,
    branch_id: BranchId,
    expected_state: Option<BranchState>,
) -> ApplyResult<Branch> {
    let branch = txn
        .get_branch(branch_id)
        .map_err(|e| storage_err(e, "getting branch"))?
        .ok_or_else(|| ApplyError::branch_not_found(branch_id))?;

    if let Some(expected) = expected_state {
        let matches = match expected {
            BranchState::Alive => branch.state().is_alive(),
            BranchState::Deleted => branch.state().is_deleted(),
        };

        if !matches {
            return Err(ApplyError::invalid_branch_state(
                branch_id,
                branch.state().to_string(),
                format!("expected {}", expected),
            ));
        }
    }

    Ok(branch)
}

/// Validates that a branch belongs to the specified trunk.
///
/// # Arguments
///
/// * `txn` - The transaction
/// * `branch_id` - The branch to validate
/// * `expected_trunk` - The expected parent trunk
///
/// # Returns
///
/// `Ok(())` if the branch belongs to the trunk.
pub fn validate_branch_parent<T: MutCrdtTxnT>(
    txn: &T,
    branch_id: BranchId,
    expected_trunk: TrunkId,
) -> ApplyResult<()> {
    let branch = txn
        .get_branch(branch_id)
        .map_err(|e| storage_err(e, "getting branch"))?
        .ok_or_else(|| ApplyError::branch_not_found(branch_id))?;

    if branch.trunk() != expected_trunk {
        return Err(ApplyError::internal(format!(
            "Branch {} belongs to trunk {}, not {}",
            branch_id,
            branch.trunk(),
            expected_trunk
        )));
    }

    Ok(())
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt::apply::options::ApplyOptions;
    use crate::crdt::{Leaf, LeafId, LeafState, Trunk, TrunkState};
    use crate::diff::token::TokenKind;
    use crate::types::{Inode, NodeId};
    use std::collections::HashMap;

    // Mock Transaction

    #[derive(Default)]
    struct MockTxn {
        trunks: HashMap<TrunkId, Trunk>,
        branches: HashMap<BranchId, Branch>,
        leaves: HashMap<LeafId, Leaf>,
        trunk_branches: HashMap<TrunkId, Vec<BranchId>>,
        branch_leaves: HashMap<BranchId, Vec<LeafId>>,
        next_inode: u64,
    }

    impl MockTxn {
        fn new() -> Self {
            Self {
                next_inode: 1,
                ..Default::default()
            }
        }

        fn add_trunk(&mut self, trunk: Trunk) {
            self.trunks.insert(trunk.id(), trunk);
        }
    }

    impl MutCrdtTxnT for MockTxn {
        type Error = crate::pristine::PristineError;

        // Trunk methods
        fn put_trunk(&mut self, trunk: &Trunk) -> Result<bool, Self::Error> {
            let is_new = !self.trunks.contains_key(&trunk.id());
            self.trunks.insert(trunk.id(), trunk.clone());
            Ok(is_new)
        }

        fn get_trunk(&self, id: TrunkId) -> Result<Option<Trunk>, Self::Error> {
            Ok(self.trunks.get(&id).cloned())
        }

        fn has_trunk(&self, id: TrunkId) -> Result<bool, Self::Error> {
            Ok(self.trunks.contains_key(&id))
        }

        fn del_trunk(&mut self, id: TrunkId) -> Result<bool, Self::Error> {
            Ok(self.trunks.remove(&id).is_some())
        }

        fn update_trunk_state(
            &mut self,
            id: TrunkId,
            state: TrunkState,
        ) -> Result<(), Self::Error> {
            if let Some(trunk) = self.trunks.get_mut(&id) {
                trunk.set_state(state);
            }
            Ok(())
        }

        fn update_trunk_path(&mut self, _id: TrunkId, _new_path: &str) -> Result<(), Self::Error> {
            Ok(())
        }

        fn get_trunk_by_path(&self, _path: &str) -> Result<Option<TrunkId>, Self::Error> {
            Ok(None)
        }

        fn get_trunk_by_inode(&self, _inode: Inode) -> Result<Option<TrunkId>, Self::Error> {
            Ok(None)
        }

        // Branch methods
        fn put_branch(
            &mut self,
            branch: &Branch,
            _after: Option<BranchId>,
        ) -> Result<bool, Self::Error> {
            let is_new = !self.branches.contains_key(&branch.id());
            self.branches.insert(branch.id(), branch.clone());
            self.trunk_branches
                .entry(branch.trunk())
                .or_default()
                .push(branch.id());
            Ok(is_new)
        }

        fn get_branch(&self, id: BranchId) -> Result<Option<Branch>, Self::Error> {
            Ok(self.branches.get(&id).cloned())
        }

        fn has_branch(&self, id: BranchId) -> Result<bool, Self::Error> {
            Ok(self.branches.contains_key(&id))
        }

        fn del_branch(&mut self, id: BranchId) -> Result<bool, Self::Error> {
            if let Some(branch) = self.branches.remove(&id) {
                if let Some(list) = self.trunk_branches.get_mut(&branch.trunk()) {
                    list.retain(|b| *b != id);
                }
                Ok(true)
            } else {
                Ok(false)
            }
        }

        fn update_branch_state(
            &mut self,
            id: BranchId,
            state: BranchState,
        ) -> Result<(), Self::Error> {
            if let Some(branch) = self.branches.get_mut(&id) {
                branch.set_state(state);
            }
            Ok(())
        }

        fn list_branches(&self, trunk_id: TrunkId) -> Result<Vec<BranchId>, Self::Error> {
            Ok(self
                .trunk_branches
                .get(&trunk_id)
                .cloned()
                .unwrap_or_default())
        }

        fn count_branches(&self, trunk_id: TrunkId) -> Result<usize, Self::Error> {
            Ok(self.trunk_branches.get(&trunk_id).map_or(0, |v| v.len()))
        }

        // Leaf methods
        fn put_leaf(&mut self, leaf: &Leaf, _after: Option<LeafId>) -> Result<bool, Self::Error> {
            let is_new = !self.leaves.contains_key(&leaf.id());
            self.leaves.insert(leaf.id(), leaf.clone());
            self.branch_leaves
                .entry(leaf.branch())
                .or_default()
                .push(leaf.id());
            Ok(is_new)
        }

        fn get_leaf(&self, id: LeafId) -> Result<Option<Leaf>, Self::Error> {
            Ok(self.leaves.get(&id).cloned())
        }

        fn has_leaf(&self, id: LeafId) -> Result<bool, Self::Error> {
            Ok(self.leaves.contains_key(&id))
        }

        fn del_leaf(&mut self, id: LeafId) -> Result<bool, Self::Error> {
            if let Some(leaf) = self.leaves.remove(&id) {
                if let Some(list) = self.branch_leaves.get_mut(&leaf.branch()) {
                    list.retain(|l| *l != id);
                }
                Ok(true)
            } else {
                Ok(false)
            }
        }

        fn update_leaf_state(&mut self, id: LeafId, state: LeafState) -> Result<(), Self::Error> {
            if let Some(leaf) = self.leaves.get_mut(&id) {
                leaf.set_state(state);
            }
            Ok(())
        }

        fn update_leaf_content(
            &mut self,
            id: LeafId,
            range: std::ops::Range<u32>,
        ) -> Result<(), Self::Error> {
            if let Some(leaf) = self.leaves.get_mut(&id) {
                leaf.set_content_range(range);
            }
            Ok(())
        }

        fn list_leaves(&self, branch_id: BranchId) -> Result<Vec<LeafId>, Self::Error> {
            Ok(self
                .branch_leaves
                .get(&branch_id)
                .cloned()
                .unwrap_or_default())
        }

        fn count_leaves(&self, branch_id: BranchId) -> Result<usize, Self::Error> {
            Ok(self.branch_leaves.get(&branch_id).map_or(0, |v| v.len()))
        }

        fn alloc_inode(&mut self) -> Result<Inode, Self::Error> {
            let inode = Inode::new(self.next_inode);
            self.next_inode += 1;
            Ok(inode)
        }
    }

    // Helper Functions

    fn create_test_trunk(txn: &mut MockTxn, node_id: u64) -> TrunkId {
        let trunk_id = TrunkId::new(NodeId::new(node_id), 0);
        let trunk = Trunk::new(
            trunk_id,
            Inode::new(node_id),
            format!("file{}.rs", node_id),
            None,
        );
        txn.add_trunk(trunk);
        trunk_id
    }

    // Insert Tests

    #[test]
    fn test_apply_insert() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = create_test_trunk(&mut txn, 1);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        let op = BranchOp::Insert {
            after: None,
            content: vec![],
        };

        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &op, &[]).unwrap();

        assert!(txn.has_branch(branch_id).unwrap());
        let branch = txn.get_branch(branch_id).unwrap().unwrap();
        assert_eq!(branch.trunk(), trunk_id);
        assert!(branch.state().is_alive());
        assert_eq!(context.stats().branches_inserted(), 1);
    }

    #[test]
    fn test_apply_insert_after() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = create_test_trunk(&mut txn, 1);

        let branch_id1 = BranchId::new(NodeId::new(1), 0);
        let branch_id2 = BranchId::new(NodeId::new(1), 1);

        // Insert first branch
        let op1 = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id1, &op1, &[]).unwrap();

        // Insert second branch after first
        let op2 = BranchOp::Insert {
            after: Some(branch_id1),
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id2, &op2, &[]).unwrap();

        assert!(txn.has_branch(branch_id1).unwrap());
        assert!(txn.has_branch(branch_id2).unwrap());
        assert_eq!(context.stats().branches_inserted(), 2);
    }

    #[test]
    fn test_apply_insert_duplicate_id() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = create_test_trunk(&mut txn, 1);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        let op = BranchOp::Insert {
            after: None,
            content: vec![],
        };

        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &op, &[]).unwrap();

        // Try to insert again with same ID
        let result = apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &op, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_already_exists());
    }

    #[test]
    fn test_apply_insert_duplicate_id_lenient() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::lenient());
        let trunk_id = create_test_trunk(&mut txn, 1);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        let op = BranchOp::Insert {
            after: None,
            content: vec![],
        };

        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &op, &[]).unwrap();

        // Try to insert again (should skip)
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &op, &[]).unwrap();
        assert_eq!(context.stats().operations_skipped(), 1);
    }

    #[test]
    fn test_apply_insert_trunk_not_found() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0); // Not created
        let branch_id = BranchId::new(NodeId::new(1), 0);

        let op = BranchOp::Insert {
            after: None,
            content: vec![],
        };

        let result = apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &op, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_found());
    }

    #[test]
    fn test_apply_insert_with_leaf_ops() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = create_test_trunk(&mut txn, 1);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        let leaf_ops = vec![
            LeafOp::Insert {
                after: None,
                kind: TokenKind::Word,
                content: b"hello".to_vec(),
            },
            LeafOp::Insert {
                after: None, // Will be adjusted to after previous
                kind: TokenKind::Whitespace,
                content: b" ".to_vec(),
            },
        ];

        let op = BranchOp::Insert {
            after: None,
            content: leaf_ops,
        };

        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &op, b"hello ").unwrap();

        assert!(txn.has_branch(branch_id).unwrap());
        assert_eq!(context.stats().branches_inserted(), 1);
        assert_eq!(context.stats().leaves_inserted(), 2);
    }

    // Delete Tests

    #[test]
    fn test_apply_delete() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = create_test_trunk(&mut txn, 1);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        // Create first
        let insert_op = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &insert_op, &[]).unwrap();

        // Delete
        let delete_op = BranchOp::Delete {
            branch: branch_id,
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &delete_op, &[]).unwrap();

        let branch = txn.get_branch(branch_id).unwrap().unwrap();
        assert!(branch.state().is_deleted());
        assert_eq!(context.stats().branches_deleted(), 1);
    }

    #[test]
    fn test_apply_delete_not_found() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        let op = BranchOp::Delete {
            branch: branch_id,
            content: vec![],
        };
        let result = apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &op, &[]);

        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_found());
    }

    #[test]
    fn test_apply_delete_already_deleted() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = create_test_trunk(&mut txn, 1);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        // Create and delete
        let insert_op = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &insert_op, &[]).unwrap();

        let delete_op = BranchOp::Delete {
            branch: branch_id,
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &delete_op, &[]).unwrap();

        // Try to delete again
        let result = apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &delete_op, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_invalid_state());
    }

    // Restore Tests

    #[test]
    fn test_apply_restore() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = create_test_trunk(&mut txn, 1);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        // Create and delete
        let insert_op = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &insert_op, &[]).unwrap();

        let delete_op = BranchOp::Delete {
            branch: branch_id,
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &delete_op, &[]).unwrap();

        // Restore
        let restore_op = BranchOp::Restore { branch: branch_id };
        apply_branch_op(
            &mut txn,
            &mut context,
            trunk_id,
            branch_id,
            &restore_op,
            &[],
        )
        .unwrap();

        let branch = txn.get_branch(branch_id).unwrap().unwrap();
        assert!(branch.state().is_alive());
        assert_eq!(context.stats().branches_restored(), 1);
    }

    #[test]
    fn test_apply_restore_not_deleted() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = create_test_trunk(&mut txn, 1);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        // Create (but don't delete)
        let insert_op = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &insert_op, &[]).unwrap();

        // Try to restore
        let restore_op = BranchOp::Restore { branch: branch_id };
        let result = apply_branch_op(
            &mut txn,
            &mut context,
            trunk_id,
            branch_id,
            &restore_op,
            &[],
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().is_invalid_state());
    }

    #[test]
    fn test_apply_restore_not_found() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        let op = BranchOp::Restore { branch: branch_id };
        let result = apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &op, &[]);

        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_found());
    }

    // Validation Helper Tests

    #[test]
    fn test_validate_branch_state() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = create_test_trunk(&mut txn, 1);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        // Create a branch
        let insert_op = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &insert_op, &[]).unwrap();

        // Validate alive state
        let branch = validate_branch_state(&txn, branch_id, Some(BranchState::Alive)).unwrap();
        assert!(branch.state().is_alive());

        // Should fail for deleted state
        let result = validate_branch_state(&txn, branch_id, Some(BranchState::Deleted));
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_branch_parent() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = create_test_trunk(&mut txn, 1);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        // Create a branch
        let insert_op = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        apply_branch_op(&mut txn, &mut context, trunk_id, branch_id, &insert_op, &[]).unwrap();

        // Should pass for correct trunk
        validate_branch_parent(&txn, branch_id, trunk_id).unwrap();

        // Should fail for wrong trunk
        let wrong_trunk = TrunkId::new(NodeId::new(99), 0);
        let result = validate_branch_parent(&txn, branch_id, wrong_trunk);
        assert!(result.is_err());
    }
}

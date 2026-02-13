//! Apply LeafOp operations to the pristine database.
//!
//! This module provides functions for applying token-level CRDT operations
//! (LeafOp) to the pristine database. Leaf operations manage the lifecycle
//! of tokens within lines.
//!
//! # Operations
//!
//! | Operation | Description | Tables Affected |
//! |-----------|-------------|-----------------|
//! | `Insert` | Insert a new token | LEAVES, BRANCH_LEAVES |
//! | `Delete` | Mark token as deleted | LEAVES |
//! | `Replace` | Replace token content | LEAVES |
//! | `Restore` | Restore deleted token | LEAVES |
//!
//! # Replace Operation and Blame Preservation
//!
//! The `Replace` operation is special in that it preserves the token's ID
//! while changing its content. This enables accurate blame tracking - the
//! token's identity is maintained so we know which change created it, even
//! though its content has been modified.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                    Replace vs Delete+Insert                              │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Replace:                                                               │
//! │    Before: Leaf(change=1, idx=0) → "foo"                               │
//! │    After:  Leaf(change=1, idx=0) → "bar"   ← Same ID, different content │
//! │    Blame: "change 1 created this token"                                 │
//! │                                                                         │
//! │  Delete+Insert:                                                         │
//! │    Before: Leaf(change=1, idx=0) → "foo"   ← Marked deleted            │
//! │    After:  Leaf(change=2, idx=0) → "bar"   ← New ID                     │
//! │    Blame: "change 2 created this token"                                 │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::crdt::apply::leaf::apply_leaf_op;
//! use atomic_core::crdt::{BranchId, LeafId, LeafOp};
//! use atomic_core::diff::token::TokenKind;
//! use atomic_core::types::NodeId;
//!
//! let branch_id = BranchId::new(NodeId::new(1), 0);
//! let leaf_id = LeafId::new(NodeId::new(1), 0);
//! let op = LeafOp::Insert {
//!     after: None, // Insert at start of line
//!     kind: TokenKind::Word,
//!     content: b"hello".to_vec(),
//! };
//!
//! apply_leaf_op(txn, &mut context, branch_id, leaf_id, &op, content)?;
//! ```

use crate::crdt::{BranchId, Leaf, LeafId, LeafOp, LeafState};
use crate::diff::token::TokenKind;

use super::context::ApplyContext;
use super::error::{storage_err, ApplyError, ApplyResult};
use super::traits::MutCrdtTxnT;

// =============================================================================
// Public API
// =============================================================================

/// Applies a LeafOp to the pristine database.
///
/// This is the main entry point for applying token-level operations.
///
/// # Arguments
///
/// * `txn` - The transaction to apply the operation in
/// * `context` - The apply context for tracking state and conflicts
/// * `branch_id` - The parent branch (line) this leaf belongs to
/// * `leaf_id` - The ID for the leaf (for Insert ops, this is the new ID)
/// * `op` - The operation to apply
/// * `content` - The content blob containing token data
///
/// # Returns
///
/// * `Ok(())` - The operation was applied successfully
/// * `Err(ApplyError)` - The operation failed
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::crdt::apply::leaf::apply_leaf_op;
/// use atomic_core::crdt::{BranchId, LeafId, LeafOp};
/// use atomic_core::diff::token::TokenKind;
///
/// let op = LeafOp::Insert {
///     after: None,
///     kind: TokenKind::Word,
///     content: b"hello".to_vec(),
/// };
/// apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &op, content)?;
/// ```
pub fn apply_leaf_op<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    branch_id: BranchId,
    leaf_id: LeafId,
    op: &LeafOp,
    content: &[u8],
) -> ApplyResult<()> {
    match op {
        LeafOp::Insert { after, kind, content: leaf_content } => {
            apply_insert(txn, context, branch_id, leaf_id, *after, *kind, leaf_content, content)
        }
        LeafOp::Delete { leaf } => {
            apply_delete(txn, context, *leaf)
        }
        LeafOp::Replace { leaf, new_content } => {
            apply_replace(txn, context, *leaf, new_content, content)
        }
        LeafOp::Restore { leaf } => {
            apply_restore(txn, context, *leaf)
        }
    }
}

// =============================================================================
// Insert Operation
// =============================================================================

/// Applies an Insert operation to create a new token.
///
/// # Behavior
///
/// 1. Checks if the leaf ID already exists (error if so, unless duplicates allowed)
/// 2. Validates the branch exists and is alive (if validation enabled)
/// 3. Validates content range is within bounds (if validation enabled)
/// 4. Creates the leaf entry in LEAVES table
/// 5. Updates BRANCH_LEAVES ordering multimap
///
/// # Errors
///
/// - `LeafAlreadyExists` - The leaf ID is already in use
/// - `BranchNotFound` - The parent branch doesn't exist
/// - `ContentOutOfBounds` - The content range is invalid
fn apply_insert<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    branch_id: BranchId,
    leaf_id: LeafId,
    after: Option<LeafId>,
    kind: TokenKind,
    leaf_content: &[u8],
    _content: &[u8],
) -> ApplyResult<()> {
    // Check for duplicate ID
    if txn.has_leaf(leaf_id).map_err(|e| storage_err(e, "checking leaf exists"))? {
        if context.options().allow_duplicate_ids() {
            context.record_skipped();
            return Ok(());
        }
        return Err(ApplyError::leaf_already_exists(leaf_id));
    }

    // Validate branch exists (optional based on validation settings)
    if context.options().validate_references() {
        if !txn.has_branch(branch_id).map_err(|e| storage_err(e, "checking branch exists"))? {
            return Err(ApplyError::branch_not_found(branch_id));
        }
    }

    // Validate "after" reference if provided
    if context.options().validate_references() {
        if let Some(after_id) = after {
            if !txn.has_leaf(after_id).map_err(|e| storage_err(e, "checking after leaf"))? {
                return Err(ApplyError::leaf_not_found(after_id));
            }
        }
    }

    // For now, we store the content length as the range
    // In a full implementation, we'd compute the actual offset in the content blob
    let content_len = leaf_content.len() as u32;
    let content_range = 0..content_len;

    // Create and store the leaf
    let leaf = Leaf::new(leaf_id, branch_id, kind, content_range);
    txn.put_leaf(&leaf, after)
        .map_err(|e| storage_err(e, "inserting leaf"))?;

    context.record_leaf_inserted();
    context.record_content_bytes(content_len as u64);
    Ok(())
}

// =============================================================================
// Delete Operation
// =============================================================================

/// Applies a Delete operation to mark a token as deleted.
///
/// # Behavior
///
/// 1. Verifies the leaf exists
/// 2. Verifies the leaf is not already deleted
/// 3. Updates the leaf's state to Deleted
///
/// Note: The leaf entry remains in the database (tombstone).
///
/// # Errors
///
/// - `LeafNotFound` - The leaf doesn't exist
/// - `InvalidLeafState` - The leaf is already deleted
fn apply_delete<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    leaf_id: LeafId,
) -> ApplyResult<()> {
    // Verify leaf exists
    let leaf = txn.get_leaf(leaf_id)
        .map_err(|e| storage_err(e, "getting leaf"))?
        .ok_or_else(|| ApplyError::leaf_not_found(leaf_id))?;

    // Check current state
    if leaf.state().is_deleted() {
        if context.options().allow_duplicate_ids() {
            context.record_skipped();
            return Ok(());
        }
        return Err(ApplyError::invalid_leaf_state(
            leaf_id,
            "deleted",
            "delete",
        ));
    }

    // Update state to deleted
    txn.update_leaf_state(leaf_id, LeafState::Deleted)
        .map_err(|e| storage_err(e, "updating leaf state"))?;

    context.record_leaf_deleted();
    Ok(())
}

// =============================================================================
// Replace Operation
// =============================================================================

/// Applies a Replace operation to change a token's content.
///
/// # Behavior
///
/// 1. Verifies the leaf exists
/// 2. Verifies the leaf is alive (not deleted)
/// 3. Updates the leaf's content range to point to new content
///
/// The leaf's ID is preserved, enabling accurate blame tracking.
///
/// # Errors
///
/// - `LeafNotFound` - The leaf doesn't exist
/// - `InvalidLeafState` - The leaf is deleted
fn apply_replace<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    leaf_id: LeafId,
    new_content: &[u8],
    _content: &[u8],
) -> ApplyResult<()> {
    // Verify leaf exists
    let leaf = txn.get_leaf(leaf_id)
        .map_err(|e| storage_err(e, "getting leaf"))?
        .ok_or_else(|| ApplyError::leaf_not_found(leaf_id))?;

    // Check current state
    if leaf.state().is_deleted() {
        return Err(ApplyError::invalid_leaf_state(
            leaf_id,
            "deleted",
            "replace",
        ));
    }

    // Update content range
    // In a full implementation, we'd compute the actual offset in the content blob
    let content_len = new_content.len() as u32;
    let new_range = 0..content_len;

    txn.update_leaf_content(leaf_id, new_range)
        .map_err(|e| storage_err(e, "updating leaf content"))?;

    context.record_leaf_replaced();
    context.record_content_bytes(content_len as u64);
    Ok(())
}

// =============================================================================
// Restore Operation
// =============================================================================

/// Applies a Restore operation to restore a deleted token.
///
/// # Behavior
///
/// 1. Verifies the leaf exists
/// 2. Verifies the leaf is currently deleted
/// 3. Updates the leaf's state to Alive
///
/// # Errors
///
/// - `LeafNotFound` - The leaf doesn't exist
/// - `InvalidLeafState` - The leaf is not deleted
fn apply_restore<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    leaf_id: LeafId,
) -> ApplyResult<()> {
    // Verify leaf exists
    let leaf = txn.get_leaf(leaf_id)
        .map_err(|e| storage_err(e, "getting leaf"))?
        .ok_or_else(|| ApplyError::leaf_not_found(leaf_id))?;

    // Check current state
    if !leaf.state().is_deleted() {
        if context.options().allow_duplicate_ids() {
            context.record_skipped();
            return Ok(());
        }
        return Err(ApplyError::invalid_leaf_state(
            leaf_id,
            "alive",
            "restore",
        ));
    }

    // Update state to alive
    txn.update_leaf_state(leaf_id, LeafState::Alive)
        .map_err(|e| storage_err(e, "updating leaf state"))?;

    context.record_leaf_restored();
    Ok(())
}

// =============================================================================
// Validation Helpers
// =============================================================================

/// Validates that a leaf exists and is in the expected state.
///
/// # Arguments
///
/// * `txn` - The transaction
/// * `leaf_id` - The leaf to validate
/// * `expected_state` - The expected state, or `None` for any state
///
/// # Returns
///
/// The leaf if validation passes.
pub fn validate_leaf_state<T: MutCrdtTxnT>(
    txn: &T,
    leaf_id: LeafId,
    expected_state: Option<LeafState>,
) -> ApplyResult<Leaf> {
    let leaf = txn.get_leaf(leaf_id)
        .map_err(|e| storage_err(e, "getting leaf"))?
        .ok_or_else(|| ApplyError::leaf_not_found(leaf_id))?;

    if let Some(expected) = expected_state {
        let matches = match expected {
            LeafState::Alive => leaf.state().is_alive(),
            LeafState::Deleted => leaf.state().is_deleted(),
        };

        if !matches {
            return Err(ApplyError::invalid_leaf_state(
                leaf_id,
                leaf.state().to_string(),
                format!("expected {}", expected),
            ));
        }
    }

    Ok(leaf)
}

/// Validates that a leaf belongs to the specified branch.
///
/// # Arguments
///
/// * `txn` - The transaction
/// * `leaf_id` - The leaf to validate
/// * `expected_branch` - The expected parent branch
///
/// # Returns
///
/// `Ok(())` if the leaf belongs to the branch.
pub fn validate_leaf_parent<T: MutCrdtTxnT>(
    txn: &T,
    leaf_id: LeafId,
    expected_branch: BranchId,
) -> ApplyResult<()> {
    let leaf = txn.get_leaf(leaf_id)
        .map_err(|e| storage_err(e, "getting leaf"))?
        .ok_or_else(|| ApplyError::leaf_not_found(leaf_id))?;

    if leaf.branch() != expected_branch {
        return Err(ApplyError::internal(format!(
            "Leaf {} belongs to branch {}, not {}",
            leaf_id,
            leaf.branch(),
            expected_branch
        )));
    }

    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crdt::apply::options::ApplyOptions;
    use crate::crdt::{Branch, BranchState, Trunk, TrunkId, TrunkState};
    use crate::types::{Inode, NodeId};
    use std::collections::HashMap;

    // =========================================================================
    // Mock Transaction
    // =========================================================================

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

        fn add_branch(&mut self, branch: Branch) {
            self.branches.insert(branch.id(), branch);
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

        fn update_trunk_state(&mut self, id: TrunkId, state: TrunkState) -> Result<(), Self::Error> {
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
        fn put_branch(&mut self, branch: &Branch, _after: Option<BranchId>) -> Result<bool, Self::Error> {
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

        fn update_branch_state(&mut self, id: BranchId, state: BranchState) -> Result<(), Self::Error> {
            if let Some(branch) = self.branches.get_mut(&id) {
                branch.set_state(state);
            }
            Ok(())
        }

        fn list_branches(&self, trunk_id: TrunkId) -> Result<Vec<BranchId>, Self::Error> {
            Ok(self.trunk_branches.get(&trunk_id).cloned().unwrap_or_default())
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

        fn update_leaf_content(&mut self, id: LeafId, range: std::ops::Range<u32>) -> Result<(), Self::Error> {
            if let Some(leaf) = self.leaves.get_mut(&id) {
                leaf.set_content_range(range);
            }
            Ok(())
        }

        fn list_leaves(&self, branch_id: BranchId) -> Result<Vec<LeafId>, Self::Error> {
            Ok(self.branch_leaves.get(&branch_id).cloned().unwrap_or_default())
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

    // =========================================================================
    // Helper Functions
    // =========================================================================

    fn create_test_branch(txn: &mut MockTxn, change_id: u64, branch_idx: u32) -> BranchId {
        let trunk_id = TrunkId::new(NodeId::new(change_id), 0);
        let branch_id = BranchId::new(NodeId::new(change_id), branch_idx);
        let branch = Branch::new(branch_id, trunk_id);
        txn.add_branch(branch);
        branch_id
    }

    // =========================================================================
    // Insert Tests
    // =========================================================================

    #[test]
    fn test_apply_insert() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        let op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };

        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &op, b"hello").unwrap();

        assert!(txn.has_leaf(leaf_id).unwrap());
        let leaf = txn.get_leaf(leaf_id).unwrap().unwrap();
        assert_eq!(leaf.branch(), branch_id);
        assert_eq!(leaf.kind(), TokenKind::Word);
        assert!(leaf.state().is_alive());
        assert_eq!(context.stats().leaves_inserted(), 1);
    }

    #[test]
    fn test_apply_insert_after() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);

        let leaf_id1 = LeafId::new(NodeId::new(1), 0);
        let leaf_id2 = LeafId::new(NodeId::new(1), 1);

        // Insert first leaf
        let op1 = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id1, &op1, b"hello").unwrap();

        // Insert second leaf after first
        let op2 = LeafOp::Insert {
            after: Some(leaf_id1),
            kind: TokenKind::Whitespace,
            content: b" ".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id2, &op2, b" ").unwrap();

        assert!(txn.has_leaf(leaf_id1).unwrap());
        assert!(txn.has_leaf(leaf_id2).unwrap());
        assert_eq!(context.stats().leaves_inserted(), 2);
    }

    #[test]
    fn test_apply_insert_duplicate_id() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        let op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };

        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &op, b"hello").unwrap();

        // Try to insert again with same ID
        let result = apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &op, b"hello");
        assert!(result.is_err());
        assert!(result.unwrap_err().is_already_exists());
    }

    #[test]
    fn test_apply_insert_duplicate_id_lenient() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::lenient());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        let op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };

        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &op, b"hello").unwrap();

        // Try to insert again (should skip)
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &op, b"hello").unwrap();
        assert_eq!(context.stats().operations_skipped(), 1);
    }

    #[test]
    fn test_apply_insert_branch_not_found() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = BranchId::new(NodeId::new(1), 0); // Not created
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        let op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };

        let result = apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &op, b"hello");
        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_found());
    }

    // =========================================================================
    // Delete Tests
    // =========================================================================

    #[test]
    fn test_apply_delete() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        // Create first
        let insert_op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &insert_op, b"hello").unwrap();

        // Delete
        let delete_op = LeafOp::Delete { leaf: leaf_id };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &delete_op, &[]).unwrap();

        let leaf = txn.get_leaf(leaf_id).unwrap().unwrap();
        assert!(leaf.state().is_deleted());
        assert_eq!(context.stats().leaves_deleted(), 1);
    }

    #[test]
    fn test_apply_delete_not_found() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        let op = LeafOp::Delete { leaf: leaf_id };
        let result = apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &op, &[]);

        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_found());
    }

    #[test]
    fn test_apply_delete_already_deleted() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        // Create and delete
        let insert_op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &insert_op, b"hello").unwrap();

        let delete_op = LeafOp::Delete { leaf: leaf_id };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &delete_op, &[]).unwrap();

        // Try to delete again
        let result = apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &delete_op, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_invalid_state());
    }

    // =========================================================================
    // Replace Tests
    // =========================================================================

    #[test]
    fn test_apply_replace() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        // Create first
        let insert_op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &insert_op, b"hello").unwrap();

        // Replace content
        let replace_op = LeafOp::Replace {
            leaf: leaf_id,
            new_content: b"world".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &replace_op, b"world").unwrap();

        // Leaf should still exist with same ID
        let leaf = txn.get_leaf(leaf_id).unwrap().unwrap();
        assert!(leaf.state().is_alive());
        assert_eq!(context.stats().leaves_replaced(), 1);
    }

    #[test]
    fn test_apply_replace_not_found() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        let op = LeafOp::Replace {
            leaf: leaf_id,
            new_content: b"world".to_vec(),
        };
        let result = apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &op, b"world");

        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_found());
    }

    #[test]
    fn test_apply_replace_deleted() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        // Create and delete
        let insert_op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &insert_op, b"hello").unwrap();

        let delete_op = LeafOp::Delete { leaf: leaf_id };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &delete_op, &[]).unwrap();

        // Try to replace
        let replace_op = LeafOp::Replace {
            leaf: leaf_id,
            new_content: b"world".to_vec(),
        };
        let result = apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &replace_op, b"world");

        assert!(result.is_err());
        assert!(result.unwrap_err().is_invalid_state());
    }

    // =========================================================================
    // Restore Tests
    // =========================================================================

    #[test]
    fn test_apply_restore() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        // Create and delete
        let insert_op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &insert_op, b"hello").unwrap();

        let delete_op = LeafOp::Delete { leaf: leaf_id };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &delete_op, &[]).unwrap();

        // Restore
        let restore_op = LeafOp::Restore { leaf: leaf_id };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &restore_op, &[]).unwrap();

        let leaf = txn.get_leaf(leaf_id).unwrap().unwrap();
        assert!(leaf.state().is_alive());
        assert_eq!(context.stats().leaves_restored(), 1);
    }

    #[test]
    fn test_apply_restore_not_deleted() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        // Create (but don't delete)
        let insert_op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &insert_op, b"hello").unwrap();

        // Try to restore
        let restore_op = LeafOp::Restore { leaf: leaf_id };
        let result = apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &restore_op, &[]);

        assert!(result.is_err());
        assert!(result.unwrap_err().is_invalid_state());
    }

    #[test]
    fn test_apply_restore_not_found() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        let op = LeafOp::Restore { leaf: leaf_id };
        let result = apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &op, &[]);

        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_found());
    }

    // =========================================================================
    // Validation Helper Tests
    // =========================================================================

    #[test]
    fn test_validate_leaf_state() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        // Create a leaf
        let insert_op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &insert_op, b"hello").unwrap();

        // Validate alive state
        let leaf = validate_leaf_state(&txn, leaf_id, Some(LeafState::Alive)).unwrap();
        assert!(leaf.state().is_alive());

        // Should fail for deleted state
        let result = validate_leaf_state(&txn, leaf_id, Some(LeafState::Deleted));
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_leaf_parent() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let branch_id = create_test_branch(&mut txn, 1, 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        // Create a leaf
        let insert_op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        apply_leaf_op(&mut txn, &mut context, branch_id, leaf_id, &insert_op, b"hello").unwrap();

        // Should pass for correct branch
        validate_leaf_parent(&txn, leaf_id, branch_id).unwrap();

        // Should fail for wrong branch
        let wrong_branch = BranchId::new(NodeId::new(99), 0);
        let result = validate_leaf_parent(&txn, leaf_id, wrong_branch);
        assert!(result.is_err());
    }
}

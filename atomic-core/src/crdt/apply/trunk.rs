//! Apply TrunkOp operations to the pristine database.
//!
//! This module provides functions for applying file-level CRDT operations
//! (TrunkOp) to the pristine database. Trunk operations manage the lifecycle
//! of files in the repository.
//!
//! # Operations
//!
//! | Operation | Description | Tables Affected |
//! |-----------|-------------|-----------------|
//! | `Create` | Create a new file | TRUNKS, PATH_TRUNK, INODE_TRUNK |
//! | `Delete` | Mark file as deleted | TRUNKS |
//! | `Move` | Rename/move file | TRUNKS, PATH_TRUNK |
//! | `Undelete` | Restore deleted file | TRUNKS |
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::crdt::apply::trunk::apply_trunk_op;
//! use atomic_core::crdt::{TrunkId, TrunkOp};
//! use atomic_core::change::Encoding;
//! use atomic_core::types::NodeId;
//!
//! let trunk_id = TrunkId::new(NodeId::new(1), 0);
//! let op = TrunkOp::Create {
//!     path: "src/main.rs".to_string(),
//!     encoding: Some(Encoding::Utf8),
//! };
//!
//! apply_trunk_op(txn, &mut context, trunk_id, &op)?;
//! ```

use crate::change::Encoding;
use crate::crdt::{Trunk, TrunkId, TrunkOp, TrunkState};
#[allow(unused_imports)]
use crate::types::Inode;

use super::context::ApplyContext;
use super::error::{storage_err, ApplyError, ApplyResult};
use super::traits::MutCrdtTxnT;

// =============================================================================
// Public API
// =============================================================================

/// Applies a TrunkOp to the pristine database.
///
/// This is the main entry point for applying file-level operations.
///
/// # Arguments
///
/// * `txn` - The transaction to apply the operation in
/// * `context` - The apply context for tracking state and conflicts
/// * `trunk_id` - The ID for the trunk (for Create ops, this is the new ID)
/// * `op` - The operation to apply
///
/// # Returns
///
/// * `Ok(())` - The operation was applied successfully
/// * `Err(ApplyError)` - The operation failed
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::crdt::apply::trunk::apply_trunk_op;
/// use atomic_core::crdt::{TrunkId, TrunkOp};
///
/// let op = TrunkOp::Create {
///     path: "new_file.rs".to_string(),
///     encoding: None,
/// };
/// apply_trunk_op(&mut txn, &mut context, trunk_id, &op)?;
/// ```
pub fn apply_trunk_op<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    trunk_id: TrunkId,
    op: &TrunkOp,
) -> ApplyResult<()> {
    match op {
        TrunkOp::Create { path, encoding } => apply_create(txn, context, trunk_id, path, *encoding),
        TrunkOp::Delete { trunk } => apply_delete(txn, context, *trunk),
        TrunkOp::Move { trunk, new_path } => apply_move(txn, context, *trunk, new_path),
        TrunkOp::Undelete { trunk } => apply_undelete(txn, context, *trunk),
    }
}

// =============================================================================
// Create Operation
// =============================================================================

/// Applies a Create operation to create a new file.
///
/// # Behavior
///
/// 1. Checks if the trunk ID already exists (error if so, unless duplicates allowed)
/// 2. Checks if the path is already taken (error if so)
/// 3. Allocates a new inode
/// 4. Creates the trunk entry in TRUNKS table
/// 5. Updates PATH_TRUNK and INODE_TRUNK indexes
///
/// # Errors
///
/// - `TrunkAlreadyExists` - The trunk ID is already in use
/// - `PathAlreadyExists` - The path is occupied by another file
fn apply_create<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    trunk_id: TrunkId,
    path: &str,
    encoding: Option<Encoding>,
) -> ApplyResult<()> {
    // Check for duplicate ID
    if txn
        .has_trunk(trunk_id)
        .map_err(|e| storage_err(e, "checking trunk exists"))?
    {
        if context.options().allow_duplicate_ids() {
            context.record_skipped();
            return Ok(());
        }
        return Err(ApplyError::trunk_already_exists(trunk_id));
    }

    // Check for path collision
    if let Some(existing) = txn
        .get_trunk_by_path(path)
        .map_err(|e| storage_err(e, "checking path exists"))?
    {
        return Err(ApplyError::path_already_exists(path, existing));
    }

    // Allocate inode and create trunk
    let inode = txn
        .alloc_inode()
        .map_err(|e| storage_err(e, "allocating inode"))?;
    let trunk = Trunk::new(trunk_id, inode, path.to_string(), encoding);

    txn.put_trunk(&trunk)
        .map_err(|e| storage_err(e, "inserting trunk"))?;

    context.record_trunk_created();
    Ok(())
}

// =============================================================================
// Delete Operation
// =============================================================================

/// Applies a Delete operation to mark a file as deleted.
///
/// # Behavior
///
/// 1. Verifies the trunk exists
/// 2. Verifies the trunk is not already deleted
/// 3. Updates the trunk's state to Deleted
///
/// Note: The trunk entry remains in the database (tombstone).
/// Associated branches and leaves are not automatically deleted.
///
/// # Errors
///
/// - `TrunkNotFound` - The trunk doesn't exist
/// - `InvalidTrunkState` - The trunk is already deleted
fn apply_delete<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    trunk_id: TrunkId,
) -> ApplyResult<()> {
    // Verify trunk exists
    let trunk = txn
        .get_trunk(trunk_id)
        .map_err(|e| storage_err(e, "getting trunk"))?
        .ok_or_else(|| ApplyError::trunk_not_found(trunk_id))?;

    // Check current state
    if trunk.state().is_deleted() {
        if context.options().allow_duplicate_ids() {
            context.record_skipped();
            return Ok(());
        }
        return Err(ApplyError::invalid_trunk_state(
            trunk_id, "deleted", "delete",
        ));
    }

    // Update state to deleted
    txn.update_trunk_state(trunk_id, TrunkState::Deleted)
        .map_err(|e| storage_err(e, "updating trunk state"))?;

    context.record_trunk_deleted();
    Ok(())
}

// =============================================================================
// Move Operation
// =============================================================================

/// Applies a Move operation to rename or relocate a file.
///
/// # Behavior
///
/// 1. Verifies the trunk exists
/// 2. Verifies the trunk is alive (not deleted)
/// 3. Checks the new path isn't already taken
/// 4. Updates the trunk's path
/// 5. Updates the PATH_TRUNK index
///
/// # Errors
///
/// - `TrunkNotFound` - The trunk doesn't exist
/// - `InvalidTrunkState` - The trunk is deleted
/// - `PathAlreadyExists` - The new path is occupied
fn apply_move<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    trunk_id: TrunkId,
    new_path: &str,
) -> ApplyResult<()> {
    // Verify trunk exists
    let trunk = txn
        .get_trunk(trunk_id)
        .map_err(|e| storage_err(e, "getting trunk"))?
        .ok_or_else(|| ApplyError::trunk_not_found(trunk_id))?;

    // Check current state
    if trunk.state().is_deleted() {
        return Err(ApplyError::invalid_trunk_state(trunk_id, "deleted", "move"));
    }

    // Check for path collision (unless it's the same trunk)
    if let Some(existing) = txn
        .get_trunk_by_path(new_path)
        .map_err(|e| storage_err(e, "checking path exists"))?
    {
        if existing != trunk_id {
            return Err(ApplyError::path_already_exists(new_path, existing));
        }
        // Moving to same path is a no-op
        context.record_skipped();
        return Ok(());
    }

    // Update path
    txn.update_trunk_path(trunk_id, new_path)
        .map_err(|e| storage_err(e, "updating trunk path"))?;

    context.record_trunk_moved();
    Ok(())
}

// =============================================================================
// Undelete Operation
// =============================================================================

/// Applies an Undelete operation to restore a deleted file.
///
/// # Behavior
///
/// 1. Verifies the trunk exists
/// 2. Verifies the trunk is currently deleted
/// 3. Updates the trunk's state to Alive
///
/// # Errors
///
/// - `TrunkNotFound` - The trunk doesn't exist
/// - `InvalidTrunkState` - The trunk is not deleted
fn apply_undelete<T: MutCrdtTxnT>(
    txn: &mut T,
    context: &mut ApplyContext,
    trunk_id: TrunkId,
) -> ApplyResult<()> {
    // Verify trunk exists
    let trunk = txn
        .get_trunk(trunk_id)
        .map_err(|e| storage_err(e, "getting trunk"))?
        .ok_or_else(|| ApplyError::trunk_not_found(trunk_id))?;

    // Check current state
    if !trunk.state().is_deleted() {
        if context.options().allow_duplicate_ids() {
            context.record_skipped();
            return Ok(());
        }
        return Err(ApplyError::invalid_trunk_state(
            trunk_id, "alive", "undelete",
        ));
    }

    // Update state to alive
    txn.update_trunk_state(trunk_id, TrunkState::Alive)
        .map_err(|e| storage_err(e, "updating trunk state"))?;

    context.record_trunk_undeleted();
    Ok(())
}

// =============================================================================
// Validation Helpers
// =============================================================================

/// Validates that a trunk exists and is in the expected state.
///
/// # Arguments
///
/// * `txn` - The transaction
/// * `trunk_id` - The trunk to validate
/// * `expected_state` - The expected state, or `None` for any state
///
/// # Returns
///
/// The trunk if validation passes.
pub fn validate_trunk_state<T: MutCrdtTxnT>(
    txn: &T,
    trunk_id: TrunkId,
    expected_state: Option<TrunkState>,
) -> ApplyResult<Trunk> {
    let trunk = txn
        .get_trunk(trunk_id)
        .map_err(|e| storage_err(e, "getting trunk"))?
        .ok_or_else(|| ApplyError::trunk_not_found(trunk_id))?;

    if let Some(expected) = expected_state {
        let matches = match expected {
            TrunkState::Alive => trunk.state().is_alive(),
            TrunkState::Deleted => trunk.state().is_deleted(),
            TrunkState::Zombie => trunk.state().is_zombie(),
        };

        if !matches {
            return Err(ApplyError::invalid_trunk_state(
                trunk_id,
                trunk.state().to_string(),
                format!("expected {}", expected),
            ));
        }
    }

    Ok(trunk)
}

/// Checks if a path is available (not taken by another file).
///
/// # Arguments
///
/// * `txn` - The transaction
/// * `path` - The path to check
/// * `exclude` - Optionally exclude this trunk from the check
///
/// # Returns
///
/// `Ok(())` if the path is available, or an error if it's taken.
pub fn validate_path_available<T: MutCrdtTxnT>(
    txn: &T,
    path: &str,
    exclude: Option<TrunkId>,
) -> ApplyResult<()> {
    if let Some(existing) = txn
        .get_trunk_by_path(path)
        .map_err(|e| storage_err(e, "checking path"))?
    {
        if exclude != Some(existing) {
            return Err(ApplyError::path_already_exists(path, existing));
        }
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
    use crate::types::NodeId;
    use std::collections::HashMap;

    // =========================================================================
    // Mock Transaction
    // =========================================================================

    #[derive(Default)]
    struct MockTxn {
        trunks: HashMap<TrunkId, Trunk>,
        path_index: HashMap<String, TrunkId>,
        inode_index: HashMap<Inode, TrunkId>,
        next_inode: u64,
    }

    impl MockTxn {
        fn new() -> Self {
            Self {
                next_inode: 1,
                ..Default::default()
            }
        }
    }

    impl MutCrdtTxnT for MockTxn {
        type Error = crate::pristine::PristineError;

        fn put_trunk(&mut self, trunk: &Trunk) -> Result<bool, Self::Error> {
            let is_new = !self.trunks.contains_key(&trunk.id());
            self.trunks.insert(trunk.id(), trunk.clone());
            self.path_index.insert(trunk.path().to_string(), trunk.id());
            self.inode_index.insert(trunk.inode(), trunk.id());
            Ok(is_new)
        }

        fn get_trunk(&self, id: TrunkId) -> Result<Option<Trunk>, Self::Error> {
            Ok(self.trunks.get(&id).cloned())
        }

        fn has_trunk(&self, id: TrunkId) -> Result<bool, Self::Error> {
            Ok(self.trunks.contains_key(&id))
        }

        fn del_trunk(&mut self, id: TrunkId) -> Result<bool, Self::Error> {
            if let Some(trunk) = self.trunks.remove(&id) {
                self.path_index.remove(trunk.path());
                self.inode_index.remove(&trunk.inode());
                Ok(true)
            } else {
                Ok(false)
            }
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

        fn update_trunk_path(&mut self, id: TrunkId, new_path: &str) -> Result<(), Self::Error> {
            if let Some(trunk) = self.trunks.get_mut(&id) {
                self.path_index.remove(trunk.path());
                trunk.set_path(new_path.to_string());
                self.path_index.insert(new_path.to_string(), id);
            }
            Ok(())
        }

        fn get_trunk_by_path(&self, path: &str) -> Result<Option<TrunkId>, Self::Error> {
            Ok(self.path_index.get(path).copied())
        }

        fn get_trunk_by_inode(&self, inode: Inode) -> Result<Option<TrunkId>, Self::Error> {
            Ok(self.inode_index.get(&inode).copied())
        }

        // Branch methods (not used in trunk tests)
        fn put_branch(
            &mut self,
            _: &crate::crdt::Branch,
            _: Option<crate::crdt::BranchId>,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
        fn get_branch(
            &self,
            _: crate::crdt::BranchId,
        ) -> Result<Option<crate::crdt::Branch>, Self::Error> {
            Ok(None)
        }
        fn has_branch(&self, _: crate::crdt::BranchId) -> Result<bool, Self::Error> {
            Ok(false)
        }
        fn del_branch(&mut self, _: crate::crdt::BranchId) -> Result<bool, Self::Error> {
            Ok(false)
        }
        fn update_branch_state(
            &mut self,
            _: crate::crdt::BranchId,
            _: crate::crdt::BranchState,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
        fn list_branches(&self, _: TrunkId) -> Result<Vec<crate::crdt::BranchId>, Self::Error> {
            Ok(vec![])
        }
        fn count_branches(&self, _: TrunkId) -> Result<usize, Self::Error> {
            Ok(0)
        }

        // Leaf methods (not used in trunk tests)
        fn put_leaf(
            &mut self,
            _: &crate::crdt::Leaf,
            _: Option<crate::crdt::LeafId>,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
        fn get_leaf(
            &self,
            _: crate::crdt::LeafId,
        ) -> Result<Option<crate::crdt::Leaf>, Self::Error> {
            Ok(None)
        }
        fn has_leaf(&self, _: crate::crdt::LeafId) -> Result<bool, Self::Error> {
            Ok(false)
        }
        fn del_leaf(&mut self, _: crate::crdt::LeafId) -> Result<bool, Self::Error> {
            Ok(false)
        }
        fn update_leaf_state(
            &mut self,
            _: crate::crdt::LeafId,
            _: crate::crdt::LeafState,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
        fn update_leaf_content(
            &mut self,
            _: crate::crdt::LeafId,
            _: std::ops::Range<u32>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
        fn list_leaves(
            &self,
            _: crate::crdt::BranchId,
        ) -> Result<Vec<crate::crdt::LeafId>, Self::Error> {
            Ok(vec![])
        }
        fn count_leaves(&self, _: crate::crdt::BranchId) -> Result<usize, Self::Error> {
            Ok(0)
        }

        fn alloc_inode(&mut self) -> Result<Inode, Self::Error> {
            let inode = Inode::new(self.next_inode);
            self.next_inode += 1;
            Ok(inode)
        }
    }

    // =========================================================================
    // Create Tests
    // =========================================================================

    #[test]
    fn test_apply_create() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        let op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: Some(Encoding::Utf8),
        };

        apply_trunk_op(&mut txn, &mut context, trunk_id, &op).unwrap();

        assert!(txn.has_trunk(trunk_id).unwrap());
        let trunk = txn.get_trunk(trunk_id).unwrap().unwrap();
        assert_eq!(trunk.path(), "test.rs");
        assert_eq!(trunk.encoding(), Some(Encoding::Utf8));
        assert_eq!(context.stats().trunks_created(), 1);
    }

    #[test]
    fn test_apply_create_duplicate_id() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        let op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };

        apply_trunk_op(&mut txn, &mut context, trunk_id, &op).unwrap();

        // Try to create again with same ID
        let op2 = TrunkOp::Create {
            path: "other.rs".to_string(),
            encoding: None,
        };

        let result = apply_trunk_op(&mut txn, &mut context, trunk_id, &op2);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_already_exists());
    }

    #[test]
    fn test_apply_create_duplicate_id_lenient() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::lenient());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        let op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };

        apply_trunk_op(&mut txn, &mut context, trunk_id, &op).unwrap();

        // Try to create again with same ID (should skip)
        let op2 = TrunkOp::Create {
            path: "other.rs".to_string(),
            encoding: None,
        };

        apply_trunk_op(&mut txn, &mut context, trunk_id, &op2).unwrap();
        assert_eq!(context.stats().operations_skipped(), 1);
    }

    #[test]
    fn test_apply_create_path_collision() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());

        let trunk_id1 = TrunkId::new(NodeId::new(1), 0);
        let trunk_id2 = TrunkId::new(NodeId::new(2), 0);

        let op1 = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id1, &op1).unwrap();

        // Try to create another file at same path
        let op2 = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };

        let result = apply_trunk_op(&mut txn, &mut context, trunk_id2, &op2);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_already_exists());
    }

    // =========================================================================
    // Delete Tests
    // =========================================================================

    #[test]
    fn test_apply_delete() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // Create first
        let create_op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &create_op).unwrap();

        // Delete
        let delete_op = TrunkOp::Delete { trunk: trunk_id };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &delete_op).unwrap();

        let trunk = txn.get_trunk(trunk_id).unwrap().unwrap();
        assert!(trunk.state().is_deleted());
        assert_eq!(context.stats().trunks_deleted(), 1);
    }

    #[test]
    fn test_apply_delete_not_found() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        let op = TrunkOp::Delete { trunk: trunk_id };
        let result = apply_trunk_op(&mut txn, &mut context, trunk_id, &op);

        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_found());
    }

    #[test]
    fn test_apply_delete_already_deleted() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // Create and delete
        let create_op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &create_op).unwrap();

        let delete_op = TrunkOp::Delete { trunk: trunk_id };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &delete_op).unwrap();

        // Try to delete again
        let result = apply_trunk_op(&mut txn, &mut context, trunk_id, &delete_op);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_invalid_state());
    }

    // =========================================================================
    // Move Tests
    // =========================================================================

    #[test]
    fn test_apply_move() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // Create
        let create_op = TrunkOp::Create {
            path: "old.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &create_op).unwrap();

        // Move
        let move_op = TrunkOp::Move {
            trunk: trunk_id,
            new_path: "new.rs".to_string(),
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &move_op).unwrap();

        let trunk = txn.get_trunk(trunk_id).unwrap().unwrap();
        assert_eq!(trunk.path(), "new.rs");
        assert!(txn.get_trunk_by_path("old.rs").unwrap().is_none());
        assert_eq!(txn.get_trunk_by_path("new.rs").unwrap(), Some(trunk_id));
        assert_eq!(context.stats().trunks_moved(), 1);
    }

    #[test]
    fn test_apply_move_deleted() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // Create and delete
        let create_op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &create_op).unwrap();

        let delete_op = TrunkOp::Delete { trunk: trunk_id };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &delete_op).unwrap();

        // Try to move
        let move_op = TrunkOp::Move {
            trunk: trunk_id,
            new_path: "new.rs".to_string(),
        };
        let result = apply_trunk_op(&mut txn, &mut context, trunk_id, &move_op);

        assert!(result.is_err());
        assert!(result.unwrap_err().is_invalid_state());
    }

    #[test]
    fn test_apply_move_path_collision() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());

        let trunk_id1 = TrunkId::new(NodeId::new(1), 0);
        let trunk_id2 = TrunkId::new(NodeId::new(2), 0);

        // Create two files
        let create1 = TrunkOp::Create {
            path: "file1.rs".to_string(),
            encoding: None,
        };
        let create2 = TrunkOp::Create {
            path: "file2.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id1, &create1).unwrap();
        apply_trunk_op(&mut txn, &mut context, trunk_id2, &create2).unwrap();

        // Try to move file1 to file2's path
        let move_op = TrunkOp::Move {
            trunk: trunk_id1,
            new_path: "file2.rs".to_string(),
        };
        let result = apply_trunk_op(&mut txn, &mut context, trunk_id1, &move_op);

        assert!(result.is_err());
        assert!(result.unwrap_err().is_already_exists());
    }

    #[test]
    fn test_apply_move_same_path() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // Create
        let create_op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &create_op).unwrap();

        // Move to same path (no-op)
        let move_op = TrunkOp::Move {
            trunk: trunk_id,
            new_path: "test.rs".to_string(),
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &move_op).unwrap();

        assert_eq!(context.stats().operations_skipped(), 1);
        assert_eq!(context.stats().trunks_moved(), 0);
    }

    // =========================================================================
    // Undelete Tests
    // =========================================================================

    #[test]
    fn test_apply_undelete() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // Create and delete
        let create_op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &create_op).unwrap();

        let delete_op = TrunkOp::Delete { trunk: trunk_id };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &delete_op).unwrap();

        // Undelete
        let undelete_op = TrunkOp::Undelete { trunk: trunk_id };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &undelete_op).unwrap();

        let trunk = txn.get_trunk(trunk_id).unwrap().unwrap();
        assert!(trunk.state().is_alive());
        assert_eq!(context.stats().trunks_undeleted(), 1);
    }

    #[test]
    fn test_apply_undelete_not_deleted() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // Create (but don't delete)
        let create_op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &create_op).unwrap();

        // Try to undelete
        let undelete_op = TrunkOp::Undelete { trunk: trunk_id };
        let result = apply_trunk_op(&mut txn, &mut context, trunk_id, &undelete_op);

        assert!(result.is_err());
        assert!(result.unwrap_err().is_invalid_state());
    }

    #[test]
    fn test_apply_undelete_not_found() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        let op = TrunkOp::Undelete { trunk: trunk_id };
        let result = apply_trunk_op(&mut txn, &mut context, trunk_id, &op);

        assert!(result.is_err());
        assert!(result.unwrap_err().is_not_found());
    }

    // =========================================================================
    // Validation Helper Tests
    // =========================================================================

    #[test]
    fn test_validate_trunk_state() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // Create a trunk
        let create_op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &create_op).unwrap();

        // Validate alive state
        let trunk = validate_trunk_state(&txn, trunk_id, Some(TrunkState::Alive)).unwrap();
        assert!(trunk.state().is_alive());

        // Should fail for deleted state
        let result = validate_trunk_state(&txn, trunk_id, Some(TrunkState::Deleted));
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_path_available() {
        let mut txn = MockTxn::new();
        let mut context = ApplyContext::new(ApplyOptions::default());
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // Path is available before creating
        validate_path_available(&txn, "test.rs", None).unwrap();

        // Create a trunk
        let create_op = TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        };
        apply_trunk_op(&mut txn, &mut context, trunk_id, &create_op).unwrap();

        // Path is now taken
        let result = validate_path_available(&txn, "test.rs", None);
        assert!(result.is_err());

        // But available if we exclude the owner
        validate_path_available(&txn, "test.rs", Some(trunk_id)).unwrap();
    }
}

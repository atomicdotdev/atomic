//! Transaction traits for CRDT apply operations.
//!
//! This module defines the [`MutCrdtTxnT`] trait which extends the base
//! transaction traits with CRDT-specific operations for applying TrunkOp,
//! BranchOp, and LeafOp operations to the pristine database.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      Transaction Trait Hierarchy                         │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │                         MutTxnT                                         │
//! │                    (base write traits)                                  │
//! │                           │                                             │
//! │                           │ extends                                     │
//! │                           ▼                                             │
//! │                      MutCrdtTxnT                                        │
//! │                    (CRDT operations)                                    │
//! │                           │                                             │
//! │           ┌───────────────┼───────────────┐                            │
//! │           ▼               ▼               ▼                            │
//! │    Trunk Methods   Branch Methods   Leaf Methods                       │
//! │    ─────────────   ──────────────   ────────────                       │
//! │    put_trunk       put_branch       put_leaf                           │
//! │    get_trunk       get_branch       get_leaf                           │
//! │    del_trunk       del_branch       del_leaf                           │
//! │    update_trunk    update_branch    update_leaf                        │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Design Principles
//!
//! 1. **Separation of Concerns**: CRDT operations are separate from base
//!    graph operations, allowing independent evolution.
//!
//! 2. **Consistency**: All CRDT operations maintain table consistency by
//!    updating both primary tables and indexes.
//!
//! 3. **Atomicity**: Operations within a transaction are atomic; either
//!    all succeed or none do.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::crdt::apply::traits::MutCrdtTxnT;
//! use atomic_core::crdt::{Trunk, TrunkId, TrunkState};
//! use atomic_core::change::Encoding;
//! use atomic_core::types::{Inode, NodeId};
//!
//! fn create_file<T: MutCrdtTxnT>(
//!     txn: &mut T,
//!     trunk_id: TrunkId,
//!     path: &str,
//!     encoding: Option<Encoding>,
//! ) -> Result<(), T::Error> {
//!     let inode = txn.alloc_inode()?;
//!     let trunk = Trunk::new(trunk_id, inode, path.to_string(), encoding);
//!     txn.put_trunk(&trunk)?;
//!     Ok(())
//! }
//! ```

use crate::change::Encoding;
use crate::crdt::{
    Branch, BranchId, BranchState, Leaf, LeafId, LeafState, Trunk, TrunkId, TrunkState,
};
use crate::diff::token::TokenKind;
use crate::pristine::PristineError;
use crate::types::Inode;
use std::ops::Range;

// =============================================================================
// MutCrdtTxnT Trait
// =============================================================================

/// Transaction trait for CRDT apply operations.
///
/// Extends the base transaction traits with methods for manipulating
/// the hierarchical CRDT tables (TRUNKS, BRANCHES, LEAVES) and their
/// associated indexes.
///
/// # Implementors
///
/// This trait should be implemented by write transaction types that
/// support CRDT operations. The implementation should ensure:
///
/// 1. All table updates are atomic within the transaction
/// 2. Indexes (PATH_TRUNK, INODE_TRUNK, etc.) are kept consistent
/// 3. Ordering multimaps (TRUNK_BRANCHES, BRANCH_LEAVES) are maintained
///
/// # Table Responsibilities
///
/// | Method | Primary Table | Indexes Updated |
/// |--------|--------------|-----------------|
/// | `put_trunk` | TRUNKS | PATH_TRUNK, INODE_TRUNK |
/// | `put_branch` | BRANCHES | TRUNK_BRANCHES |
/// | `put_leaf` | LEAVES | BRANCH_LEAVES |
pub trait MutCrdtTxnT {
    /// The error type for CRDT operations.
    type Error: Into<PristineError>;

    // =========================================================================
    // Trunk (File) Operations
    // =========================================================================

    /// Stores a trunk in the TRUNKS table.
    ///
    /// Also updates the PATH_TRUNK and INODE_TRUNK reverse indexes.
    ///
    /// # Arguments
    ///
    /// * `trunk` - The trunk to store
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The trunk was newly inserted
    /// * `Ok(false)` - An existing trunk was updated
    /// * `Err(_)` - Database error
    fn put_trunk(&mut self, trunk: &Trunk) -> Result<bool, Self::Error>;

    /// Retrieves a trunk by ID.
    ///
    /// # Arguments
    ///
    /// * `id` - The trunk ID to look up
    ///
    /// # Returns
    ///
    /// The trunk if found, or `None` if not present.
    fn get_trunk(&self, id: TrunkId) -> Result<Option<Trunk>, Self::Error>;

    /// Checks if a trunk exists.
    ///
    /// # Arguments
    ///
    /// * `id` - The trunk ID to check
    fn has_trunk(&self, id: TrunkId) -> Result<bool, Self::Error>;

    /// Deletes a trunk from the TRUNKS table.
    ///
    /// Also removes entries from PATH_TRUNK and INODE_TRUNK indexes.
    /// Does not delete associated branches and leaves.
    ///
    /// # Arguments
    ///
    /// * `id` - The trunk ID to delete
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The trunk was deleted
    /// * `Ok(false)` - The trunk didn't exist
    fn del_trunk(&mut self, id: TrunkId) -> Result<bool, Self::Error>;

    /// Updates a trunk's state.
    ///
    /// # Arguments
    ///
    /// * `id` - The trunk ID to update
    /// * `state` - The new state
    fn update_trunk_state(&mut self, id: TrunkId, state: TrunkState) -> Result<(), Self::Error>;

    /// Updates a trunk's path.
    ///
    /// Also updates the PATH_TRUNK index.
    ///
    /// # Arguments
    ///
    /// * `id` - The trunk ID to update
    /// * `new_path` - The new file path
    fn update_trunk_path(&mut self, id: TrunkId, new_path: &str) -> Result<(), Self::Error>;

    /// Looks up a trunk by file path.
    ///
    /// Uses the PATH_TRUNK index.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path to look up
    fn get_trunk_by_path(&self, path: &str) -> Result<Option<TrunkId>, Self::Error>;

    /// Looks up a trunk by inode.
    ///
    /// Uses the INODE_TRUNK index.
    ///
    /// # Arguments
    ///
    /// * `inode` - The inode to look up
    fn get_trunk_by_inode(&self, inode: Inode) -> Result<Option<TrunkId>, Self::Error>;

    // =========================================================================
    // Branch (Line) Operations
    // =========================================================================

    /// Stores a branch in the BRANCHES table.
    ///
    /// Also updates the TRUNK_BRANCHES ordering multimap.
    ///
    /// # Arguments
    ///
    /// * `branch` - The branch to store
    /// * `after` - The branch this one should be inserted after, or `None` for start
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The branch was newly inserted
    /// * `Ok(false)` - An existing branch was updated
    fn put_branch(&mut self, branch: &Branch, after: Option<BranchId>) -> Result<bool, Self::Error>;

    /// Retrieves a branch by ID.
    ///
    /// # Arguments
    ///
    /// * `id` - The branch ID to look up
    fn get_branch(&self, id: BranchId) -> Result<Option<Branch>, Self::Error>;

    /// Checks if a branch exists.
    ///
    /// # Arguments
    ///
    /// * `id` - The branch ID to check
    fn has_branch(&self, id: BranchId) -> Result<bool, Self::Error>;

    /// Deletes a branch from the BRANCHES table.
    ///
    /// Also removes the entry from TRUNK_BRANCHES.
    /// Does not delete associated leaves.
    ///
    /// # Arguments
    ///
    /// * `id` - The branch ID to delete
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The branch was deleted
    /// * `Ok(false)` - The branch didn't exist
    fn del_branch(&mut self, id: BranchId) -> Result<bool, Self::Error>;

    /// Updates a branch's state.
    ///
    /// # Arguments
    ///
    /// * `id` - The branch ID to update
    /// * `state` - The new state
    fn update_branch_state(&mut self, id: BranchId, state: BranchState) -> Result<(), Self::Error>;

    /// Lists all branch IDs for a trunk in order.
    ///
    /// Uses the TRUNK_BRANCHES multimap.
    ///
    /// # Arguments
    ///
    /// * `trunk_id` - The trunk to list branches for
    fn list_branches(&self, trunk_id: TrunkId) -> Result<Vec<BranchId>, Self::Error>;

    /// Counts the branches in a trunk.
    ///
    /// # Arguments
    ///
    /// * `trunk_id` - The trunk to count branches for
    fn count_branches(&self, trunk_id: TrunkId) -> Result<usize, Self::Error>;

    // =========================================================================
    // Leaf (Token) Operations
    // =========================================================================

    /// Stores a leaf in the LEAVES table.
    ///
    /// Also updates the BRANCH_LEAVES ordering multimap.
    ///
    /// # Arguments
    ///
    /// * `leaf` - The leaf to store
    /// * `after` - The leaf this one should be inserted after, or `None` for start
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The leaf was newly inserted
    /// * `Ok(false)` - An existing leaf was updated
    fn put_leaf(&mut self, leaf: &Leaf, after: Option<LeafId>) -> Result<bool, Self::Error>;

    /// Retrieves a leaf by ID.
    ///
    /// # Arguments
    ///
    /// * `id` - The leaf ID to look up
    fn get_leaf(&self, id: LeafId) -> Result<Option<Leaf>, Self::Error>;

    /// Checks if a leaf exists.
    ///
    /// # Arguments
    ///
    /// * `id` - The leaf ID to check
    fn has_leaf(&self, id: LeafId) -> Result<bool, Self::Error>;

    /// Deletes a leaf from the LEAVES table.
    ///
    /// Also removes the entry from BRANCH_LEAVES.
    ///
    /// # Arguments
    ///
    /// * `id` - The leaf ID to delete
    ///
    /// # Returns
    ///
    /// * `Ok(true)` - The leaf was deleted
    /// * `Ok(false)` - The leaf didn't exist
    fn del_leaf(&mut self, id: LeafId) -> Result<bool, Self::Error>;

    /// Updates a leaf's state.
    ///
    /// # Arguments
    ///
    /// * `id` - The leaf ID to update
    /// * `state` - The new state
    fn update_leaf_state(&mut self, id: LeafId, state: LeafState) -> Result<(), Self::Error>;

    /// Updates a leaf's content range.
    ///
    /// Used for Replace operations.
    ///
    /// # Arguments
    ///
    /// * `id` - The leaf ID to update
    /// * `range` - The new content range
    fn update_leaf_content(&mut self, id: LeafId, range: Range<u32>) -> Result<(), Self::Error>;

    /// Lists all leaf IDs for a branch in order.
    ///
    /// Uses the BRANCH_LEAVES multimap.
    ///
    /// # Arguments
    ///
    /// * `branch_id` - The branch to list leaves for
    fn list_leaves(&self, branch_id: BranchId) -> Result<Vec<LeafId>, Self::Error>;

    /// Counts the leaves in a branch.
    ///
    /// # Arguments
    ///
    /// * `branch_id` - The branch to count leaves for
    fn count_leaves(&self, branch_id: BranchId) -> Result<usize, Self::Error>;

    // =========================================================================
    // Utility Operations
    // =========================================================================

    /// Allocates a new unique inode.
    ///
    /// Inodes are stable identifiers that survive file renames.
    fn alloc_inode(&mut self) -> Result<Inode, Self::Error>;
}

// =============================================================================
// Extension Methods
// =============================================================================

/// Extension trait providing higher-level CRDT operations.
///
/// Built on top of [`MutCrdtTxnT`] to provide common operation patterns.
pub trait MutCrdtTxnExt: MutCrdtTxnT {
    /// Creates a new trunk (file).
    ///
    /// Allocates an inode and stores the trunk.
    ///
    /// # Arguments
    ///
    /// * `id` - The trunk ID
    /// * `path` - The file path
    /// * `encoding` - Optional text encoding
    fn create_trunk(
        &mut self,
        id: TrunkId,
        path: &str,
        encoding: Option<Encoding>,
    ) -> Result<Trunk, Self::Error> {
        let inode = self.alloc_inode()?;
        let trunk = Trunk::new(id, inode, path.to_string(), encoding);
        self.put_trunk(&trunk)?;
        Ok(trunk)
    }

    /// Marks a trunk as deleted.
    ///
    /// Does not remove the trunk, just updates its state.
    fn mark_trunk_deleted(&mut self, id: TrunkId) -> Result<(), Self::Error> {
        self.update_trunk_state(id, TrunkState::Deleted)
    }

    /// Marks a trunk as alive (undeletes).
    fn mark_trunk_alive(&mut self, id: TrunkId) -> Result<(), Self::Error> {
        self.update_trunk_state(id, TrunkState::Alive)
    }

    /// Creates a new branch (line) in a trunk.
    ///
    /// # Arguments
    ///
    /// * `id` - The branch ID
    /// * `trunk_id` - The parent trunk
    /// * `after` - Insert after this branch, or `None` for start
    fn create_branch(
        &mut self,
        id: BranchId,
        trunk_id: TrunkId,
        after: Option<BranchId>,
    ) -> Result<Branch, Self::Error> {
        let branch = Branch::new(id, trunk_id);
        self.put_branch(&branch, after)?;
        Ok(branch)
    }

    /// Marks a branch as deleted.
    fn mark_branch_deleted(&mut self, id: BranchId) -> Result<(), Self::Error> {
        self.update_branch_state(id, BranchState::Deleted)
    }

    /// Marks a branch as alive (restores).
    fn mark_branch_alive(&mut self, id: BranchId) -> Result<(), Self::Error> {
        self.update_branch_state(id, BranchState::Alive)
    }

    /// Creates a new leaf (token) in a branch.
    ///
    /// # Arguments
    ///
    /// * `id` - The leaf ID
    /// * `branch_id` - The parent branch
    /// * `kind` - The token kind
    /// * `content_range` - Byte range in the content blob
    /// * `after` - Insert after this leaf, or `None` for start
    fn create_leaf(
        &mut self,
        id: LeafId,
        branch_id: BranchId,
        kind: TokenKind,
        content_range: Range<u32>,
        after: Option<LeafId>,
    ) -> Result<Leaf, Self::Error> {
        let leaf = Leaf::new(id, branch_id, kind, content_range);
        self.put_leaf(&leaf, after)?;
        Ok(leaf)
    }

    /// Marks a leaf as deleted.
    fn mark_leaf_deleted(&mut self, id: LeafId) -> Result<(), Self::Error> {
        self.update_leaf_state(id, LeafState::Deleted)
    }

    /// Marks a leaf as alive (restores).
    fn mark_leaf_alive(&mut self, id: LeafId) -> Result<(), Self::Error> {
        self.update_leaf_state(id, LeafState::Alive)
    }
}

// Blanket implementation for all MutCrdtTxnT implementors
impl<T: MutCrdtTxnT> MutCrdtTxnExt for T {}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;
    use std::collections::HashMap;

    // =========================================================================
    // Mock Transaction for Testing
    // =========================================================================

    /// A mock transaction implementation for testing.
    #[derive(Default)]
    struct MockCrdtTxn {
        trunks: HashMap<TrunkId, Trunk>,
        branches: HashMap<BranchId, Branch>,
        leaves: HashMap<LeafId, Leaf>,
        path_index: HashMap<String, TrunkId>,
        inode_index: HashMap<Inode, TrunkId>,
        trunk_branches: HashMap<TrunkId, Vec<BranchId>>,
        branch_leaves: HashMap<BranchId, Vec<LeafId>>,
        next_inode: u64,
    }

    impl MockCrdtTxn {
        fn new() -> Self {
            Self {
                next_inode: 1,
                ..Default::default()
            }
        }
    }

    impl MutCrdtTxnT for MockCrdtTxn {
        type Error = PristineError;

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

        fn update_trunk_state(&mut self, id: TrunkId, state: TrunkState) -> Result<(), Self::Error> {
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

        fn update_leaf_content(&mut self, id: LeafId, range: Range<u32>) -> Result<(), Self::Error> {
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
    // Trunk Tests
    // =========================================================================

    #[test]
    fn test_put_and_get_trunk() {
        let mut txn = MockCrdtTxn::new();
        let id = TrunkId::new(NodeId::new(1), 0);
        let trunk = Trunk::new(id, Inode::new(1), "test.rs".to_string(), None);

        assert!(txn.put_trunk(&trunk).unwrap());
        let retrieved = txn.get_trunk(id).unwrap().unwrap();
        assert_eq!(retrieved.path(), "test.rs");
    }

    #[test]
    fn test_has_trunk() {
        let mut txn = MockCrdtTxn::new();
        let id = TrunkId::new(NodeId::new(1), 0);

        assert!(!txn.has_trunk(id).unwrap());

        let trunk = Trunk::new(id, Inode::new(1), "test.rs".to_string(), None);
        txn.put_trunk(&trunk).unwrap();

        assert!(txn.has_trunk(id).unwrap());
    }

    #[test]
    fn test_del_trunk() {
        let mut txn = MockCrdtTxn::new();
        let id = TrunkId::new(NodeId::new(1), 0);
        let trunk = Trunk::new(id, Inode::new(1), "test.rs".to_string(), None);

        txn.put_trunk(&trunk).unwrap();
        assert!(txn.del_trunk(id).unwrap());
        assert!(!txn.has_trunk(id).unwrap());
    }

    #[test]
    fn test_get_trunk_by_path() {
        let mut txn = MockCrdtTxn::new();
        let id = TrunkId::new(NodeId::new(1), 0);
        let trunk = Trunk::new(id, Inode::new(1), "src/main.rs".to_string(), None);

        txn.put_trunk(&trunk).unwrap();
        let found = txn.get_trunk_by_path("src/main.rs").unwrap();
        assert_eq!(found, Some(id));
    }

    #[test]
    fn test_get_trunk_by_inode() {
        let mut txn = MockCrdtTxn::new();
        let id = TrunkId::new(NodeId::new(1), 0);
        let inode = Inode::new(42);
        let trunk = Trunk::new(id, inode, "test.rs".to_string(), None);

        txn.put_trunk(&trunk).unwrap();
        let found = txn.get_trunk_by_inode(inode).unwrap();
        assert_eq!(found, Some(id));
    }

    #[test]
    fn test_update_trunk_path() {
        let mut txn = MockCrdtTxn::new();
        let id = TrunkId::new(NodeId::new(1), 0);
        let trunk = Trunk::new(id, Inode::new(1), "old.rs".to_string(), None);

        txn.put_trunk(&trunk).unwrap();
        txn.update_trunk_path(id, "new.rs").unwrap();

        let retrieved = txn.get_trunk(id).unwrap().unwrap();
        assert_eq!(retrieved.path(), "new.rs");

        // Old path should not work
        assert!(txn.get_trunk_by_path("old.rs").unwrap().is_none());
        // New path should work
        assert_eq!(txn.get_trunk_by_path("new.rs").unwrap(), Some(id));
    }

    // =========================================================================
    // Branch Tests
    // =========================================================================

    #[test]
    fn test_put_and_get_branch() {
        let mut txn = MockCrdtTxn::new();
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let branch = Branch::new(branch_id, trunk_id);

        assert!(txn.put_branch(&branch, None).unwrap());
        let retrieved = txn.get_branch(branch_id).unwrap().unwrap();
        assert_eq!(retrieved.trunk(), trunk_id);
    }

    #[test]
    fn test_list_branches() {
        let mut txn = MockCrdtTxn::new();
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        let b1 = BranchId::new(NodeId::new(1), 0);
        let b2 = BranchId::new(NodeId::new(1), 1);

        txn.put_branch(&Branch::new(b1, trunk_id), None).unwrap();
        txn.put_branch(&Branch::new(b2, trunk_id), Some(b1)).unwrap();

        let branches = txn.list_branches(trunk_id).unwrap();
        assert_eq!(branches.len(), 2);
    }

    #[test]
    fn test_count_branches() {
        let mut txn = MockCrdtTxn::new();
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        assert_eq!(txn.count_branches(trunk_id).unwrap(), 0);

        let b1 = BranchId::new(NodeId::new(1), 0);
        txn.put_branch(&Branch::new(b1, trunk_id), None).unwrap();

        assert_eq!(txn.count_branches(trunk_id).unwrap(), 1);
    }

    // =========================================================================
    // Leaf Tests
    // =========================================================================

    #[test]
    fn test_put_and_get_leaf() {
        let mut txn = MockCrdtTxn::new();
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let leaf = Leaf::new(leaf_id, branch_id, TokenKind::Word, 0..5);

        assert!(txn.put_leaf(&leaf, None).unwrap());
        let retrieved = txn.get_leaf(leaf_id).unwrap().unwrap();
        assert_eq!(retrieved.branch(), branch_id);
        assert_eq!(retrieved.kind(), TokenKind::Word);
    }

    #[test]
    fn test_list_leaves() {
        let mut txn = MockCrdtTxn::new();
        let branch_id = BranchId::new(NodeId::new(1), 0);

        let l1 = LeafId::new(NodeId::new(1), 0);
        let l2 = LeafId::new(NodeId::new(1), 1);

        txn.put_leaf(&Leaf::new(l1, branch_id, TokenKind::Word, 0..2), None).unwrap();
        txn.put_leaf(&Leaf::new(l2, branch_id, TokenKind::Whitespace, 2..3), Some(l1)).unwrap();

        let leaves = txn.list_leaves(branch_id).unwrap();
        assert_eq!(leaves.len(), 2);
    }

    #[test]
    fn test_update_leaf_content() {
        let mut txn = MockCrdtTxn::new();
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let leaf = Leaf::new(leaf_id, branch_id, TokenKind::Word, 0..5);

        txn.put_leaf(&leaf, None).unwrap();
        txn.update_leaf_content(leaf_id, 10..20).unwrap();

        let retrieved = txn.get_leaf(leaf_id).unwrap().unwrap();
        assert_eq!(retrieved.content_range(), 10..20);
    }

    // =========================================================================
    // Extension Methods Tests
    // =========================================================================

    #[test]
    fn test_create_trunk() {
        let mut txn = MockCrdtTxn::new();
        let id = TrunkId::new(NodeId::new(1), 0);

        let trunk = txn.create_trunk(id, "test.rs", Some(Encoding::Utf8)).unwrap();

        assert_eq!(trunk.id(), id);
        assert_eq!(trunk.path(), "test.rs");
        assert_eq!(trunk.encoding(), Some(Encoding::Utf8));
        assert!(txn.has_trunk(id).unwrap());
    }

    #[test]
    fn test_mark_trunk_deleted() {
        let mut txn = MockCrdtTxn::new();
        let id = TrunkId::new(NodeId::new(1), 0);

        txn.create_trunk(id, "test.rs", None).unwrap();
        txn.mark_trunk_deleted(id).unwrap();

        let trunk = txn.get_trunk(id).unwrap().unwrap();
        assert!(trunk.state().is_deleted());
    }

    #[test]
    fn test_mark_trunk_alive() {
        let mut txn = MockCrdtTxn::new();
        let id = TrunkId::new(NodeId::new(1), 0);

        txn.create_trunk(id, "test.rs", None).unwrap();
        txn.mark_trunk_deleted(id).unwrap();
        txn.mark_trunk_alive(id).unwrap();

        let trunk = txn.get_trunk(id).unwrap().unwrap();
        assert!(trunk.state().is_alive());
    }

    #[test]
    fn test_create_branch() {
        let mut txn = MockCrdtTxn::new();
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        let branch = txn.create_branch(branch_id, trunk_id, None).unwrap();

        assert_eq!(branch.id(), branch_id);
        assert_eq!(branch.trunk(), trunk_id);
        assert!(txn.has_branch(branch_id).unwrap());
    }

    #[test]
    fn test_mark_branch_deleted() {
        let mut txn = MockCrdtTxn::new();
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let branch_id = BranchId::new(NodeId::new(1), 0);

        txn.create_branch(branch_id, trunk_id, None).unwrap();
        txn.mark_branch_deleted(branch_id).unwrap();

        let branch = txn.get_branch(branch_id).unwrap().unwrap();
        assert!(branch.state().is_deleted());
    }

    #[test]
    fn test_create_leaf() {
        let mut txn = MockCrdtTxn::new();
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        let leaf = txn.create_leaf(leaf_id, branch_id, TokenKind::Word, 0..5, None).unwrap();

        assert_eq!(leaf.id(), leaf_id);
        assert_eq!(leaf.branch(), branch_id);
        assert_eq!(leaf.kind(), TokenKind::Word);
        assert!(txn.has_leaf(leaf_id).unwrap());
    }

    #[test]
    fn test_mark_leaf_deleted() {
        let mut txn = MockCrdtTxn::new();
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        txn.create_leaf(leaf_id, branch_id, TokenKind::Word, 0..5, None).unwrap();
        txn.mark_leaf_deleted(leaf_id).unwrap();

        let leaf = txn.get_leaf(leaf_id).unwrap().unwrap();
        assert!(leaf.state().is_deleted());
    }

    #[test]
    fn test_alloc_inode() {
        let mut txn = MockCrdtTxn::new();

        let inode1 = txn.alloc_inode().unwrap();
        let inode2 = txn.alloc_inode().unwrap();

        assert_ne!(inode1, inode2);
    }
}

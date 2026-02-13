//! Hierarchical CRDT graph model for Atomic VCS.
//!
//! This module implements a **Trunk → Branch → Leaf** architecture for
//! representing file content as a conflict-free replicated data type (CRDT).
//! This design enables efficient fine-grained tracking of changes at the
//! token level while maintaining the semantic structure of files.
//!
//! # Architecture Overview
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────────┐
//! │                     Trunk → Branch → Leaf Architecture                       │
//! ├─────────────────────────────────────────────────────────────────────────────┤
//! │                                                                             │
//! │  TRUNK (File)                                                               │
//! │  ┌─────────────────────────────────────────────────────────────────────┐   │
//! │  │  id: TrunkId          path: "src/main.rs"    encoding: UTF-8        │   │
//! │  └─────────────────────────────────────────────────────────────────────┘   │
//! │       │                                                                     │
//! │       ├──────────────────┬──────────────────┬─────────────────────┐        │
//! │       ▼                  ▼                  ▼                     ▼        │
//! │  BRANCH (Line 1)    BRANCH (Line 2)    BRANCH (Line 3)      BRANCH (...)  │
//! │  ┌──────────────┐   ┌──────────────┐   ┌──────────────┐                    │
//! │  │ id: B(ch1,0) │   │ id: B(ch1,1) │   │ id: B(ch2,0) │  ← different       │
//! │  │ state: alive │   │ state: alive │   │ state: alive │    change!         │
//! │  └──────────────┘   └──────────────┘   └──────────────┘                    │
//! │       │                  │                                                  │
//! │       ▼                  ▼                                                  │
//! │  ┌────┬────┬────┐   ┌────┬────┬────┬────┐                                  │
//! │  │ fn │ ░░ │main│   │ ░░ │ ░░ │let │ ░░ │   LEAF (Token)                   │
//! │  │L0  │L1  │L2  │   │L0  │L1  │L2  │L3  │   id: L(ch_id, leaf_idx)         │
//! │  └────┴────┴────┘   └────┴────┴────┴────┘                                  │
//! │                                                                             │
//! └─────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Design Principles
//!
//! ## 1. Global Uniqueness Without Coordination
//!
//! Each ID (TrunkId, BranchId, LeafId) embeds the change that created it.
//! Since changes are content-addressed and unique, IDs are automatically
//! globally unique without requiring a central authority.
//!
//! ## 2. Immutability Enables CRDT Semantics
//!
//! Once created, IDs never change. Even deleted content retains its ID
//! (with a deleted state flag). This means:
//! - References to IDs always resolve (no dangling pointers)
//! - Concurrent operations can reference the same ID safely
//! - Undo/redo is trivial (toggle the deleted flag)
//!
//! ## 3. Deterministic Conflict Resolution
//!
//! When two concurrent operations insert at the same position, they are
//! ordered deterministically by their IDs. This ensures all replicas
//! converge to the same state.
//!
//! # Performance Characteristics
//!
//! | Operation | Current (Flat) | Hierarchical |
//! |-----------|----------------|--------------|
//! | Find line N | O(vertices) | O(1) via branch index |
//! | Find token in line | O(vertices) | O(tokens in line) |
//! | Insert line | O(vertices) to find position | O(1) branch insert |
//! | Delete line | O(tokens) edge updates | O(1) mark branch deleted |
//! | Word-diff line | Reconstruct + diff | Compare leaf sequences |
//! | Blame token | Traverse graph | Direct: `leaf.change_id` |
//!
//! # Module Structure
//!
//! - [`ids`] - Unique identifiers (TrunkId, BranchId, LeafId)
//! - [`trunk`] - File-level structures and operations
//! - [`branch`] - Line-level structures and operations
//! - [`leaf`] - Token-level structures and operations
//!
//! # Example
//!
//! ```rust
//! use atomic_core::crdt::{
//!     TrunkId, BranchId, LeafId,
//!     Trunk, Branch, Leaf,
//!     TrunkOp, BranchOp, LeafOp,
//!     TrunkState, BranchState, LeafState,
//! };
//! use atomic_core::change::Encoding;
//! use atomic_core::diff::token::TokenKind;
//! use atomic_core::types::{NodeId, Inode};
//!
//! // Create a file
//! let change_id = NodeId::new(1);
//! let trunk = Trunk::new(
//!     TrunkId::new(change_id, 0),
//!     Inode::new(42),
//!     "src/main.rs".to_string(),
//!     Some(Encoding::Utf8),
//! );
//!
//! // Create a line in that file
//! let branch = Branch::new(
//!     BranchId::new(change_id, 0),
//!     trunk.id(),
//! );
//!
//! // Create a token in that line
//! let leaf = Leaf::new(
//!     LeafId::new(change_id, 0),
//!     branch.id(),
//!     TokenKind::Word,
//!     0..2,  // "fn" in content blob
//! );
//!
//! assert!(trunk.state().is_alive());
//! assert!(branch.state().is_alive());
//! assert!(leaf.state().is_alive());
//! ```

pub mod apply;
pub mod branch;
pub mod ids;
pub mod leaf;
pub mod tables;
pub mod trunk;

// Re-export ID types
pub use ids::{BranchId, LeafId, TrunkId};

// Re-export structures
pub use branch::Branch;
pub use leaf::Leaf;
pub use trunk::Trunk;

// Re-export state enums
pub use branch::BranchState;
pub use leaf::LeafState;
pub use trunk::TrunkState;

// Re-export operation types
pub use branch::BranchOp;
pub use leaf::LeafOp;
pub use trunk::TrunkOp;

// Re-export table definitions and encoding helpers
pub use tables::{
    // Table definitions
    BRANCHES, BRANCH_LEAVES, INODE_TRUNK, LEAVES, PATH_TRUNK, TRUNKS, TRUNK_BRANCHES,
    // ID encoding/decoding
    decode_branch_id, decode_leaf_id, decode_trunk_id, encode_branch_id, encode_leaf_id,
    encode_trunk_id,
    // Value encoding/decoding
    decode_branch_value, decode_leaf_value, decode_trunk_value, encode_branch_value,
    encode_leaf_value, encode_trunk_value,
    // Serialized value types
    SerializedBranch, SerializedLeaf, SerializedTrunk,
    // State encoding/decoding
    decode_branch_state, decode_leaf_state, decode_trunk_state, encode_branch_state,
    encode_leaf_state, encode_trunk_state,
    // TokenKind encoding/decoding
    decode_token_kind, encode_token_kind,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::Encoding;
    use crate::diff::token::TokenKind;
    use crate::types::{Inode, NodeId};

    #[test]
    fn test_full_hierarchy() {
        let change_id = NodeId::new(1);

        // Create trunk (file)
        let trunk = Trunk::new(
            TrunkId::new(change_id, 0),
            Inode::new(1),
            "test.rs".to_string(),
            Some(Encoding::Utf8),
        );

        // Create branch (line)
        let branch = Branch::new(BranchId::new(change_id, 0), trunk.id());

        // Create leaf (token)
        let leaf = Leaf::new(
            LeafId::new(change_id, 0),
            branch.id(),
            TokenKind::Word,
            0..4,
        );

        // Verify hierarchy
        assert_eq!(leaf.branch(), branch.id());
        assert_eq!(branch.trunk(), trunk.id());

        // Verify all are alive
        assert!(trunk.state().is_alive());
        assert!(branch.state().is_alive());
        assert!(leaf.state().is_alive());
    }

    #[test]
    fn test_operations_create() {
        // File creation
        let create_file = TrunkOp::Create {
            path: "new.rs".to_string(),
            encoding: Some(Encoding::Utf8),
        };
        assert!(create_file.is_create());

        // Line insertion
        let insert_line = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        assert!(insert_line.is_insert());

        // Token insertion
        let insert_token = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        };
        assert!(insert_token.is_insert());
    }

    #[test]
    fn test_id_ordering_for_conflict_resolution() {
        // IDs from earlier changes sort before IDs from later changes
        let early_change = NodeId::new(1);
        let late_change = NodeId::new(2);

        let early_branch = BranchId::new(early_change, 0);
        let late_branch = BranchId::new(late_change, 0);

        // Concurrent inserts at same position are ordered by ID
        assert!(early_branch < late_branch);
    }

    #[test]
    fn test_deletion_preserves_id() {
        let change_id = NodeId::new(1);
        let mut leaf = Leaf::new(
            LeafId::new(change_id, 0),
            BranchId::new(change_id, 0),
            TokenKind::Word,
            0..5,
        );

        let original_id = leaf.id();

        // Delete the leaf
        leaf.delete();
        assert!(leaf.state().is_deleted());

        // ID is preserved
        assert_eq!(leaf.id(), original_id);

        // Can be restored
        leaf.restore();
        assert!(leaf.state().is_alive());
        assert_eq!(leaf.id(), original_id);
    }

    #[test]
    fn test_replace_preserves_id_for_blame() {
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        // Replace operation targets the existing leaf
        let replace_op = LeafOp::Replace {
            leaf: leaf_id,
            new_content: b"new_value".to_vec(),
        };

        // The leaf ID is preserved, enabling accurate blame
        assert_eq!(replace_op.leaf_id(), Some(leaf_id));
    }
}

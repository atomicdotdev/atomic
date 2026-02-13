//! Apply CRDT operations to the pristine database.
//!
//! This module is responsible for applying CRDT operations (TrunkOp, BranchOp,
//! LeafOp) generated during the record workflow to the pristine database. It
//! maintains the hierarchical Trunk → Branch → Leaf structure and ensures
//! CRDT invariants are preserved.
//!
//! # Architecture Overview
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────────┐
//! │                      CRDT Apply Workflow Pipeline                            │
//! ├─────────────────────────────────────────────────────────────────────────────┤
//! │                                                                             │
//! │  Input: CRDT Operations              Output: Updated Pristine               │
//! │  ┌──────────────────────┐           ┌────────────────────────────┐         │
//! │  │ TrunkOp (file ops)   │           │ TRUNKS table               │         │
//! │  │ BranchOp (line ops)  │  apply()  │ BRANCHES table             │         │
//! │  │ LeafOp (token ops)   │ ────────► │ LEAVES table               │         │
//! │  │ Content blob         │           │ Ordering multimaps         │         │
//! │  └──────────────────────┘           │ Reverse lookup tables      │         │
//! │                                      └────────────────────────────┘         │
//! │                                                                             │
//! │  Processing Pipeline:                                                       │
//! │  ┌─────────────────────────────────────────────────────────────────────┐   │
//! │  │ 1. trunk.rs   - Apply TrunkOp (create/delete/move/undelete files)  │   │
//! │  │ 2. branch.rs  - Apply BranchOp (insert/delete/restore lines)       │   │
//! │  │ 3. leaf.rs    - Apply LeafOp (insert/delete/replace/restore tokens)│   │
//! │  │ 4. order.rs   - Maintain CRDT ordering for concurrent operations   │   │
//! │  │ 5. conflict.rs - Detect and track conflicts during apply           │   │
//! │  └─────────────────────────────────────────────────────────────────────┘   │
//! │                                                                             │
//! └─────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Design Principles
//!
//! ## 1. CRDT Semantics Preservation
//!
//! All operations preserve CRDT semantics:
//! - Operations are commutative where possible
//! - Concurrent insertions are ordered deterministically by ID
//! - Deletions mark content as deleted rather than removing it
//! - IDs are immutable and globally unique
//!
//! ## 2. Atomic Application
//!
//! Operations are applied atomically within a transaction:
//! - Either all operations in a change succeed, or none do
//! - Partial application is never visible to readers
//! - Conflicts are detected and tracked, not silently ignored
//!
//! ## 3. Efficient Storage Updates
//!
//! The apply workflow updates multiple tables efficiently:
//! - Primary tables: TRUNKS, BRANCHES, LEAVES
//! - Ordering multimaps: TRUNK_BRANCHES, BRANCH_LEAVES
//! - Reverse lookups: INODE_TRUNK, PATH_TRUNK
//!
//! # Module Structure
//!
//! - [`error`] - Error types for apply failures
//! - [`options`] - Configuration options for apply behavior
//! - [`trunk`] - Apply TrunkOp operations (file level)
//! - [`branch`] - Apply BranchOp operations (line level)
//! - [`leaf`] - Apply LeafOp operations (token level)
//! - [`order`] - CRDT ordering and conflict resolution
//! - [`conflict`] - Conflict detection and tracking
//! - [`context`] - Apply context and state management
//!
//! # Key Types
//!
//! - [`ApplyOptions`] - Configuration for apply behavior
//! - [`ApplyContext`] - State maintained during apply
//! - [`ApplyOutcome`] - Result of applying CRDT operations
//! - [`ApplyStats`] - Statistics about the apply process
//! - [`CrdtConflict`] - Represents a detected conflict
//!
//! # Example: Applying a Complete Change
//!
//! ```rust,ignore
//! use atomic_core::crdt::apply::{
//!     ApplyContext, ApplyOptions, apply_crdt_change,
//! };
//! use atomic_core::record::workflow::crdt::CrdtChangeResult;
//!
//! fn apply_recorded_change(
//!     txn: &mut impl MutCrdtTxnT,
//!     crdt_result: &CrdtChangeResult,
//!     content: &[u8],
//! ) -> Result<ApplyOutcome, ApplyError> {
//!     let options = ApplyOptions::default();
//!     let mut context = ApplyContext::new(options);
//!
//!     // Apply all file operations
//!     for file_ops in crdt_result.file_ops() {
//!         // Apply trunk operation (create/delete/move file)
//!         if let Some(trunk_op) = file_ops.trunk_op() {
//!             apply_trunk_op(txn, &mut context, trunk_op)?;
//!         }
//!
//!         // Apply line operations
//!         for line_ops in file_ops.line_ops() {
//!             apply_branch_op(txn, &mut context, line_ops.branch_op())?;
//!
//!             // Apply token operations
//!             for leaf_op in line_ops.leaf_ops() {
//!                 apply_leaf_op(txn, &mut context, leaf_op, content)?;
//!             }
//!         }
//!     }
//!
//!     context.finish()
//! }
//! ```
//!
//! # Example: Applying Individual Operations
//!
//! ```rust,ignore
//! use atomic_core::crdt::{TrunkId, TrunkOp, BranchId, BranchOp};
//! use atomic_core::crdt::apply::{apply_trunk_op, apply_branch_op, ApplyContext};
//! use atomic_core::change::Encoding;
//! use atomic_core::types::NodeId;
//!
//! // Apply a file creation
//! let create_op = TrunkOp::Create {
//!     path: "src/main.rs".to_string(),
//!     encoding: Some(Encoding::Utf8),
//! };
//! let trunk_id = TrunkId::new(change_id, 0);
//! apply_trunk_op(txn, &mut context, trunk_id, &create_op)?;
//!
//! // Apply a line insertion
//! let insert_op = BranchOp::Insert {
//!     after: None, // Insert at start
//!     content: vec![], // Leaf ops applied separately
//! };
//! let branch_id = BranchId::new(change_id, 0);
//! apply_branch_op(txn, &mut context, trunk_id, branch_id, &insert_op)?;
//! ```
//!
//! # Conflict Handling
//!
//! When concurrent operations conflict, the apply module:
//!
//! 1. **Detects** the conflict type (ordering, deletion, etc.)
//! 2. **Records** the conflict in the context
//! 3. **Resolves** using deterministic CRDT rules (ID ordering)
//! 4. **Reports** conflicts in the outcome for user awareness
//!
//! ## Conflict Types
//!
//! - **Concurrent Insert**: Two operations insert at the same position.
//!   Resolved by ordering insertions by their IDs.
//!
//! - **Delete/Modify**: One operation deletes content that another modifies.
//!   The modification creates a "zombie" that may need user resolution.
//!
//! - **Move/Delete**: File is moved and deleted concurrently.
//!   The delete takes precedence (can be undone).
//!
//! # Performance Characteristics
//!
//! | Operation | Complexity | Notes |
//! |-----------|------------|-------|
//! | Apply TrunkOp | O(1) | Single table insert/update |
//! | Apply BranchOp::Insert | O(log n) | Find insertion position |
//! | Apply LeafOp::Insert | O(log m) | Find insertion position |
//! | Apply deletion ops | O(1) | Mark as deleted |
//! | Find insertion point | O(log n) | Binary search in ordering |
//!
//! Where n = branches in trunk, m = leaves in branch.
//!
//! # Thread Safety
//!
//! Apply operations require exclusive (write) access to the transaction.
//! The [`ApplyContext`] is not thread-safe and should be used within a
//! single transaction scope.
//!
//! # Relationship to Record Workflow
//!
//! The apply module is the inverse of the record workflow:
//!
//! ```text
//! Record Workflow                    Apply Workflow
//! ┌────────────────┐                ┌────────────────┐
//! │ Working Copy   │                │ CRDT Tables    │
//! │      ↓         │                │      ↓         │
//! │ Diff Analysis  │                │ Validate Ops   │
//! │      ↓         │                │      ↓         │
//! │ CRDT Ops       │ ─────────────► │ Apply to DB    │
//! │      ↓         │                │      ↓         │
//! │ Serialize      │                │ Update Indexes │
//! └────────────────┘                └────────────────┘
//! ```
//!
//! [`ApplyOptions`]: options::ApplyOptions
//! [`ApplyContext`]: context::ApplyContext
//! [`ApplyOutcome`]: context::ApplyOutcome
//! [`ApplyStats`]: context::ApplyStats
//! [`CrdtConflict`]: conflict::CrdtConflict

pub mod branch;
pub mod conflict;
pub mod context;
pub mod error;
pub mod leaf;
pub mod options;
pub mod order;
pub mod traits;
pub mod trunk;

// Re-export primary types for convenience
pub use context::{ApplyContext, ApplyOutcome, ApplyStats};
pub use error::{ApplyError, ApplyResult};
pub use options::ApplyOptions;

// Re-export operation application functions
pub use branch::apply_branch_op;
pub use leaf::apply_leaf_op;
pub use trunk::apply_trunk_op;

// Re-export conflict types
pub use conflict::{ConflictKind, CrdtConflict, CrdtConflictTracker};

// Re-export ordering utilities
pub use order::{find_insert_position, CrdtOrdering};

// Re-export transaction trait
pub use traits::MutCrdtTxnT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_exports_exist() {
        // Verify that all expected types are accessible
        let _ = std::any::type_name::<ApplyOptions>();
        let _ = std::any::type_name::<ApplyContext>();
        let _ = std::any::type_name::<ApplyOutcome>();
        let _ = std::any::type_name::<ApplyStats>();
        let _ = std::any::type_name::<ApplyError>();
        let _ = std::any::type_name::<CrdtConflict>();
        let _ = std::any::type_name::<ConflictKind>();
    }

    #[test]
    fn test_apply_result_type_alias() {
        // Verify ApplyResult is a proper Result type alias
        fn _check_result_type() -> ApplyResult<()> {
            Ok(())
        }
        assert!(_check_result_type().is_ok());
    }
}

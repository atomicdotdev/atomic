//! Applying changes to the repository graph
//!
//! The **apply** module is responsible for taking a [`Change`] and modifying
//! the repository graph to reflect its contents. This includes adding vertices,
//! updating edges, maintaining the dependency graph, and updating file tree
//! mappings.
//!
//! # Two-Tier Edge Routing
//!
//! The apply module uses [`ApplyTarget`] to route edges to the correct storage
//! table based on the stack kind:
//!
//! - **`ApplyTarget::Global`**: Edges go to the global `GRAPH` + `INODE_GRAPH`
//!   tables. Used for Shared stacks (dev, release, main).
//! - **`ApplyTarget::Local { stack_id }`**: Edges go to the per-stack
//!   `STACK_GRAPH[(stack_id, vertex)]` table. Used for Local workspaces
//!   (feature, bug, service-*). Cascade-deleted when the stack is removed.
//!
//! # Overview
//!
//! Applying a change is the inverse of recording:
//!
//! 1. **Validate** that all dependencies are present in the repository
//! 2. **Register** the change to get an internal [`NodeId`]
//! 3. **Apply** each atom (span or edge operation) to the graph
//! 4. **Update** the stack's Merkle state
//! 5. **Update** file tree mappings for added/deleted files
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                          Apply Pipeline                                 │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Change File           Validator             Graph Operations           │
//! │  ┌──────────┐        ┌───────────┐         ┌─────────────────┐         │
//! │  │  Hunks   │ parse  │ Check     │ apply   │  Add Vertices   │         │
//! │  │  Atoms   │ ─────► │ Deps      │ ──────► │  Update Edges   │         │
//! │  │  Deps    │        │ Conflicts │         │  Track Deps     │         │
//! │  └──────────┘        └───────────┘         └─────────────────┘         │
//! │       │                    │                       │                   │
//! │       │                    │                       │                   │
//! │       ▼                    ▼                       ▼                   │
//! │  ┌──────────┐        ┌───────────┐         ┌─────────────────┐         │
//! │  │ Contents │        │ NodeId    │         │ Updated Graph   │         │
//! │  │ (bytes)  │        │ Assigned  │         │ New Merkle      │         │
//! │  └──────────┘        └───────────┘         └─────────────────┘         │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Module Structure
//!
//! - [`change`]: Core change application functions (validation, state computation)
//! - [`position`]: Position resolution between external hashes and internal IDs
//! - [`insertion`]: Insertion atom application (inserting new content)
//! - [`edge`]: EdgeUpdate atom application (modifying existing edges)
//! - [`conflict`]: Conflict detection and tracking (zombies, missing context)
//! - [`workspace`]: Temporary state during application
//! - [`error`]: Error types for application failures
//!
//! # Key Components
//!
//! - [`apply_new_vertex`]: Apply a Insertion atom to insert content
//! - [`apply_edge_map`]: Apply an EdgeUpdate atom to modify edges
//! - [`verify_dependencies`]: Check all dependencies are present
//! - [`compute_new_state`]: Calculate new Merkle state
//! - [`Workspace`]: Temporary state during application
//! - [`ConflictTracker`]: Track conflicts for later resolution
//!
//! # Dependency Verification
//!
//! Before a change can be applied, all its dependencies must be present:
//!
//! ```rust,ignore
//! use atomic_core::apply::{verify_dependencies, ApplyError};
//!
//! // Check dependencies before applying
//! let missing = verify_dependencies(&txn, &change)?;
//! if !missing.is_empty() {
//!     for hash in &missing {
//!         eprintln!("Missing dependency: {}", hash);
//!     }
//!     return Err(ApplyError::Local(
//!         LocalApplyError::DependencyMissing { hash: missing[0] }
//!     ));
//! }
//! ```
//!
//! # Atom Application
//!
//! Changes contain atoms - the primitive graph operations:
//!
//! ## Insertion
//!
//! Inserts new content into the graph:
//!
//! ```rust,ignore
//! // A Insertion adds content between context vertices
//! let atom = Atom::Insertion(Insertion {
//!     predecessors: vec![parent_pos],   // What comes before
//!     successors: vec![child_pos],  // What comes after
//!     flag: EdgeFlags::BLOCK,
//!     start: ChangePosition::new(0),
//!     end: ChangePosition::new(100),
//!     inode: file_position,
//! });
//! ```
//!
//! ## EdgeUpdate
//!
//! Modifies existing edges (deletion, undeletion, etc.):
//!
//! ```rust,ignore
//! // An EdgeUpdate marks edges as deleted
//! let atom = Atom::EdgeUpdate(EdgeUpdate {
//!     edges: vec![
//!         NewEdge {
//!             previous: EdgeFlags::BLOCK,
//!             flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
//!             from: source_pos,
//!             to: dest_vertex,
//!             introduced_by: original_change,
//!         }
//!     ],
//!     inode: file_position,
//! });
//! ```
//!
//! # Stack Updates
//!
//! After applying a change, the stack is updated:
//!
//! 1. The change is added to the stack's change log
//! 2. The Merkle state is updated: `new_state = Hash(old_state || change_hash)`
//! 3. The change count is incremented
//!
//! # Conflict Handling
//!
//! During application, conflicts can arise:
//!
//! - **Zombie vertices**: Deleted content that's been modified elsewhere
//! - **Missing context**: Context vertices that don't exist
//! - **Order conflicts**: Ambiguous insertion order
//!
//! The apply module handles these by:
//!
//! 1. Detecting conflicts during application
//! 2. Marking conflicting regions with special edges
//! 3. Tracking conflicts in the [`ConflictTracker`]
//! 4. Allowing the working copy to show conflict markers
//!
//! # Error Handling
//!
//! Application can fail for various reasons:
//!
//! - **Missing dependencies**: Required changes not yet applied
//! - **Already applied**: Change is already on the stack
//! - **Invalid format**: Corrupted or malformed change data
//! - **Graph inconsistency**: Operations that violate graph invariants
//!
//! See [`ApplyError`] and [`LocalApplyError`] for complete error types.
//!
//! # Example: Applying a Change
//!
//! ```rust,ignore
//! use atomic_core::apply::{
//!     verify_dependencies, validate_can_apply, compute_new_state,
//!     apply_new_vertex, apply_edge_map, Workspace, ApplyError,
//! };
//! use atomic_core::change::{Change, Atom};
//!
//! fn apply_to_stack(
//!     txn: &mut impl MutTxnT,
//!     stack: &mut StackState,
//!     change: &Change,
//!     change_hash: &Hash,
//! ) -> Result<(), ApplyError> {
//!     // Register the change to get an internal ID
//!     let change_id = txn.register_change(change_hash)?;
//!
//!     // Validate the change can be applied
//!     validate_can_apply(txn, stack, change_id, change_hash, change)?;
//!
//!     // Create workspace for tracking state
//!     let mut workspace = Workspace::new();
//!
//!     // Apply each graph_op's atoms
//!     for graph_op in change.hunks() {
//!         for atom in graph_op.atoms() {
//!             match atom {
//!                 Atom::Insertion(nv) => {
//!                     apply_new_vertex(txn, &mut workspace, change_id, nv, change)?;
//!                 }
//!                 Atom::EdgeUpdate(em) => {
//!                     apply_edge_map(txn, &mut workspace, change_id, em, change)?;
//!                 }
//!             }
//!         }
//!     }
//!
//!     // Update stack state
//!     let new_state = compute_new_state(&stack.state, change_hash);
//!     txn.put_change(stack, change_id, change_hash)?;
//!     txn.update_stack(stack)?;
//!
//!     // Commit the transaction
//!     txn.commit()?;
//!
//!     Ok(())
//! }
//! ```
//!
//! # Performance Considerations
//!
//! - **Batch application**: Apply multiple changes in a single transaction
//! - **Workspace reuse**: Reuse the workspace across multiple applies
//! - **Lazy content loading**: Only load change contents when needed
//!
//! # Thread Safety
//!
//! The apply operations require exclusive (write) access to the transaction.
//! For concurrent access patterns, use separate transactions and merge
//! the results.
//!
//! [`Change`]: crate::change::Change
//! [`NodeId`]: crate::types::NodeId
//! [`ApplyError`]: crate::apply::ApplyError
//! [`LocalApplyError`]: crate::apply::LocalApplyError
//! [`Workspace`]: crate::apply::Workspace
//! [`ConflictTracker`]: crate::apply::ConflictTracker

mod change;
pub mod conflict;
pub mod edge;
mod error;
pub mod file_ops;
pub mod insertion;
pub mod position;
mod workspace;

// Two-Tier Edge Routing

/// Controls where edges are written during change application.
///
/// This is the bridge between the stack model (`StackKind`) and the apply
/// pipeline. The caller constructs the appropriate `ApplyTarget` from the
/// stack being applied to, and passes it through to `add_edge_with_reverse`
/// and `del_edge_with_reverse`.
///
/// # Construction
///
/// ```rust,ignore
/// use atomic_core::apply::ApplyTarget;
/// use atomic_core::pristine::StackKind;
///
/// let target = match stack.kind {
///     StackKind::Shared   => ApplyTarget::Global,
///     StackKind::Local => ApplyTarget::Local { stack_id: stack.id },
/// };
/// ```
///
/// # Edge Routing
///
/// | Target | Forward/Reverse Edges | Inode Index |
/// |--------|----------------------|-------------|
/// | `Global` | `GRAPH` | `INODE_GRAPH` |
/// | `Local { stack_id }` | `STACK_GRAPH[(stack_id, vertex)]` | — |
///
/// Local workspaces do not populate `INODE_GRAPH` because their edges are
/// ephemeral and the inode index is only needed for long-lived graph data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyTarget {
    /// Write edges to the global `GRAPH` and `INODE_GRAPH` tables.
    ///
    /// Used for Shared stacks (dev, release, main). These edges are
    /// permanent and visible to all stacks.
    Global,

    /// Write edges to `STACK_GRAPH[(stack_id, vertex)]`.
    ///
    /// Used for Local workspaces (feature, bug, service-*). These edges
    /// are only visible through the overlay chain and are cascade-deleted
    /// when the stack is removed.
    Local {
        /// The local workspace's internal ID.
        stack_id: u64,
    },
}

impl ApplyTarget {
    /// Create an `ApplyTarget` from a stack's kind and ID.
    ///
    /// This is the canonical way to construct the target from stack metadata.
    #[inline]
    pub fn from_stack_kind(kind: crate::pristine::StackKind, stack_id: u64) -> Self {
        match kind {
            crate::pristine::StackKind::Shared => Self::Global,
            crate::pristine::StackKind::Local => Self::Local { stack_id },
        }
    }

    /// Check if this targets the global graph.
    #[inline]
    pub fn is_global(&self) -> bool {
        matches!(self, Self::Global)
    }

    /// Check if this targets an local workspace's graph.
    #[inline]
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }
}

// Re-export core change application functions
pub use change::{
    compute_new_state, is_change_on_stack, validate_can_apply, verify_dependencies,
    ApplyResult as ApplyChangeResult, ChangeToApply,
};

// Re-export error types
pub use error::{ApplyError, ApplyResult, LocalApplyError, LocalApplyResult};

// Re-export workspace types
pub use workspace::{MissingContext, PendingEdge, Workspace, WorkspaceStats, Zombie};

// Re-export position resolution functions
pub use position::{
    resolve_context_vertex, resolve_inode, resolve_introduced_by, resolve_position, resolve_vertex,
};

// Re-export atom application functions
pub use edge::{apply_edge_map, find_source_vertex, find_target_vertex};
pub use insertion::{add_edge_with_reverse, apply_new_vertex};

// Re-export conflict tracking types
pub use conflict::{
    ConflictSummary, ConflictTracker, MissingContextConflict, OrderConflict, ZombieConflict,
};

// Re-export FileOps application
pub use file_ops::{apply_file_ops, ApplyFileOpsStats};

#[cfg(test)]
mod target_tests {
    use super::*;
    use crate::pristine::StackKind;

    #[test]
    fn apply_target_from_shared_stack() {
        let target = ApplyTarget::from_stack_kind(StackKind::Shared, 42);
        assert_eq!(target, ApplyTarget::Global);
        assert!(target.is_global());
        assert!(!target.is_local());
    }

    #[test]
    fn apply_target_from_isolated_stack() {
        let target = ApplyTarget::from_stack_kind(StackKind::Local, 7);
        assert_eq!(target, ApplyTarget::Local { stack_id: 7 });
        assert!(target.is_local());
        assert!(!target.is_global());
    }
}

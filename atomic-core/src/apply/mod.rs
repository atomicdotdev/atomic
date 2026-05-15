//! Applying changes to the repository graph
//!
//! The **apply** module is responsible for taking a [`Change`] and modifying
//! the repository graph to reflect its contents. This includes adding vertices,
//! updating edges, maintaining the dependency graph, and updating file tree
//! mappings.
//!
//! # Overview
//!
//! Applying a change is the inverse of recording:
//!
//! 1. **Validate** that all dependencies are present in the repository
//! 2. **Register** the change to get an internal [`NodeId`]
//! 3. **Apply** each atom (span or edge operation) to the graph
//! 4. **Update** the view's Merkle state
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
//! - `error`: Error types for application failures
//!
//! # Key Components
//!
//! - [`write_new_vertex`]: Apply a Insertion atom to insert content
//! - [`write_edge_map`]: Apply an EdgeUpdate atom to modify edges
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
//! # View Updates
//!
//! After applying a change, the view is updated:
//!
//! 1. The change is added to the view's change log
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
//! - **Already applied**: Change is already on the view
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
//!     write_new_vertex, write_edge_map, Workspace, ApplyError,
//! };
//! use atomic_core::change::{Change, Atom};
//!
//! fn apply_to_view(
//!     txn: &mut impl MutTxnT,
//!     view: &mut ViewState,
//!     change: &Change,
//!     change_hash: &Hash,
//! ) -> Result<(), ApplyError> {
//!     // Register the change to get an internal ID
//!     let change_id = txn.register_change(change_hash)?;
//!
//!     // Validate the change can be applied
//!     validate_can_apply(txn, view, change_id, change_hash, change)?;
//!
//!     // Create workspace for tracking state
//!     let mut workspace = Workspace::new();
//!
//!     // Apply each graph_op's atoms
//!     for graph_op in change.hunks() {
//!         for atom in graph_op.atoms() {
//!             match atom {
//!                 Atom::Insertion(nv) => {
//!                     write_new_vertex(txn, &mut workspace, change_id, nv, change)?;
//!                 }
//!                 Atom::EdgeUpdate(em) => {
//!                     write_edge_map(txn, &mut workspace, change_id, em, change)?;
//!                 }
//!             }
//!         }
//!     }
//!
//!     // Update view state
//!     let new_state = compute_new_state(&view.state, change_hash);
//!     txn.put_change(view, change_id, change_hash)?;
//!     txn.update_view(view)?;
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
mod graph_batch;
pub mod insertion;
pub mod position;
mod workspace;

// Re-export core change application functions
pub use change::{
    compute_new_state, is_change_on_view, validate_can_apply, verify_dependencies,
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
pub use edge::{find_source_vertex, find_target_vertex, write_edge_map, write_edge_map_batched};
pub use insertion::{add_edge_with_reverse, write_new_vertex, write_new_vertex_batched};

// Re-export conflict tracking types
pub use conflict::{
    ConflictSummary, ConflictTracker, MissingContextConflict, OrderConflict, ZombieConflict,
};

// Re-export FileOps application
pub use file_ops::{apply_file_ops, apply_file_ops_batched, ApplyFileOpsStats};
pub use graph_batch::GraphWriteBatch;

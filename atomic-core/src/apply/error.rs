//! Error types for the apply module
//!
//! This module defines error types that can occur during the application process.
//! Applying is the process of taking a [`Change`] and modifying the repository
//! graph to reflect its contents - adding vertices, updating edges, and
//! maintaining the dependency graph.
//!
//! # Error Categories
//!
//! Errors during application fall into several categories:
//!
//! - **Dependency Errors**: Missing required changes that must be applied first
//! - **Conflict Errors**: Changes that conflict with current graph state
//! - **Database Errors**: Pristine storage layer failures
//! - **Validation Errors**: Invalid change format or corrupted data
//! - **State Errors**: Repository in unexpected state
//!
//! # Error Hierarchy
//!
//! There are two main error types:
//!
//! - [`ApplyError`]: High-level errors that wrap storage and local errors
//! - [`LocalApplyError`]: Errors specific to the apply logic itself
//!
//! # Example
//!
//! ```rust
//! use atomic_core::apply::{ApplyError, LocalApplyError};
//!
//! fn handle_apply_error(err: ApplyError) {
//!     match &err {
//!         ApplyError::Local(local_err) => {
//!             if let LocalApplyError::DependencyMissing { hash } = local_err {
//!                 eprintln!("Missing dependency: {}", hash);
//!             }
//!         }
//!         ApplyError::Pristine(p_err) => {
//!             eprintln!("Storage error: {}", p_err);
//!         }
//!         _ => {
//!             eprintln!("Apply failed: {}", err);
//!         }
//!     }
//! }
//! ```
//!
//! [`Change`]: crate::change::Change

use std::path::PathBuf;

use thiserror::Error;

use crate::pristine::PristineError;
use crate::types::{Hash, Merkle, NodeId, Position};

/// Errors specific to the local apply logic.
///
/// These errors occur during the actual application of a change to the
/// repository graph. They represent conditions that prevent a change
/// from being applied correctly.
///
/// # Dependency Handling
///
/// Changes in Atomic have explicit dependencies. Before a change can be
/// applied, all of its dependencies must already be present in the repository.
/// The [`DependencyMissing`] variant indicates this requirement wasn't met.
///
/// # Example
///
/// ```rust
/// use atomic_core::apply::LocalApplyError;
/// use atomic_core::types::Hash;
///
/// fn check_dependency_error(err: &LocalApplyError) -> Option<&Hash> {
///     if let LocalApplyError::DependencyMissing { hash } = err {
///         Some(hash)
///     } else {
///         None
///     }
/// }
/// ```
///
/// [`DependencyMissing`]: LocalApplyError::DependencyMissing
#[derive(Debug, Error)]
pub enum LocalApplyError {
    /// A required dependency is missing from the repository.
    ///
    /// Changes must be applied in dependency order. This error indicates
    /// that a change this one depends on has not yet been applied.
    ///
    /// # Resolution
    ///
    /// Apply the missing dependency first, then retry.
    ///
    /// # Fields
    ///
    /// * `hash` - The hash of the missing dependency
    #[error("Dependency missing: {hash}")]
    DependencyMissing {
        /// The hash of the change that must be applied first
        hash: Hash,
    },

    /// The change has already been applied to this stack.
    ///
    /// Applying the same change twice is not allowed and would corrupt
    /// the graph state.
    ///
    /// # Fields
    ///
    /// * `hash` - The hash of the already-applied change
    #[error("Change already applied to stack: {hash}")]
    ChangeAlreadyApplied {
        /// The hash of the change that's already on the stack
        hash: Hash,
    },

    /// A tag has already been applied to this stack.
    ///
    /// Tags, like changes, can only be applied once per stack.
    ///
    /// # Fields
    ///
    /// * `hash` - The hash of the already-applied tag
    #[error("Tag already applied to stack: {hash}")]
    TagAlreadyApplied {
        /// The hash of the tag that's already on the stack
        hash: Hash,
    },

    /// Tag state doesn't match the expected stack state.
    ///
    /// Tags are associated with a specific Merkle state. If the stack's
    /// current state doesn't match, the tag cannot be applied.
    ///
    /// # Fields
    ///
    /// * `tag_hash` - The hash of the tag
    /// * `expected_state` - The Merkle state the tag expects
    /// * `actual_state` - The stack's current Merkle state
    #[error("Tag state mismatch for {tag_hash}: expected {expected_state}, got {actual_state}")]
    TagStateMismatch {
        /// The hash of the tag being applied
        tag_hash: Hash,
        /// The Merkle state the tag was created for
        expected_state: Merkle,
        /// The current state of the stack
        actual_state: Merkle,
    },

    /// The tag is not registered in the repository.
    ///
    /// Tags must be registered (have an internal NodeId) before they
    /// can be applied.
    ///
    /// # Fields
    ///
    /// * `hash` - The hash of the unregistered tag
    #[error("Tag not registered: {hash}")]
    TagNotRegistered {
        /// The hash of the tag that's not in the repository
        hash: Hash,
    },

    /// A referenced block (span) could not be found in the graph.
    ///
    /// This indicates the change references a position that doesn't
    /// exist in the current graph state, which may indicate corruption
    /// or an incorrect dependency.
    ///
    /// # Fields
    ///
    /// * `position` - The position that couldn't be found
    #[error("Block not found at position: {:?}", position)]
    BlockNotFound {
        /// The position that has no corresponding span
        position: Position<NodeId>,
    },

    /// The change format is invalid.
    ///
    /// This occurs when the change data is malformed or contains
    /// inconsistent information.
    #[error("Invalid change format")]
    InvalidChange,

    /// The change references a non-existent node.
    ///
    /// This indicates the change depends on a node (change or tag)
    /// that is not registered in this repository.
    ///
    /// # Fields
    ///
    /// * `node_id` - The internal ID that doesn't exist
    #[error("Node not found: {node_id:?}")]
    NodeNotFound {
        /// The internal node ID that's missing
        node_id: NodeId,
    },

    /// Repository corruption was detected.
    ///
    /// This is a serious error indicating that the repository data
    /// is in an inconsistent state.
    #[error("Repository corruption detected")]
    Corruption,

    /// The graph would become cyclic if this change were applied.
    ///
    /// The repository graph must remain acyclic. This error indicates
    /// the change would create a cycle.
    ///
    /// # Fields
    ///
    /// * `message` - Description of the cycle detected
    #[error("Cyclic dependency detected: {message}")]
    CyclicDependency {
        /// Description of the detected cycle
        message: String,
    },

    /// The change contains an inconsistent edge operation.
    ///
    /// Edge operations must be valid with respect to the current graph
    /// state. This error indicates an edge operation that doesn't make
    /// sense (e.g., deleting a non-existent edge).
    ///
    /// # Fields
    ///
    /// * `message` - Description of the inconsistency
    #[error("Inconsistent edge operation: {message}")]
    InconsistentEdge {
        /// Description of what's inconsistent
        message: String,
    },

    /// A context span required by the change is missing.
    ///
    /// When inserting new vertices, the change specifies context
    /// (neighboring vertices). If those context vertices are missing,
    /// the change cannot be applied.
    ///
    /// # Fields
    ///
    /// * `position` - The position of the missing context
    #[error("Missing context at position: {:?}", position)]
    MissingContext {
        /// The position where context was expected
        position: Position<NodeId>,
    },

    /// An internal error occurred during application.
    ///
    /// This indicates a bug in the apply logic.
    ///
    /// # Fields
    ///
    /// * `message` - Description of the internal error
    #[error("Internal error: {message}")]
    Internal {
        /// What went wrong internally
        message: String,
    },
}

impl LocalApplyError {
    /// Create a new dependency-missing error.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the missing dependency
    pub fn dependency_missing(hash: Hash) -> Self {
        Self::DependencyMissing { hash }
    }

    /// Create a new change-already-applied error.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the already-applied change
    pub fn change_already_applied(hash: Hash) -> Self {
        Self::ChangeAlreadyApplied { hash }
    }

    /// Create a new tag-already-applied error.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the already-applied tag
    pub fn tag_already_applied(hash: Hash) -> Self {
        Self::TagAlreadyApplied { hash }
    }

    /// Create a new tag-state-mismatch error.
    ///
    /// # Arguments
    ///
    /// * `tag_hash` - The hash of the tag
    /// * `expected_state` - The expected Merkle state
    /// * `actual_state` - The actual stack state
    pub fn tag_state_mismatch(
        tag_hash: Hash,
        expected_state: Merkle,
        actual_state: Merkle,
    ) -> Self {
        Self::TagStateMismatch {
            tag_hash,
            expected_state,
            actual_state,
        }
    }

    /// Create a new tag-not-registered error.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the unregistered tag
    pub fn tag_not_registered(hash: Hash) -> Self {
        Self::TagNotRegistered { hash }
    }

    /// Create a new block-not-found error.
    ///
    /// # Arguments
    ///
    /// * `position` - The position that couldn't be found
    pub fn block_not_found(position: Position<NodeId>) -> Self {
        Self::BlockNotFound { position }
    }

    /// Create a new node-not-found error.
    ///
    /// # Arguments
    ///
    /// * `node_id` - The internal node ID that's missing
    pub fn node_not_found(node_id: NodeId) -> Self {
        Self::NodeNotFound { node_id }
    }

    /// Create a new cyclic-dependency error.
    ///
    /// # Arguments
    ///
    /// * `message` - Description of the cycle
    pub fn cyclic_dependency(message: impl Into<String>) -> Self {
        Self::CyclicDependency {
            message: message.into(),
        }
    }

    /// Create a new inconsistent-edge error.
    ///
    /// # Arguments
    ///
    /// * `message` - Description of the inconsistency
    pub fn inconsistent_edge(message: impl Into<String>) -> Self {
        Self::InconsistentEdge {
            message: message.into(),
        }
    }

    /// Create a new missing-context error.
    ///
    /// # Arguments
    ///
    /// * `position` - The position of the missing context
    pub fn missing_context(position: Position<NodeId>) -> Self {
        Self::MissingContext { position }
    }

    /// Create a new internal error.
    ///
    /// # Arguments
    ///
    /// * `message` - Description of the internal error
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }

    /// Check if this error indicates a missing dependency.
    ///
    /// # Returns
    ///
    /// `true` if the error is about a missing dependency.
    pub fn is_dependency_error(&self) -> bool {
        matches!(
            self,
            Self::DependencyMissing { .. }
                | Self::NodeNotFound { .. }
                | Self::MissingContext { .. }
        )
    }

    /// Check if this error indicates the change was already applied.
    ///
    /// # Returns
    ///
    /// `true` if the change or tag was already applied.
    pub fn is_already_applied(&self) -> bool {
        matches!(
            self,
            Self::ChangeAlreadyApplied { .. } | Self::TagAlreadyApplied { .. }
        )
    }

    /// Check if this error indicates data corruption.
    ///
    /// # Returns
    ///
    /// `true` if the error suggests repository corruption.
    pub fn is_corruption(&self) -> bool {
        matches!(
            self,
            Self::Corruption | Self::BlockNotFound { .. } | Self::InconsistentEdge { .. }
        )
    }

    /// Check if this error is recoverable.
    ///
    /// Recoverable errors are those where the operation might succeed
    /// if dependencies are resolved or the state changes.
    ///
    /// # Returns
    ///
    /// `true` if the error might be recoverable.
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::DependencyMissing { .. }
                | Self::ChangeAlreadyApplied { .. }
                | Self::TagAlreadyApplied { .. }
                | Self::TagStateMismatch { .. }
        )
    }
}

/// High-level errors that can occur during change application.
///
/// This enum wraps both storage-level errors ([`PristineError`]) and
/// logic-level errors ([`LocalApplyError`]), providing a unified error
/// type for the apply API.
///
/// # Example
///
/// ```rust
/// use atomic_core::apply::ApplyError;
/// use atomic_core::pristine::PristineError;
///
/// fn is_storage_issue(err: &ApplyError) -> bool {
///     matches!(err, ApplyError::Pristine(_) | ApplyError::Io(_))
/// }
/// ```
#[derive(Debug, Error)]
pub enum ApplyError {
    /// An error in the apply logic itself.
    ///
    /// This wraps [`LocalApplyError`] for errors specific to the
    /// change application process.
    #[error("Apply error: {0}")]
    Local(#[from] LocalApplyError),

    /// A database/pristine storage error.
    ///
    /// This indicates a problem with the underlying storage layer.
    #[error("Pristine error: {0}")]
    Pristine(#[from] PristineError),

    /// An IO error occurred.
    ///
    /// This typically happens when reading change files from disk.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Error reading or parsing the change data.
    ///
    /// This occurs when the change file is corrupted or in an
    /// unrecognized format.
    ///
    /// # Fields
    ///
    /// * `message` - Description of the parse error
    #[error("Change parse error: {message}")]
    ChangeParse {
        /// What went wrong during parsing
        message: String,
    },

    /// The change file was not found.
    ///
    /// # Fields
    ///
    /// * `hash` - The hash of the change that's missing
    /// * `path` - The expected path where the change file should be
    #[error("Change file not found for {hash} at {}", path.display())]
    ChangeNotFound {
        /// The hash of the missing change
        hash: Hash,
        /// Where we looked for it
        path: PathBuf,
    },
}

impl ApplyError {
    /// Create a new change-parse error.
    ///
    /// # Arguments
    ///
    /// * `message` - Description of the parse error
    pub fn change_parse(message: impl Into<String>) -> Self {
        Self::ChangeParse {
            message: message.into(),
        }
    }

    /// Create a new change-not-found error.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the missing change
    /// * `path` - The expected path
    pub fn change_not_found(hash: Hash, path: impl Into<PathBuf>) -> Self {
        Self::ChangeNotFound {
            hash,
            path: path.into(),
        }
    }

    /// Check if this is a local apply error.
    ///
    /// # Returns
    ///
    /// `true` if this wraps a `LocalApplyError`.
    pub fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// Check if this is a storage error.
    ///
    /// # Returns
    ///
    /// `true` if this is a storage-related error.
    pub fn is_storage(&self) -> bool {
        matches!(self, Self::Pristine(_) | Self::Io(_))
    }

    /// Check if this error indicates the change was already applied.
    ///
    /// # Returns
    ///
    /// `true` if the underlying error is about an already-applied change.
    pub fn is_already_applied(&self) -> bool {
        matches!(
            self,
            Self::Local(LocalApplyError::ChangeAlreadyApplied { .. })
                | Self::Local(LocalApplyError::TagAlreadyApplied { .. })
        )
    }

    /// Check if this error indicates a missing dependency.
    ///
    /// # Returns
    ///
    /// `true` if the underlying error is about a missing dependency.
    pub fn is_dependency_missing(&self) -> bool {
        matches!(self, Self::Local(LocalApplyError::DependencyMissing { .. }))
    }

    /// Get the underlying local error, if any.
    ///
    /// # Returns
    ///
    /// `Some(&LocalApplyError)` if this is a `Local` variant.
    pub fn as_local(&self) -> Option<&LocalApplyError> {
        match self {
            Self::Local(e) => Some(e),
            _ => None,
        }
    }

    /// Check if this error is recoverable.
    ///
    /// # Returns
    ///
    /// `true` if the operation might succeed after resolving dependencies.
    pub fn is_recoverable(&self) -> bool {
        match self {
            Self::Local(e) => e.is_recoverable(),
            Self::ChangeNotFound { .. } => true, // Might be fetched
            _ => false,
        }
    }
}

/// Result type alias for apply operations.
///
/// This is a convenience type for functions that return `Result<T, ApplyError>`.
///
/// # Example
///
/// ```rust
/// use atomic_core::apply::{ApplyResult, ApplyError};
///
/// fn my_apply_function() -> ApplyResult<()> {
///     // ... do apply work ...
///     Ok(())
/// }
/// ```
pub type ApplyResult<T> = Result<T, ApplyError>;

/// Result type alias for local apply operations.
///
/// This is for internal functions that only produce [`LocalApplyError`].
pub type LocalApplyResult<T> = Result<T, LocalApplyError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    fn hash_a() -> Hash {
        Hash::of(b"change A")
    }

    #[allow(dead_code)]
    fn hash_b() -> Hash {
        Hash::of(b"change B")
    }

    fn some_position() -> Position<NodeId> {
        Position::new(NodeId::new(42), ChangePosition::new(100))
    }

    // -- Classification logic: the interesting behavior of these error types
    //    is how they're bucketed for retry/recovery decisions in the apply loop.

    #[test]
    fn dependency_errors_are_the_union_of_missing_dep_missing_node_missing_context() {
        let dep = LocalApplyError::dependency_missing(hash_a());
        let node = LocalApplyError::node_not_found(NodeId::new(9));
        let ctx = LocalApplyError::missing_context(some_position());

        for err in [&dep, &node, &ctx] {
            assert!(
                err.is_dependency_error(),
                "{err} should be a dependency error"
            );
        }

        // Everything else must NOT be classified as dependency.
        let non_dep = [
            LocalApplyError::change_already_applied(hash_a()),
            LocalApplyError::InvalidChange,
            LocalApplyError::Corruption,
            LocalApplyError::internal("bug"),
            LocalApplyError::cyclic_dependency("A->B->A"),
            LocalApplyError::block_not_found(some_position()),
        ];
        for err in &non_dep {
            assert!(
                !err.is_dependency_error(),
                "{err} should NOT be a dependency error"
            );
        }
    }

    #[test]
    fn corruption_classification_catches_graph_integrity_failures() {
        // These indicate the on-disk graph is broken — not user error.
        assert!(LocalApplyError::Corruption.is_corruption());
        assert!(LocalApplyError::block_not_found(some_position()).is_corruption());
        assert!(LocalApplyError::inconsistent_edge("dangling").is_corruption());

        // Dependency issues are NOT corruption — the graph is fine, just incomplete.
        assert!(!LocalApplyError::dependency_missing(hash_a()).is_corruption());
        assert!(!LocalApplyError::node_not_found(NodeId::new(1)).is_corruption());
    }

    #[test]
    fn recoverable_errors_are_exactly_the_ones_a_sync_loop_should_retry() {
        // Recoverable: fetch the dep / skip the dup / wait for state convergence.
        let recoverable = [
            LocalApplyError::dependency_missing(hash_a()),
            LocalApplyError::change_already_applied(hash_a()),
            LocalApplyError::tag_already_applied(hash_a()),
            LocalApplyError::tag_state_mismatch(hash_a(), Merkle::of(b"x"), Merkle::of(b"y")),
        ];
        for err in &recoverable {
            assert!(err.is_recoverable(), "{err} should be recoverable");
        }

        // NOT recoverable: corruption, bad format, internal bugs.
        let fatal = [
            LocalApplyError::Corruption,
            LocalApplyError::InvalidChange,
            LocalApplyError::internal("assertion failed"),
            LocalApplyError::block_not_found(some_position()),
            LocalApplyError::cyclic_dependency("A->A"),
        ];
        for err in &fatal {
            assert!(
                !err.is_recoverable(),
                "{err} should be fatal (not recoverable)"
            );
        }
    }

    #[test]
    fn already_applied_is_idempotency_guard() {
        assert!(LocalApplyError::change_already_applied(hash_a()).is_already_applied());
        assert!(LocalApplyError::tag_already_applied(hash_a()).is_already_applied());

        // Dependency errors are a different category — don't conflate them.
        assert!(!LocalApplyError::dependency_missing(hash_a()).is_already_applied());
    }

    // -- ApplyError wrapping: verify the high-level type correctly delegates.

    #[test]
    fn apply_error_delegates_classification_through_local_wrapper() {
        let dep: ApplyError = LocalApplyError::dependency_missing(hash_a()).into();
        let dup: ApplyError = LocalApplyError::change_already_applied(hash_a()).into();
        let parse = ApplyError::change_parse("bad header");

        assert!(dep.is_dependency_missing());
        assert!(dep.is_local());
        assert!(dep.is_recoverable());

        assert!(dup.is_already_applied());
        assert!(dup.is_recoverable());

        assert!(!parse.is_local());
        assert!(!parse.is_recoverable());
        assert!(parse.as_local().is_none());
    }

    #[test]
    fn storage_errors_are_pristine_or_io() {
        let pristine: ApplyError = PristineError::ViewNotFound {
            name: "gone".into(),
        }
        .into();
        let io: ApplyError = std::io::Error::new(std::io::ErrorKind::NotFound, "vanished").into();

        assert!(pristine.is_storage());
        assert!(io.is_storage());

        // Local and parse errors are NOT storage.
        assert!(!ApplyError::change_parse("x").is_storage());
        let local: ApplyError = LocalApplyError::Corruption.into();
        assert!(!local.is_storage());
    }

    #[test]
    fn change_not_found_is_recoverable_because_it_might_be_fetched() {
        let err = ApplyError::change_not_found(hash_a(), PathBuf::from("/changes/AB/CD"));
        assert!(err.is_recoverable());

        // But a parse error means the file IS there and is broken — not recoverable.
        assert!(!ApplyError::change_parse("truncated").is_recoverable());
    }

    #[test]
    fn as_local_extracts_inner_error_for_detailed_matching() {
        let inner = LocalApplyError::cyclic_dependency("A -> B -> A");
        let outer: ApplyError = inner.into();

        let extracted = outer.as_local().expect("should unwrap Local variant");
        assert!(matches!(
            extracted,
            LocalApplyError::CyclicDependency { .. }
        ));
    }

    // -- Error propagation: the ? operator must work across the error hierarchy.

    #[test]
    fn question_mark_chains_local_through_apply() {
        fn step_one() -> LocalApplyResult<()> {
            Err(LocalApplyError::InvalidChange)
        }

        fn step_two() -> ApplyResult<()> {
            step_one()?; // LocalApplyError -> ApplyError via From
            Ok(())
        }

        let result = step_two();
        assert!(matches!(
            result,
            Err(ApplyError::Local(LocalApplyError::InvalidChange))
        ));
    }

    #[test]
    fn question_mark_chains_io_through_apply() {
        fn read_change() -> ApplyResult<Vec<u8>> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file",
            ))?
        }

        assert!(matches!(read_change(), Err(ApplyError::Io(_))));
    }

    // -- Display: verify that error messages carry enough context for debugging.
    //    We test the display contract, not the exact wording.

    #[test]
    fn tag_state_mismatch_display_shows_both_states() {
        let expected = Merkle::of(b"expected state");
        let actual = Merkle::of(b"actual state");
        let err = LocalApplyError::tag_state_mismatch(hash_a(), expected, actual);

        let msg = err.to_string();
        // The message must help someone figure out WHY the tag didn't apply.
        assert!(
            msg.contains("expected"),
            "should mention expected state: {msg}"
        );
        assert!(msg.contains("got"), "should mention actual state: {msg}");
    }

    #[test]
    fn change_not_found_display_includes_path() {
        let err =
            ApplyError::change_not_found(hash_a(), PathBuf::from("/repo/.atomic/changes/AB/CDEF"));
        let msg = err.to_string();
        assert!(
            msg.contains("changes/AB/CDEF"),
            "should show where we looked: {msg}"
        );
    }

    #[test]
    fn local_errors_surface_through_apply_display() {
        let err: ApplyError = LocalApplyError::dependency_missing(hash_a()).into();
        let msg = err.to_string();
        // The wrapping layer shouldn't swallow the inner message.
        assert!(
            msg.contains("Dependency missing"),
            "inner message lost: {msg}"
        );
    }
}

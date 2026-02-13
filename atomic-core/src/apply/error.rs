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
    #[error(
        "Tag state mismatch for {tag_hash}: expected {expected_state}, got {actual_state}"
    )]
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
    pub fn tag_state_mismatch(tag_hash: Hash, expected_state: Merkle, actual_state: Merkle) -> Self {
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

    // =========================================================================
    // Helper Functions
    // =========================================================================

    fn test_hash() -> Hash {
        Hash::of(b"test hash data")
    }

    fn test_merkle() -> Merkle {
        Merkle::of(b"test merkle data")
    }

    fn test_position() -> Position<NodeId> {
        Position::new(NodeId::new(42), ChangePosition::new(100))
    }

    fn test_node_id() -> NodeId {
        NodeId::new(123)
    }

    // =========================================================================
    // LocalApplyError Construction Tests
    // =========================================================================

    #[test]
    fn test_dependency_missing_construction() {
        let hash = test_hash();
        let err = LocalApplyError::dependency_missing(hash);
        assert!(matches!(err, LocalApplyError::DependencyMissing { hash: h } if h == hash));
    }

    #[test]
    fn test_change_already_applied_construction() {
        let hash = test_hash();
        let err = LocalApplyError::change_already_applied(hash);
        assert!(matches!(err, LocalApplyError::ChangeAlreadyApplied { hash: h } if h == hash));
    }

    #[test]
    fn test_tag_already_applied_construction() {
        let hash = test_hash();
        let err = LocalApplyError::tag_already_applied(hash);
        assert!(matches!(err, LocalApplyError::TagAlreadyApplied { hash: h } if h == hash));
    }

    #[test]
    fn test_tag_state_mismatch_construction() {
        let tag_hash = test_hash();
        let expected = test_merkle();
        let actual = Merkle::of(b"different state");
        let err = LocalApplyError::tag_state_mismatch(tag_hash, expected, actual);

        match err {
            LocalApplyError::TagStateMismatch {
                tag_hash: h,
                expected_state: e,
                actual_state: a,
            } => {
                assert_eq!(h, tag_hash);
                assert_eq!(e, expected);
                assert_eq!(a, actual);
            }
            _ => panic!("Expected TagStateMismatch"),
        }
    }

    #[test]
    fn test_tag_not_registered_construction() {
        let hash = test_hash();
        let err = LocalApplyError::tag_not_registered(hash);
        assert!(matches!(err, LocalApplyError::TagNotRegistered { hash: h } if h == hash));
    }

    #[test]
    fn test_block_not_found_construction() {
        let pos = test_position();
        let err = LocalApplyError::block_not_found(pos);
        assert!(matches!(err, LocalApplyError::BlockNotFound { position: p } if p == pos));
    }

    #[test]
    fn test_node_not_found_construction() {
        let node_id = test_node_id();
        let err = LocalApplyError::node_not_found(node_id);
        assert!(matches!(err, LocalApplyError::NodeNotFound { node_id: n } if n == node_id));
    }

    #[test]
    fn test_cyclic_dependency_construction() {
        let err = LocalApplyError::cyclic_dependency("A -> B -> A");
        assert!(
            matches!(err, LocalApplyError::CyclicDependency { message } if message == "A -> B -> A")
        );
    }

    #[test]
    fn test_inconsistent_edge_construction() {
        let err = LocalApplyError::inconsistent_edge("edge to deleted node");
        assert!(matches!(
            err,
            LocalApplyError::InconsistentEdge { message } if message == "edge to deleted node"
        ));
    }

    #[test]
    fn test_missing_context_construction() {
        let pos = test_position();
        let err = LocalApplyError::missing_context(pos);
        assert!(matches!(err, LocalApplyError::MissingContext { position: p } if p == pos));
    }

    #[test]
    fn test_internal_construction() {
        let err = LocalApplyError::internal("unexpected state");
        assert!(
            matches!(err, LocalApplyError::Internal { message } if message == "unexpected state")
        );
    }

    #[test]
    fn test_invalid_change_construction() {
        let err = LocalApplyError::InvalidChange;
        assert!(matches!(err, LocalApplyError::InvalidChange));
    }

    #[test]
    fn test_corruption_construction() {
        let err = LocalApplyError::Corruption;
        assert!(matches!(err, LocalApplyError::Corruption));
    }

    // =========================================================================
    // LocalApplyError Classification Tests
    // =========================================================================

    #[test]
    fn test_is_dependency_error() {
        assert!(LocalApplyError::dependency_missing(test_hash()).is_dependency_error());
        assert!(LocalApplyError::node_not_found(test_node_id()).is_dependency_error());
        assert!(LocalApplyError::missing_context(test_position()).is_dependency_error());

        // Non-dependency errors
        assert!(!LocalApplyError::change_already_applied(test_hash()).is_dependency_error());
        assert!(!LocalApplyError::InvalidChange.is_dependency_error());
        assert!(!LocalApplyError::Corruption.is_dependency_error());
    }

    #[test]
    fn test_is_already_applied() {
        assert!(LocalApplyError::change_already_applied(test_hash()).is_already_applied());
        assert!(LocalApplyError::tag_already_applied(test_hash()).is_already_applied());

        // Not already-applied errors
        assert!(!LocalApplyError::dependency_missing(test_hash()).is_already_applied());
        assert!(!LocalApplyError::InvalidChange.is_already_applied());
    }

    #[test]
    fn test_is_corruption() {
        assert!(LocalApplyError::Corruption.is_corruption());
        assert!(LocalApplyError::block_not_found(test_position()).is_corruption());
        assert!(LocalApplyError::inconsistent_edge("x").is_corruption());

        // Non-corruption errors
        assert!(!LocalApplyError::dependency_missing(test_hash()).is_corruption());
        assert!(!LocalApplyError::InvalidChange.is_corruption());
    }

    #[test]
    fn test_local_is_recoverable() {
        // Recoverable
        assert!(LocalApplyError::dependency_missing(test_hash()).is_recoverable());
        assert!(LocalApplyError::change_already_applied(test_hash()).is_recoverable());
        assert!(LocalApplyError::tag_already_applied(test_hash()).is_recoverable());
        assert!(LocalApplyError::tag_state_mismatch(
            test_hash(),
            test_merkle(),
            test_merkle()
        )
        .is_recoverable());

        // Not recoverable
        assert!(!LocalApplyError::Corruption.is_recoverable());
        assert!(!LocalApplyError::InvalidChange.is_recoverable());
        assert!(!LocalApplyError::internal("bug").is_recoverable());
    }

    // =========================================================================
    // LocalApplyError Display Tests
    // =========================================================================

    #[test]
    fn test_display_dependency_missing() {
        let err = LocalApplyError::dependency_missing(test_hash());
        let display = format!("{}", err);
        assert!(display.contains("Dependency missing"));
    }

    #[test]
    fn test_display_change_already_applied() {
        let err = LocalApplyError::change_already_applied(test_hash());
        let display = format!("{}", err);
        assert!(display.contains("Change already applied"));
    }

    #[test]
    fn test_display_tag_state_mismatch() {
        let err = LocalApplyError::tag_state_mismatch(
            test_hash(),
            Merkle::of(b"expected"),
            Merkle::of(b"actual"),
        );
        let display = format!("{}", err);
        assert!(display.contains("Tag state mismatch"));
        assert!(display.contains("expected"));
        assert!(display.contains("got"));
    }

    #[test]
    fn test_display_block_not_found() {
        let err = LocalApplyError::block_not_found(test_position());
        let display = format!("{}", err);
        assert!(display.contains("Block not found"));
    }

    #[test]
    fn test_display_corruption() {
        let err = LocalApplyError::Corruption;
        let display = format!("{}", err);
        assert!(display.contains("corruption"));
    }

    #[test]
    fn test_display_cyclic_dependency() {
        let err = LocalApplyError::cyclic_dependency("A -> B -> C -> A");
        let display = format!("{}", err);
        assert!(display.contains("Cyclic dependency"));
        assert!(display.contains("A -> B -> C -> A"));
    }

    // =========================================================================
    // ApplyError Construction Tests
    // =========================================================================

    #[test]
    fn test_change_parse_construction() {
        let err = ApplyError::change_parse("invalid header");
        assert!(
            matches!(err, ApplyError::ChangeParse { message } if message == "invalid header")
        );
    }

    #[test]
    fn test_change_not_found_construction() {
        let hash = test_hash();
        let err = ApplyError::change_not_found(hash, PathBuf::from("/changes/AB/CDEF"));
        match err {
            ApplyError::ChangeNotFound { hash: h, path: p } => {
                assert_eq!(h, hash);
                assert_eq!(p, PathBuf::from("/changes/AB/CDEF"));
            }
            _ => panic!("Expected ChangeNotFound"),
        }
    }

    // =========================================================================
    // ApplyError Classification Tests
    // =========================================================================

    #[test]
    fn test_apply_is_local() {
        let local_err = LocalApplyError::dependency_missing(test_hash());
        let err: ApplyError = local_err.into();
        assert!(err.is_local());

        let err = ApplyError::change_parse("test");
        assert!(!err.is_local());
    }

    #[test]
    fn test_apply_is_storage() {
        let pristine_err = PristineError::StackNotFound {
            name: "test".to_string(),
        };
        let err: ApplyError = pristine_err.into();
        assert!(err.is_storage());

        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "test");
        let err: ApplyError = io_err.into();
        assert!(err.is_storage());

        let err = ApplyError::change_parse("test");
        assert!(!err.is_storage());
    }

    #[test]
    fn test_apply_is_already_applied() {
        let local_err = LocalApplyError::change_already_applied(test_hash());
        let err: ApplyError = local_err.into();
        assert!(err.is_already_applied());

        let local_err = LocalApplyError::tag_already_applied(test_hash());
        let err: ApplyError = local_err.into();
        assert!(err.is_already_applied());

        let err = ApplyError::change_parse("test");
        assert!(!err.is_already_applied());
    }

    #[test]
    fn test_apply_is_dependency_missing() {
        let local_err = LocalApplyError::dependency_missing(test_hash());
        let err: ApplyError = local_err.into();
        assert!(err.is_dependency_missing());

        let err = ApplyError::change_parse("test");
        assert!(!err.is_dependency_missing());
    }

    #[test]
    fn test_apply_as_local() {
        let local_err = LocalApplyError::dependency_missing(test_hash());
        let err: ApplyError = local_err.into();
        assert!(err.as_local().is_some());

        let err = ApplyError::change_parse("test");
        assert!(err.as_local().is_none());
    }

    #[test]
    fn test_apply_is_recoverable() {
        // Recoverable local errors
        let local_err = LocalApplyError::dependency_missing(test_hash());
        let err: ApplyError = local_err.into();
        assert!(err.is_recoverable());

        // ChangeNotFound is recoverable (might be fetched)
        let err = ApplyError::change_not_found(test_hash(), PathBuf::from("/test"));
        assert!(err.is_recoverable());

        // Non-recoverable
        let err = ApplyError::change_parse("corrupted");
        assert!(!err.is_recoverable());
    }

    // =========================================================================
    // ApplyError Display Tests
    // =========================================================================

    #[test]
    fn test_display_apply_local() {
        let local_err = LocalApplyError::dependency_missing(test_hash());
        let err: ApplyError = local_err.into();
        let display = format!("{}", err);
        assert!(display.contains("Apply error"));
        assert!(display.contains("Dependency missing"));
    }

    #[test]
    fn test_display_apply_change_parse() {
        let err = ApplyError::change_parse("invalid format");
        let display = format!("{}", err);
        assert!(display.contains("Change parse error"));
        assert!(display.contains("invalid format"));
    }

    #[test]
    fn test_display_apply_change_not_found() {
        let err = ApplyError::change_not_found(test_hash(), PathBuf::from("/changes/AB/CDEF"));
        let display = format!("{}", err);
        assert!(display.contains("Change file not found"));
        assert!(display.contains("changes/AB/CDEF"));
    }

    // =========================================================================
    // ApplyError From Trait Tests
    // =========================================================================

    #[test]
    fn test_from_local_apply_error() {
        let local_err = LocalApplyError::InvalidChange;
        let err: ApplyError = local_err.into();
        assert!(matches!(err, ApplyError::Local(LocalApplyError::InvalidChange)));
    }

    #[test]
    fn test_from_pristine_error() {
        let pristine_err = PristineError::StackNotFound {
            name: "test".to_string(),
        };
        let err: ApplyError = pristine_err.into();
        assert!(matches!(err, ApplyError::Pristine(_)));
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err: ApplyError = io_err.into();
        assert!(matches!(err, ApplyError::Io(_)));
    }

    // =========================================================================
    // Result Type Tests
    // =========================================================================

    #[test]
    fn test_apply_result_ok() {
        let result: ApplyResult<i32> = Ok(42);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_apply_result_err() {
        let result: ApplyResult<i32> = Err(ApplyError::change_parse("test"));
        assert!(result.is_err());
    }

    #[test]
    fn test_local_apply_result_ok() {
        let result: LocalApplyResult<String> = Ok("success".to_string());
        assert!(result.is_ok());
    }

    #[test]
    fn test_local_apply_result_err() {
        let result: LocalApplyResult<String> = Err(LocalApplyError::Corruption);
        assert!(result.is_err());
    }

    #[test]
    fn test_question_mark_propagation() {
        fn inner() -> LocalApplyResult<i32> {
            Err(LocalApplyError::InvalidChange)
        }

        fn outer() -> ApplyResult<i32> {
            let _value = inner()?;
            Ok(42)
        }

        let result = outer();
        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(ApplyError::Local(LocalApplyError::InvalidChange))
        ));
    }

    // =========================================================================
    // Edge Case Tests
    // =========================================================================

    #[test]
    fn test_empty_message() {
        let err = LocalApplyError::internal("");
        assert!(matches!(err, LocalApplyError::Internal { message } if message.is_empty()));
    }

    #[test]
    fn test_unicode_in_message() {
        let err = LocalApplyError::cyclic_dependency("循环依赖: A → B → A");
        let display = format!("{}", err);
        assert!(display.contains("循环依赖"));
    }

    #[test]
    fn test_debug_format() {
        let err = LocalApplyError::dependency_missing(test_hash());
        let debug = format!("{:?}", err);
        assert!(debug.contains("DependencyMissing"));
    }

    #[test]
    fn test_apply_debug_format() {
        let err = ApplyError::change_parse("test error");
        let debug = format!("{:?}", err);
        assert!(debug.contains("ChangeParse"));
    }
}

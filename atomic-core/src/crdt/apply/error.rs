//! Error types for CRDT apply operations.
//!
//! This module defines the error types that can occur when applying CRDT
//! operations (TrunkOp, BranchOp, LeafOp) to the pristine database.
//!
//! # Error Categories
//!
//! Errors are organized into categories for easier handling:
//!
//! - **Not Found**: Referenced entities don't exist
//! - **Already Exists**: Duplicate creation attempts
//! - **Invalid State**: Operations on entities in wrong state
//! - **Ordering**: Failures in CRDT ordering resolution
//! - **Storage**: Database-level failures
//!
//! # Example
//!
//! ```rust
//! use atomic_core::crdt::apply::error::{ApplyError, ApplyResult};
//! use atomic_core::crdt::TrunkId;
//! use atomic_core::types::NodeId;
//!
//! fn example_operation() -> ApplyResult<()> {
//!     let trunk_id = TrunkId::new(NodeId::new(1), 0);
//!
//!     // Simulate a not-found error
//!     if true {
//!         return Err(ApplyError::trunk_not_found(trunk_id));
//!     }
//!
//!     Ok(())
//! }
//!
//! let result = example_operation();
//! assert!(result.is_err());
//! if let Err(e) = result {
//!     assert!(e.is_not_found());
//!     assert!(e.suggestion().contains("verify"));
//! }
//! ```

use crate::crdt::{BranchId, LeafId, TrunkId};
use crate::pristine::PristineError;
#[allow(unused_imports)]
use std::error::Error;
use std::fmt;

// Helper Functions

/// Converts a MutCrdtTxnT error to an ApplyError with context.
///
/// This helper function simplifies error handling in apply operations by
/// converting any error type that implements `Into<PristineError>` into
/// an `ApplyError::Storage` variant with the given context.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::crdt::apply::error::storage_err;
///
/// let result = txn.get_trunk(id).map_err(|e| storage_err(e, "getting trunk"))?;
/// ```
#[inline]
pub fn storage_err<E: Into<PristineError>>(err: E, context: &str) -> ApplyError {
    ApplyError::Storage {
        source: Box::new(err.into()),
        context: context.to_string(),
    }
}

// ApplyResult Type Alias

/// Result type for CRDT apply operations.
///
/// This is the standard result type returned by all apply functions.
pub type ApplyResult<T> = Result<T, ApplyError>;

// ApplyError

/// Errors that can occur when applying CRDT operations.
///
/// This enum covers all failure modes during CRDT operation application,
/// from missing entities to database errors.
///
/// # Error Classification
///
/// Use the classification methods to handle errors appropriately:
///
/// - [`is_not_found()`](ApplyError::is_not_found) - Entity doesn't exist
/// - [`is_already_exists()`](ApplyError::is_already_exists) - Duplicate creation
/// - [`is_invalid_state()`](ApplyError::is_invalid_state) - Wrong entity state
/// - [`is_ordering_error()`](ApplyError::is_ordering_error) - CRDT ordering failure
/// - [`is_storage_error()`](ApplyError::is_storage_error) - Database failure
/// - [`is_recoverable()`](ApplyError::is_recoverable) - Can retry or continue
#[derive(Debug)]
pub enum ApplyError {
    // Not Found Errors
    /// Referenced trunk (file) does not exist.
    ///
    /// This occurs when an operation references a TrunkId that hasn't been
    /// created or has been permanently removed.
    TrunkNotFound {
        /// The trunk ID that was not found.
        trunk_id: TrunkId,
    },

    /// Referenced branch (line) does not exist.
    ///
    /// This occurs when an operation references a BranchId that hasn't been
    /// created or has been permanently removed.
    BranchNotFound {
        /// The branch ID that was not found.
        branch_id: BranchId,
    },

    /// Referenced leaf (token) does not exist.
    ///
    /// This occurs when an operation references a LeafId that hasn't been
    /// created or has been permanently removed.
    LeafNotFound {
        /// The leaf ID that was not found.
        leaf_id: LeafId,
    },

    /// File path does not exist in the repository.
    ///
    /// This occurs when trying to operate on a file by path that doesn't
    /// exist in the PATH_TRUNK lookup table.
    PathNotFound {
        /// The path that was not found.
        path: String,
    },

    // Already Exists Errors
    /// Trunk with this ID already exists.
    ///
    /// This occurs when attempting to create a trunk with an ID that's
    /// already in use. CRDT IDs must be globally unique.
    TrunkAlreadyExists {
        /// The trunk ID that already exists.
        trunk_id: TrunkId,
    },

    /// Branch with this ID already exists.
    ///
    /// This occurs when attempting to create a branch with an ID that's
    /// already in use. CRDT IDs must be globally unique.
    BranchAlreadyExists {
        /// The branch ID that already exists.
        branch_id: BranchId,
    },

    /// Leaf with this ID already exists.
    ///
    /// This occurs when attempting to create a leaf with an ID that's
    /// already in use. CRDT IDs must be globally unique.
    LeafAlreadyExists {
        /// The leaf ID that already exists.
        leaf_id: LeafId,
    },

    /// File path already exists (for file creation).
    ///
    /// This occurs when attempting to create a file at a path that's
    /// already occupied by another file.
    PathAlreadyExists {
        /// The path that already exists.
        path: String,
        /// The existing trunk at that path.
        existing_trunk: TrunkId,
    },

    // Invalid State Errors
    /// Trunk is in an invalid state for the requested operation.
    ///
    /// For example, trying to delete an already-deleted trunk, or
    /// trying to undelete a trunk that isn't deleted.
    InvalidTrunkState {
        /// The trunk ID.
        trunk_id: TrunkId,
        /// The current state of the trunk.
        current_state: String,
        /// The operation that was attempted.
        operation: String,
    },

    /// Branch is in an invalid state for the requested operation.
    ///
    /// For example, trying to restore a branch that isn't deleted.
    InvalidBranchState {
        /// The branch ID.
        branch_id: BranchId,
        /// The current state of the branch.
        current_state: String,
        /// The operation that was attempted.
        operation: String,
    },

    /// Leaf is in an invalid state for the requested operation.
    ///
    /// For example, trying to replace content in a deleted leaf.
    InvalidLeafState {
        /// The leaf ID.
        leaf_id: LeafId,
        /// The current state of the leaf.
        current_state: String,
        /// The operation that was attempted.
        operation: String,
    },

    // Ordering Errors
    /// Failed to find a valid insertion position.
    ///
    /// This occurs when the CRDT ordering algorithm cannot determine
    /// where to insert a new element. This typically indicates a bug
    /// or corrupted ordering data.
    InsertionPositionNotFound {
        /// Description of what was being inserted.
        description: String,
        /// The "after" reference that couldn't be resolved.
        after_ref: String,
    },

    /// Circular reference detected in ordering.
    ///
    /// This should never happen in a well-formed CRDT graph and
    /// indicates data corruption.
    CircularReference {
        /// Description of where the cycle was detected.
        description: String,
    },

    /// Ordering constraint violation.
    ///
    /// The requested operation would violate CRDT ordering invariants.
    OrderingViolation {
        /// Description of the violation.
        description: String,
    },

    // Content Errors
    /// Content range is out of bounds.
    ///
    /// The specified content range doesn't fit within the content blob.
    ContentOutOfBounds {
        /// The requested start position.
        start: usize,
        /// The requested end position.
        end: usize,
        /// The actual content length.
        content_len: usize,
    },

    /// Content is invalid (e.g., not valid UTF-8 for a text file).
    InvalidContent {
        /// Description of what's invalid.
        description: String,
    },

    // Storage Errors
    /// Database operation failed.
    ///
    /// This wraps errors from the underlying pristine storage layer.
    Storage {
        /// The underlying storage error.
        source: Box<PristineError>,
        /// Context about what operation was being performed.
        context: String,
    },

    // Internal Errors
    /// An internal invariant was violated.
    ///
    /// This indicates a bug in the apply code and should be reported.
    InternalError {
        /// Description of the internal error.
        description: String,
    },
}

impl ApplyError {
    // Constructors

    /// Creates a `TrunkNotFound` error.
    #[inline]
    pub fn trunk_not_found(trunk_id: TrunkId) -> Self {
        ApplyError::TrunkNotFound { trunk_id }
    }

    /// Creates a `BranchNotFound` error.
    #[inline]
    pub fn branch_not_found(branch_id: BranchId) -> Self {
        ApplyError::BranchNotFound { branch_id }
    }

    /// Creates a `LeafNotFound` error.
    #[inline]
    pub fn leaf_not_found(leaf_id: LeafId) -> Self {
        ApplyError::LeafNotFound { leaf_id }
    }

    /// Creates a `PathNotFound` error.
    #[inline]
    pub fn path_not_found(path: impl Into<String>) -> Self {
        ApplyError::PathNotFound { path: path.into() }
    }

    /// Creates a `TrunkAlreadyExists` error.
    #[inline]
    pub fn trunk_already_exists(trunk_id: TrunkId) -> Self {
        ApplyError::TrunkAlreadyExists { trunk_id }
    }

    /// Creates a `BranchAlreadyExists` error.
    #[inline]
    pub fn branch_already_exists(branch_id: BranchId) -> Self {
        ApplyError::BranchAlreadyExists { branch_id }
    }

    /// Creates a `LeafAlreadyExists` error.
    #[inline]
    pub fn leaf_already_exists(leaf_id: LeafId) -> Self {
        ApplyError::LeafAlreadyExists { leaf_id }
    }

    /// Creates a `PathAlreadyExists` error.
    #[inline]
    pub fn path_already_exists(path: impl Into<String>, existing_trunk: TrunkId) -> Self {
        ApplyError::PathAlreadyExists {
            path: path.into(),
            existing_trunk,
        }
    }

    /// Creates an `InvalidTrunkState` error.
    #[inline]
    pub fn invalid_trunk_state(
        trunk_id: TrunkId,
        current_state: impl Into<String>,
        operation: impl Into<String>,
    ) -> Self {
        ApplyError::InvalidTrunkState {
            trunk_id,
            current_state: current_state.into(),
            operation: operation.into(),
        }
    }

    /// Creates an `InvalidBranchState` error.
    #[inline]
    pub fn invalid_branch_state(
        branch_id: BranchId,
        current_state: impl Into<String>,
        operation: impl Into<String>,
    ) -> Self {
        ApplyError::InvalidBranchState {
            branch_id,
            current_state: current_state.into(),
            operation: operation.into(),
        }
    }

    /// Creates an `InvalidLeafState` error.
    #[inline]
    pub fn invalid_leaf_state(
        leaf_id: LeafId,
        current_state: impl Into<String>,
        operation: impl Into<String>,
    ) -> Self {
        ApplyError::InvalidLeafState {
            leaf_id,
            current_state: current_state.into(),
            operation: operation.into(),
        }
    }

    /// Creates an `InsertionPositionNotFound` error.
    #[inline]
    pub fn insertion_position_not_found(
        description: impl Into<String>,
        after_ref: impl Into<String>,
    ) -> Self {
        ApplyError::InsertionPositionNotFound {
            description: description.into(),
            after_ref: after_ref.into(),
        }
    }

    /// Creates a `CircularReference` error.
    #[inline]
    pub fn circular_reference(description: impl Into<String>) -> Self {
        ApplyError::CircularReference {
            description: description.into(),
        }
    }

    /// Creates an `OrderingViolation` error.
    #[inline]
    pub fn ordering_violation(description: impl Into<String>) -> Self {
        ApplyError::OrderingViolation {
            description: description.into(),
        }
    }

    /// Creates a `ContentOutOfBounds` error.
    #[inline]
    pub fn content_out_of_bounds(start: usize, end: usize, content_len: usize) -> Self {
        ApplyError::ContentOutOfBounds {
            start,
            end,
            content_len,
        }
    }

    /// Creates an `InvalidContent` error.
    #[inline]
    pub fn invalid_content(description: impl Into<String>) -> Self {
        ApplyError::InvalidContent {
            description: description.into(),
        }
    }

    /// Creates a `Storage` error with context.
    #[inline]
    pub fn storage(source: PristineError, context: impl Into<String>) -> Self {
        ApplyError::Storage {
            source: Box::new(source),
            context: context.into(),
        }
    }

    /// Creates an `InternalError`.
    #[inline]
    pub fn internal(description: impl Into<String>) -> Self {
        ApplyError::InternalError {
            description: description.into(),
        }
    }

    // Classification Methods

    /// Returns `true` if this is a "not found" error.
    #[inline]
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            ApplyError::TrunkNotFound { .. }
                | ApplyError::BranchNotFound { .. }
                | ApplyError::LeafNotFound { .. }
                | ApplyError::PathNotFound { .. }
        )
    }

    /// Returns `true` if this is an "already exists" error.
    #[inline]
    pub fn is_already_exists(&self) -> bool {
        matches!(
            self,
            ApplyError::TrunkAlreadyExists { .. }
                | ApplyError::BranchAlreadyExists { .. }
                | ApplyError::LeafAlreadyExists { .. }
                | ApplyError::PathAlreadyExists { .. }
        )
    }

    /// Returns `true` if this is an "invalid state" error.
    #[inline]
    pub fn is_invalid_state(&self) -> bool {
        matches!(
            self,
            ApplyError::InvalidTrunkState { .. }
                | ApplyError::InvalidBranchState { .. }
                | ApplyError::InvalidLeafState { .. }
        )
    }

    /// Returns `true` if this is an ordering-related error.
    #[inline]
    pub fn is_ordering_error(&self) -> bool {
        matches!(
            self,
            ApplyError::InsertionPositionNotFound { .. }
                | ApplyError::CircularReference { .. }
                | ApplyError::OrderingViolation { .. }
        )
    }

    /// Returns `true` if this is a storage/database error.
    #[inline]
    pub fn is_storage_error(&self) -> bool {
        matches!(self, ApplyError::Storage { .. })
    }

    /// Returns `true` if this is a content-related error.
    #[inline]
    pub fn is_content_error(&self) -> bool {
        matches!(
            self,
            ApplyError::ContentOutOfBounds { .. } | ApplyError::InvalidContent { .. }
        )
    }

    /// Returns `true` if this is an internal/bug error.
    #[inline]
    pub fn is_internal_error(&self) -> bool {
        matches!(self, ApplyError::InternalError { .. })
    }

    /// Returns `true` if the error is potentially recoverable.
    ///
    /// "Recoverable" means the operation could potentially succeed
    /// if retried or if prerequisites are satisfied.
    #[inline]
    pub fn is_recoverable(&self) -> bool {
        // Not-found and already-exists errors might be recoverable
        // if the missing entity is created or the duplicate is handled
        self.is_not_found() || self.is_already_exists() || self.is_invalid_state()
    }

    // User Guidance

    /// Returns a user-friendly suggestion for resolving this error.
    pub fn suggestion(&self) -> &'static str {
        match self {
            ApplyError::TrunkNotFound { .. } => {
                "The referenced file does not exist. Please verify the file was created \
                 and the change dependencies are applied in the correct order."
            }
            ApplyError::BranchNotFound { .. } => {
                "The referenced line does not exist. Please verify the line was created \
                 and the change dependencies are applied in the correct order."
            }
            ApplyError::LeafNotFound { .. } => {
                "The referenced token does not exist. Please verify the token was created \
                 and the change dependencies are applied in the correct order."
            }
            ApplyError::PathNotFound { .. } => {
                "The specified file path does not exist in the repository. Please verify \
                 the path is correct and the file has been added."
            }
            ApplyError::TrunkAlreadyExists { .. } => {
                "A file with this ID already exists. This may indicate duplicate change \
                 application or a CRDT ID collision (extremely rare)."
            }
            ApplyError::BranchAlreadyExists { .. } => {
                "A line with this ID already exists. This may indicate duplicate change \
                 application or a CRDT ID collision (extremely rare)."
            }
            ApplyError::LeafAlreadyExists { .. } => {
                "A token with this ID already exists. This may indicate duplicate change \
                 application or a CRDT ID collision (extremely rare)."
            }
            ApplyError::PathAlreadyExists { .. } => {
                "A file already exists at this path. Use a different path or delete/move \
                 the existing file first."
            }
            ApplyError::InvalidTrunkState { .. } => {
                "The file is not in the correct state for this operation. For example, \
                 you cannot delete an already-deleted file."
            }
            ApplyError::InvalidBranchState { .. } => {
                "The line is not in the correct state for this operation. For example, \
                 you cannot restore a line that isn't deleted."
            }
            ApplyError::InvalidLeafState { .. } => {
                "The token is not in the correct state for this operation. For example, \
                 you cannot modify a deleted token."
            }
            ApplyError::InsertionPositionNotFound { .. } => {
                "Could not find a valid position for insertion. This may indicate missing \
                 dependencies or corrupted ordering data."
            }
            ApplyError::CircularReference { .. } => {
                "A circular reference was detected in the CRDT ordering. This indicates \
                 data corruption and should be reported as a bug."
            }
            ApplyError::OrderingViolation { .. } => {
                "The operation would violate CRDT ordering constraints. This may indicate \
                 a bug in change generation."
            }
            ApplyError::ContentOutOfBounds { .. } => {
                "The content range is invalid. This may indicate a corrupted change file \
                 or mismatched content blob."
            }
            ApplyError::InvalidContent { .. } => {
                "The content is invalid for this operation. Please verify the content \
                 encoding and format."
            }
            ApplyError::Storage { .. } => {
                "A database error occurred. Please verify the repository is not corrupted \
                 and there is sufficient disk space."
            }
            ApplyError::InternalError { .. } => {
                "An internal error occurred. This is likely a bug and should be reported \
                 to the Atomic development team."
            }
        }
    }

    /// Returns an appropriate exit code for this error.
    ///
    /// Exit codes follow Unix conventions:
    /// - 0: Success (not applicable for errors)
    /// - 1: General error
    /// - 2: Misuse (user error)
    /// - 65: Data format error
    /// - 70: Internal software error
    /// - 74: I/O error
    pub fn exit_code(&self) -> i32 {
        match self {
            // User errors (could be fixed by user action)
            ApplyError::TrunkNotFound { .. }
            | ApplyError::BranchNotFound { .. }
            | ApplyError::LeafNotFound { .. }
            | ApplyError::PathNotFound { .. }
            | ApplyError::TrunkAlreadyExists { .. }
            | ApplyError::BranchAlreadyExists { .. }
            | ApplyError::LeafAlreadyExists { .. }
            | ApplyError::PathAlreadyExists { .. }
            | ApplyError::InvalidTrunkState { .. }
            | ApplyError::InvalidBranchState { .. }
            | ApplyError::InvalidLeafState { .. } => 2,

            // Data format errors
            ApplyError::ContentOutOfBounds { .. }
            | ApplyError::InvalidContent { .. }
            | ApplyError::InsertionPositionNotFound { .. }
            | ApplyError::CircularReference { .. }
            | ApplyError::OrderingViolation { .. } => 65,

            // I/O errors
            ApplyError::Storage { .. } => 74,

            // Internal errors
            ApplyError::InternalError { .. } => 70,
        }
    }
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApplyError::TrunkNotFound { trunk_id } => {
                write!(f, "trunk not found: {}", trunk_id)
            }
            ApplyError::BranchNotFound { branch_id } => {
                write!(f, "branch not found: {}", branch_id)
            }
            ApplyError::LeafNotFound { leaf_id } => {
                write!(f, "leaf not found: {}", leaf_id)
            }
            ApplyError::PathNotFound { path } => {
                write!(f, "path not found: {:?}", path)
            }
            ApplyError::TrunkAlreadyExists { trunk_id } => {
                write!(f, "trunk already exists: {}", trunk_id)
            }
            ApplyError::BranchAlreadyExists { branch_id } => {
                write!(f, "branch already exists: {}", branch_id)
            }
            ApplyError::LeafAlreadyExists { leaf_id } => {
                write!(f, "leaf already exists: {}", leaf_id)
            }
            ApplyError::PathAlreadyExists {
                path,
                existing_trunk,
            } => {
                write!(
                    f,
                    "path already exists: {:?} (trunk: {})",
                    path, existing_trunk
                )
            }
            ApplyError::InvalidTrunkState {
                trunk_id,
                current_state,
                operation,
            } => {
                write!(
                    f,
                    "invalid trunk state for {}: cannot {} (current state: {})",
                    trunk_id, operation, current_state
                )
            }
            ApplyError::InvalidBranchState {
                branch_id,
                current_state,
                operation,
            } => {
                write!(
                    f,
                    "invalid branch state for {}: cannot {} (current state: {})",
                    branch_id, operation, current_state
                )
            }
            ApplyError::InvalidLeafState {
                leaf_id,
                current_state,
                operation,
            } => {
                write!(
                    f,
                    "invalid leaf state for {}: cannot {} (current state: {})",
                    leaf_id, operation, current_state
                )
            }
            ApplyError::InsertionPositionNotFound {
                description,
                after_ref,
            } => {
                write!(
                    f,
                    "insertion position not found for {}: after {:?}",
                    description, after_ref
                )
            }
            ApplyError::CircularReference { description } => {
                write!(f, "circular reference detected: {}", description)
            }
            ApplyError::OrderingViolation { description } => {
                write!(f, "ordering violation: {}", description)
            }
            ApplyError::ContentOutOfBounds {
                start,
                end,
                content_len,
            } => {
                write!(
                    f,
                    "content out of bounds: range {}..{} exceeds content length {}",
                    start, end, content_len
                )
            }
            ApplyError::InvalidContent { description } => {
                write!(f, "invalid content: {}", description)
            }
            ApplyError::Storage { source, context } => {
                write!(f, "storage error during {}: {}", context, source)
            }
            ApplyError::InternalError { description } => {
                write!(f, "internal error: {}", description)
            }
        }
    }
}

impl std::error::Error for ApplyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ApplyError::Storage { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<PristineError> for ApplyError {
    fn from(err: PristineError) -> Self {
        ApplyError::Storage {
            source: Box::new(err),
            context: "unknown operation".to_string(),
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;

    // Constructor Tests

    #[test]
    fn test_trunk_not_found() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let err = ApplyError::trunk_not_found(trunk_id);
        assert!(err.is_not_found());
        assert!(!err.is_already_exists());
        assert!(err.to_string().contains("trunk not found"));
    }

    #[test]
    fn test_branch_not_found() {
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let err = ApplyError::branch_not_found(branch_id);
        assert!(err.is_not_found());
        assert!(err.to_string().contains("branch not found"));
    }

    #[test]
    fn test_leaf_not_found() {
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let err = ApplyError::leaf_not_found(leaf_id);
        assert!(err.is_not_found());
        assert!(err.to_string().contains("leaf not found"));
    }

    #[test]
    fn test_path_not_found() {
        let err = ApplyError::path_not_found("src/main.rs");
        assert!(err.is_not_found());
        assert!(err.to_string().contains("src/main.rs"));
    }

    #[test]
    fn test_trunk_already_exists() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let err = ApplyError::trunk_already_exists(trunk_id);
        assert!(err.is_already_exists());
        assert!(!err.is_not_found());
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn test_branch_already_exists() {
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let err = ApplyError::branch_already_exists(branch_id);
        assert!(err.is_already_exists());
    }

    #[test]
    fn test_leaf_already_exists() {
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let err = ApplyError::leaf_already_exists(leaf_id);
        assert!(err.is_already_exists());
    }

    #[test]
    fn test_path_already_exists() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let err = ApplyError::path_already_exists("src/main.rs", trunk_id);
        assert!(err.is_already_exists());
        assert!(err.to_string().contains("src/main.rs"));
    }

    #[test]
    fn test_invalid_trunk_state() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let err = ApplyError::invalid_trunk_state(trunk_id, "deleted", "delete");
        assert!(err.is_invalid_state());
        assert!(err.to_string().contains("deleted"));
        assert!(err.to_string().contains("delete"));
    }

    #[test]
    fn test_invalid_branch_state() {
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let err = ApplyError::invalid_branch_state(branch_id, "alive", "restore");
        assert!(err.is_invalid_state());
    }

    #[test]
    fn test_invalid_leaf_state() {
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let err = ApplyError::invalid_leaf_state(leaf_id, "deleted", "replace");
        assert!(err.is_invalid_state());
    }

    #[test]
    fn test_insertion_position_not_found() {
        let err = ApplyError::insertion_position_not_found("branch", "after B1:0");
        assert!(err.is_ordering_error());
        assert!(err.to_string().contains("branch"));
        assert!(err.to_string().contains("after"));
    }

    #[test]
    fn test_circular_reference() {
        let err = ApplyError::circular_reference("branch ordering cycle");
        assert!(err.is_ordering_error());
        assert!(err.to_string().contains("circular"));
    }

    #[test]
    fn test_ordering_violation() {
        let err = ApplyError::ordering_violation("cannot insert before root");
        assert!(err.is_ordering_error());
    }

    #[test]
    fn test_content_out_of_bounds() {
        let err = ApplyError::content_out_of_bounds(100, 200, 50);
        assert!(err.is_content_error());
        assert!(err.to_string().contains("100"));
        assert!(err.to_string().contains("200"));
        assert!(err.to_string().contains("50"));
    }

    #[test]
    fn test_invalid_content() {
        let err = ApplyError::invalid_content("not valid UTF-8");
        assert!(err.is_content_error());
    }

    #[test]
    fn test_storage_error() {
        let pristine_err = PristineError::ViewNotFound {
            name: "test".to_string(),
        };
        let err = ApplyError::storage(pristine_err, "testing");
        assert!(err.is_storage_error());
        assert!(err.to_string().contains("testing"));
    }

    #[test]
    fn test_internal_error() {
        let err = ApplyError::internal("unexpected state");
        assert!(err.is_internal_error());
        assert!(err.to_string().contains("unexpected state"));
    }

    // Classification Tests

    #[test]
    fn test_is_recoverable() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // Not found errors are recoverable (might need to apply deps first)
        assert!(ApplyError::trunk_not_found(trunk_id).is_recoverable());

        // Already exists errors are recoverable (might be idempotent)
        assert!(ApplyError::trunk_already_exists(trunk_id).is_recoverable());

        // Invalid state errors are recoverable (might need state change)
        assert!(ApplyError::invalid_trunk_state(trunk_id, "deleted", "delete").is_recoverable());

        // Ordering errors are not recoverable
        assert!(!ApplyError::circular_reference("test").is_recoverable());

        // Internal errors are not recoverable
        assert!(!ApplyError::internal("bug").is_recoverable());
    }

    #[test]
    fn test_classification_mutual_exclusion() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let err = ApplyError::trunk_not_found(trunk_id);

        // Each error should only match one category
        let categories = [
            err.is_not_found(),
            err.is_already_exists(),
            err.is_invalid_state(),
            err.is_ordering_error(),
            err.is_storage_error(),
            err.is_content_error(),
            err.is_internal_error(),
        ];

        let true_count = categories.iter().filter(|&&b| b).count();
        assert_eq!(true_count, 1, "Error should match exactly one category");
    }

    // Suggestion Tests

    #[test]
    fn test_suggestions_not_empty() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);

        let errors = vec![
            ApplyError::trunk_not_found(trunk_id),
            ApplyError::branch_not_found(branch_id),
            ApplyError::leaf_not_found(leaf_id),
            ApplyError::path_not_found("test"),
            ApplyError::trunk_already_exists(trunk_id),
            ApplyError::branch_already_exists(branch_id),
            ApplyError::leaf_already_exists(leaf_id),
            ApplyError::path_already_exists("test", trunk_id),
            ApplyError::invalid_trunk_state(trunk_id, "s", "o"),
            ApplyError::invalid_branch_state(branch_id, "s", "o"),
            ApplyError::invalid_leaf_state(leaf_id, "s", "o"),
            ApplyError::insertion_position_not_found("d", "a"),
            ApplyError::circular_reference("d"),
            ApplyError::ordering_violation("d"),
            ApplyError::content_out_of_bounds(0, 1, 0),
            ApplyError::invalid_content("d"),
            ApplyError::internal("d"),
        ];

        for err in errors {
            let suggestion = err.suggestion();
            assert!(
                !suggestion.is_empty(),
                "Suggestion for {} should not be empty",
                err
            );
            assert!(
                suggestion.len() > 20,
                "Suggestion for {} should be helpful",
                err
            );
        }
    }

    // Exit Code Tests

    #[test]
    fn test_exit_codes() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);

        // User errors get exit code 2
        assert_eq!(ApplyError::trunk_not_found(trunk_id).exit_code(), 2);
        assert_eq!(ApplyError::trunk_already_exists(trunk_id).exit_code(), 2);
        assert_eq!(
            ApplyError::invalid_trunk_state(trunk_id, "s", "o").exit_code(),
            2
        );

        // Data format errors get exit code 65
        assert_eq!(ApplyError::content_out_of_bounds(0, 1, 0).exit_code(), 65);
        assert_eq!(ApplyError::circular_reference("test").exit_code(), 65);

        // I/O errors get exit code 74
        let pristine_err = PristineError::ViewNotFound {
            name: "test".to_string(),
        };
        assert_eq!(ApplyError::storage(pristine_err, "test").exit_code(), 74);

        // Internal errors get exit code 70
        assert_eq!(ApplyError::internal("bug").exit_code(), 70);
    }

    // Display and Debug Tests

    #[test]
    fn test_display_format() {
        let trunk_id = TrunkId::new(NodeId::new(42), 5);
        let err = ApplyError::trunk_not_found(trunk_id);
        let display = err.to_string();

        // Should contain identifying information
        assert!(display.contains("trunk"));
        assert!(display.contains("not found"));
    }

    #[test]
    fn test_debug_format() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let err = ApplyError::trunk_not_found(trunk_id);
        let debug = format!("{:?}", err);

        // Debug format should show enum variant
        assert!(debug.contains("TrunkNotFound"));
    }

    // Error Trait Tests

    #[test]
    fn test_error_source() {
        let pristine_err = PristineError::ViewNotFound {
            name: "test".to_string(),
        };
        let err = ApplyError::storage(pristine_err, "testing");

        // Storage error should have a source
        assert!(Error::source(&err).is_some());

        // Other errors should not have a source
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let err2 = ApplyError::trunk_not_found(trunk_id);
        assert!(Error::source(&err2).is_none());
    }

    #[test]
    fn test_from_pristine_error() {
        let pristine_err = PristineError::ViewNotFound {
            name: "test".to_string(),
        };
        let err: ApplyError = pristine_err.into();

        assert!(err.is_storage_error());
        assert!(err.to_string().contains("unknown operation"));
    }

    // ApplyResult Tests

    #[test]
    fn test_apply_result_ok() {
        let result: ApplyResult<i32> = Ok(42);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_apply_result_err() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let result: ApplyResult<()> = Err(ApplyError::trunk_not_found(trunk_id));
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_result_question_mark_operator() {
        fn inner() -> ApplyResult<i32> {
            let trunk_id = TrunkId::new(NodeId::new(1), 0);
            Err(ApplyError::trunk_not_found(trunk_id))
        }

        fn outer() -> ApplyResult<i32> {
            let _value = inner()?;
            Ok(42)
        }

        assert!(outer().is_err());
    }
}

//! Error types for the record module
//!
//! This module defines error types that can occur during the recording process.
//! Recording is the process of detecting changes in the working copy and
//! converting them into [`GraphOp`] operations that can be serialized into a
//! [`Change`].
//!
//! # Error Categories
//!
//! Errors during recording fall into several categories:
//!
//! - **IO Errors**: File system operations failing (read, stat, etc.)
//! - **Database Errors**: Pristine storage layer failures
//! - **Diff Errors**: Failures during content comparison
//! - **Path Errors**: Invalid or inaccessible paths
//! - **Encoding Errors**: Character encoding detection/conversion failures
//!
//! # Example
//!
//! ```rust
//! use atomic_core::record::RecordError;
//!
//! fn handle_record_error(err: RecordError) {
//!     match &err {
//!         RecordError::Io(io_err) => {
//!             eprintln!("IO error during recording: {}", io_err);
//!         }
//!         RecordError::PathNotInRepo { path } => {
//!             eprintln!("Path '{}' is not in the repository", path);
//!         }
//!         _ => {
//!             eprintln!("Recording failed: {}", err);
//!         }
//!     }
//! }
//! ```
//!
//! [`GraphOp`]: crate::change::GraphOp
//! [`Change`]: crate::change::Change

use std::path::PathBuf;

use thiserror::Error;

use crate::pristine::PristineError;
use crate::types::Hash;

/// Errors that can occur during the recording process.
///
/// Recording involves reading the working copy, comparing it with the
/// pristine state, and generating hunks that represent the changes.
/// This enum captures all the ways that process can fail.
///
/// # Error Handling Strategy
///
/// Most record operations return `Result<T, RecordError>`. The recommended
/// approach is to:
///
/// 1. Handle recoverable errors (like missing files) gracefully
/// 2. Propagate unrecoverable errors (like database corruption)
/// 3. Provide context when re-throwing errors
///
/// # Example
///
/// ```rust
/// use atomic_core::record::RecordError;
///
/// // Check if an error is recoverable
/// fn is_recoverable(err: &RecordError) -> bool {
///     matches!(err,
///         RecordError::PathNotInRepo { .. } |
///         RecordError::PathNotFound { .. } |
///         RecordError::PermissionDenied { .. }
///     )
/// }
/// ```
#[derive(Debug, Error)]
pub enum RecordError {
    /// An IO error occurred during file operations.
    ///
    /// This typically happens when:
    /// - Reading file contents fails
    /// - Getting file metadata fails
    /// - Iterating directory contents fails
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A database/pristine error occurred.
    ///
    /// This indicates a problem with the underlying storage layer,
    /// such as:
    /// - Transaction failures
    /// - Corruption in the pristine database
    /// - Missing expected data
    #[error("Pristine error: {0}")]
    Pristine(#[from] PristineError),

    /// The specified path is not within the repository.
    ///
    /// This occurs when attempting to record changes for a path that
    /// is outside the repository root. The path may exist on disk but
    /// is not tracked by this repository.
    ///
    /// # Fields
    ///
    /// * `path` - The path that was requested
    #[error("Path not in repository: {path}")]
    PathNotInRepo {
        /// The path that is outside the repository
        path: String,
    },

    /// The specified path was not found in the working copy.
    ///
    /// Unlike `PathNotInRepo`, this means the path *should* be in the
    /// repository but doesn't exist on disk. This might indicate:
    /// - A file was deleted but not recorded
    /// - A typo in the path
    /// - A race condition where the file was removed during recording
    ///
    /// # Fields
    ///
    /// * `path` - The path that could not be found
    #[error("Path not found: {}", path.display())]
    PathNotFound {
        /// The path that doesn't exist
        path: PathBuf,
    },

    /// Permission denied when accessing a file or directory.
    ///
    /// This occurs when the process doesn't have sufficient permissions
    /// to read a file or directory in the working copy.
    ///
    /// # Fields
    ///
    /// * `path` - The path that couldn't be accessed
    #[error("Permission denied: {}", path.display())]
    PermissionDenied {
        /// The inaccessible path
        path: PathBuf,
    },

    /// A required dependency is missing from the repository.
    ///
    /// When recording changes that depend on previous changes, those
    /// dependencies must be present in the repository. This error
    /// indicates that a required dependency could not be found.
    ///
    /// # Fields
    ///
    /// * `hash` - The hash of the missing dependency
    #[error("Missing dependency: {}", hash)]
    MissingDependency {
        /// The hash of the missing change
        hash: Hash,
    },

    /// Error during diff computation.
    ///
    /// This occurs when the diff algorithm encounters a problem
    /// comparing file contents, such as:
    /// - Memory allocation failures for large files
    /// - Invalid UTF-8 in text files (when text mode is required)
    ///
    /// # Fields
    ///
    /// * `message` - Description of the diff error
    #[error("Diff error: {message}")]
    Diff {
        /// Description of what went wrong
        message: String,
    },

    /// Error during encoding detection or conversion.
    ///
    /// This occurs when:
    /// - A file's encoding cannot be determined
    /// - Content cannot be converted to the expected encoding
    /// - Binary content is found where text was expected
    ///
    /// # Fields
    ///
    /// * `path` - The file with encoding issues
    /// * `message` - Description of the encoding problem
    #[error("Encoding error for {}: {message}", path.display())]
    Encoding {
        /// The file with encoding issues
        path: PathBuf,
        /// Description of the encoding problem
        message: String,
    },

    /// The file is too large to record.
    ///
    /// Some operations have limits on file size to prevent excessive
    /// memory usage or processing time.
    ///
    /// # Fields
    ///
    /// * `path` - The file that is too large
    /// * `size` - The actual size in bytes
    /// * `max_size` - The maximum allowed size in bytes
    #[error("File too large: {} is {} bytes (max: {} bytes)", path.display(), size, max_size)]
    FileTooLarge {
        /// The file that exceeds the size limit
        path: PathBuf,
        /// The actual file size in bytes
        size: u64,
        /// The maximum allowed size in bytes
        max_size: u64,
    },

    /// The inode could not be found in the database.
    ///
    /// This indicates an inconsistency between the working copy state
    /// and the pristine database. The file exists but its internal
    /// identifier is missing.
    ///
    /// # Fields
    ///
    /// * `path` - The path whose inode is missing
    #[error("Inode not found for path: {}", path.display())]
    InodeNotFound {
        /// The path with the missing inode
        path: PathBuf,
    },

    /// A conflict was detected that prevents recording.
    ///
    /// Some conflicts must be resolved before changes can be recorded.
    /// This error indicates such a blocking conflict was found.
    ///
    /// # Fields
    ///
    /// * `path` - The path with the conflict
    /// * `message` - Description of the conflict
    #[error("Conflict in {}: {message}", path.display())]
    Conflict {
        /// The path with the conflict
        path: PathBuf,
        /// Description of the conflict
        message: String,
    },

    /// The working copy is in an invalid state.
    ///
    /// This is a catch-all for situations where the working copy
    /// state is inconsistent or corrupted in a way that prevents
    /// recording.
    ///
    /// # Fields
    ///
    /// * `message` - Description of the invalid state
    #[error("Invalid working copy state: {message}")]
    InvalidState {
        /// Description of the invalid state
        message: String,
    },

    /// A system time error occurred.
    ///
    /// This happens when reading file modification times fails,
    /// typically due to system clock issues or very old files.
    #[error("System time error: {0}")]
    SystemTime(#[from] std::time::SystemTimeError),

    /// An internal error occurred (indicates a bug).
    ///
    /// This error type is used for conditions that should never
    /// occur if the code is correct. If you encounter this error,
    /// please report it as a bug.
    ///
    /// # Fields
    ///
    /// * `message` - Description of the internal error
    #[error("Internal error: {message}")]
    Internal {
        /// Description of what went wrong internally
        message: String,
    },
}

impl RecordError {
    /// Create a new path-not-in-repo error.
    ///
    /// # Arguments
    ///
    /// * `path` - The path that is outside the repository
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::RecordError;
    ///
    /// let err = RecordError::path_not_in_repo("/outside/path");
    /// assert!(matches!(err, RecordError::PathNotInRepo { .. }));
    /// ```
    pub fn path_not_in_repo(path: impl Into<String>) -> Self {
        Self::PathNotInRepo { path: path.into() }
    }

    /// Create a new path-not-found error.
    ///
    /// # Arguments
    ///
    /// * `path` - The path that doesn't exist
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::RecordError;
    /// use std::path::PathBuf;
    ///
    /// let err = RecordError::path_not_found(PathBuf::from("/missing/file.txt"));
    /// assert!(matches!(err, RecordError::PathNotFound { .. }));
    /// ```
    pub fn path_not_found(path: impl Into<PathBuf>) -> Self {
        Self::PathNotFound { path: path.into() }
    }

    /// Create a new permission-denied error.
    ///
    /// # Arguments
    ///
    /// * `path` - The path that couldn't be accessed
    pub fn permission_denied(path: impl Into<PathBuf>) -> Self {
        Self::PermissionDenied { path: path.into() }
    }

    /// Create a new missing-dependency error.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the missing dependency
    pub fn missing_dependency(hash: Hash) -> Self {
        Self::MissingDependency { hash }
    }

    /// Create a new diff error.
    ///
    /// # Arguments
    ///
    /// * `message` - Description of the diff error
    pub fn diff(message: impl Into<String>) -> Self {
        Self::Diff {
            message: message.into(),
        }
    }

    /// Create a new encoding error.
    ///
    /// # Arguments
    ///
    /// * `path` - The file with encoding issues
    /// * `message` - Description of the problem
    pub fn encoding(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Encoding {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Create a new file-too-large error.
    ///
    /// # Arguments
    ///
    /// * `path` - The file that is too large
    /// * `size` - The actual size in bytes
    /// * `max_size` - The maximum allowed size in bytes
    pub fn file_too_large(path: impl Into<PathBuf>, size: u64, max_size: u64) -> Self {
        Self::FileTooLarge {
            path: path.into(),
            size,
            max_size,
        }
    }

    /// Create a new inode-not-found error.
    ///
    /// # Arguments
    ///
    /// * `path` - The path whose inode is missing
    pub fn inode_not_found(path: impl Into<PathBuf>) -> Self {
        Self::InodeNotFound { path: path.into() }
    }

    /// Create a new conflict error.
    ///
    /// # Arguments
    ///
    /// * `path` - The path with the conflict
    /// * `message` - Description of the conflict
    pub fn conflict(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Conflict {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Create a new invalid-state error.
    ///
    /// # Arguments
    ///
    /// * `message` - Description of the invalid state
    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::InvalidState {
            message: message.into(),
        }
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

    /// Check if this error indicates a missing or inaccessible path.
    ///
    /// This is useful for handling cases where files may have been
    /// deleted or moved during recording.
    ///
    /// # Returns
    ///
    /// `true` if the error is about a missing or inaccessible path.
    pub fn is_path_error(&self) -> bool {
        matches!(
            self,
            Self::PathNotInRepo { .. }
                | Self::PathNotFound { .. }
                | Self::PermissionDenied { .. }
                | Self::InodeNotFound { .. }
        )
    }

    /// Check if this error indicates a storage/database problem.
    ///
    /// These errors typically indicate corruption or system issues
    /// that may require administrator intervention.
    ///
    /// # Returns
    ///
    /// `true` if the error is related to storage.
    pub fn is_storage_error(&self) -> bool {
        matches!(self, Self::Pristine(_) | Self::Io(_))
    }

    /// Check if this error is recoverable.
    ///
    /// Recoverable errors are those where the operation can potentially
    /// succeed if retried or if the user fixes the underlying issue.
    ///
    /// # Returns
    ///
    /// `true` if the error might be recoverable.
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            Self::PathNotInRepo { .. }
                | Self::PathNotFound { .. }
                | Self::PermissionDenied { .. }
                | Self::Conflict { .. }
                | Self::FileTooLarge { .. }
        )
    }
}

/// Result type alias for record operations.
///
/// This is a convenience type for functions that return `Result<T, RecordError>`.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::{RecordResult, RecordError};
///
/// fn my_record_function() -> RecordResult<Vec<u8>> {
///     // ... do recording work ...
///     Ok(vec![1, 2, 3])
/// }
/// ```
pub type RecordResult<T> = Result<T, RecordError>;

#[cfg(test)]
mod tests {
    use super::*;

    // -- Classification: the record loop uses these to decide skip vs abort.

    #[test]
    fn path_errors_are_all_user_addressable_file_issues() {
        let path_errors = [
            RecordError::path_not_in_repo("/outside/repo"),
            RecordError::path_not_found(PathBuf::from("/gone")),
            RecordError::permission_denied(PathBuf::from("/locked")),
            RecordError::inode_not_found(PathBuf::from("/orphan")),
        ];
        for err in &path_errors {
            assert!(err.is_path_error(), "{err} should be a path error");
        }

        // Diff, encoding, and internal errors are NOT path problems.
        let non_path = [
            RecordError::diff("algorithm failed"),
            RecordError::invalid_state("corrupted"),
            RecordError::internal("bug"),
            RecordError::encoding(PathBuf::from("f"), "bad utf8"),
            RecordError::missing_dependency(Hash::of(b"x")),
        ];
        for err in &non_path {
            assert!(!err.is_path_error(), "{err} should NOT be a path error");
        }
    }

    #[test]
    fn storage_errors_identify_infrastructure_failures() {
        let io: RecordError = std::io::Error::new(std::io::ErrorKind::NotFound, "disk gone").into();
        let pristine: RecordError = PristineError::StackNotFound { name: "x".into() }.into();

        assert!(io.is_storage_error());
        assert!(pristine.is_storage_error());

        // User-level errors should never be classified as storage.
        assert!(!RecordError::path_not_in_repo("x").is_storage_error());
        assert!(!RecordError::diff("x").is_storage_error());
        assert!(!RecordError::file_too_large(PathBuf::from("x"), 100, 50).is_storage_error());
    }

    #[test]
    fn recoverable_errors_let_record_skip_a_file_and_continue() {
        // These are user-fixable: rename, chmod, resolve conflict, reduce file.
        let recoverable = [
            RecordError::path_not_in_repo("x"),
            RecordError::path_not_found(PathBuf::from("x")),
            RecordError::permission_denied(PathBuf::from("x")),
            RecordError::conflict(PathBuf::from("x"), "both sides edited"),
            RecordError::file_too_large(PathBuf::from("x"), 100, 50),
        ];
        for err in &recoverable {
            assert!(err.is_recoverable(), "{err} should be recoverable");
        }

        // Internal/state errors mean the repo is broken — can't skip and continue.
        let fatal = [
            RecordError::internal("assertion failed"),
            RecordError::invalid_state("corrupted working copy"),
        ];
        for err in &fatal {
            assert!(!err.is_recoverable(), "{err} should be fatal");
        }
    }

    // -- Error propagation: ? must work across the error hierarchy.

    #[test]
    fn question_mark_propagates_io_and_pristine() {
        fn read_file() -> RecordResult<Vec<u8>> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"))?
        }

        fn check_pristine() -> RecordResult<()> {
            Err(PristineError::StackNotFound {
                name: "main".into(),
            })?
        }

        assert!(matches!(read_file(), Err(RecordError::Io(_))));
        assert!(matches!(check_pristine(), Err(RecordError::Pristine(_))));
    }

    // -- Display: messages must include enough context for the user to act.

    #[test]
    fn display_messages_surface_paths_and_context() {
        let cases: Vec<(RecordError, &[&str])> = vec![
            (
                RecordError::path_not_in_repo("/outside/repo"),
                &["Path not in repository", "/outside/repo"],
            ),
            (
                RecordError::path_not_found(PathBuf::from("/missing/file.txt")),
                &["Path not found", "missing/file.txt"],
            ),
            (
                RecordError::permission_denied(PathBuf::from("/restricted/secret")),
                &["Permission denied", "restricted/secret"],
            ),
            (
                RecordError::encoding(PathBuf::from("data.bin"), "not valid UTF-8"),
                &["Encoding error", "data.bin", "not valid UTF-8"],
            ),
            (
                RecordError::conflict(PathBuf::from("src/lib.rs"), "both sides edited line 42"),
                &["Conflict", "src/lib.rs", "both sides edited"],
            ),
        ];

        for (err, expected_fragments) in &cases {
            let msg = err.to_string();
            for frag in *expected_fragments {
                assert!(msg.contains(frag), "{msg:?} missing {frag:?}");
            }
        }
    }

    #[test]
    fn file_too_large_display_shows_both_sizes() {
        let err = RecordError::file_too_large(PathBuf::from("video.mp4"), 500_000_000, 10_000_000);
        let msg = err.to_string();
        // User needs to see both: how big the file is AND what the limit is.
        assert!(msg.contains("500000000"), "should show actual size: {msg}");
        assert!(msg.contains("10000000"), "should show max size: {msg}");
        assert!(msg.contains("video.mp4"), "should show filename: {msg}");
    }

    #[test]
    fn file_too_large_does_not_panic_on_extreme_values() {
        // Guard against formatting issues with boundary values.
        let _ = RecordError::file_too_large(PathBuf::from("f"), 0, 0).to_string();
        let _ = RecordError::file_too_large(PathBuf::from("f"), u64::MAX, u64::MAX - 1).to_string();
    }
}

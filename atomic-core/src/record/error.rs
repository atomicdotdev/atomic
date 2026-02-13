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

    // =========================================================================
    // RecordError Construction Tests
    // =========================================================================

    #[test]
    fn test_path_not_in_repo_construction() {
        let err = RecordError::path_not_in_repo("/outside/repo");
        assert!(matches!(err, RecordError::PathNotInRepo { path } if path == "/outside/repo"));
    }

    #[test]
    fn test_path_not_in_repo_from_string() {
        let path = String::from("some/path");
        let err = RecordError::path_not_in_repo(path);
        assert!(matches!(err, RecordError::PathNotInRepo { path } if path == "some/path"));
    }

    #[test]
    fn test_path_not_found_construction() {
        let err = RecordError::path_not_found(PathBuf::from("/missing/file.txt"));
        assert!(matches!(err, RecordError::PathNotFound { path } if path == PathBuf::from("/missing/file.txt")));
    }

    #[test]
    fn test_permission_denied_construction() {
        let err = RecordError::permission_denied(PathBuf::from("/restricted"));
        assert!(matches!(err, RecordError::PermissionDenied { path } if path == PathBuf::from("/restricted")));
    }

    #[test]
    fn test_missing_dependency_construction() {
        let hash = Hash::of(b"test dependency");
        let err = RecordError::missing_dependency(hash);
        assert!(matches!(err, RecordError::MissingDependency { hash: h } if h == hash));
    }

    #[test]
    fn test_diff_error_construction() {
        let err = RecordError::diff("failed to compute diff");
        assert!(
            matches!(err, RecordError::Diff { message } if message == "failed to compute diff")
        );
    }

    #[test]
    fn test_encoding_error_construction() {
        let err = RecordError::encoding(PathBuf::from("file.txt"), "invalid UTF-8");
        match err {
            RecordError::Encoding { path, message } => {
                assert_eq!(path, PathBuf::from("file.txt"));
                assert_eq!(message, "invalid UTF-8");
            }
            _ => panic!("Expected Encoding error"),
        }
    }

    #[test]
    fn test_file_too_large_construction() {
        let err = RecordError::file_too_large(PathBuf::from("huge.bin"), 1_000_000_000, 100_000_000);
        match err {
            RecordError::FileTooLarge {
                path,
                size,
                max_size,
            } => {
                assert_eq!(path, PathBuf::from("huge.bin"));
                assert_eq!(size, 1_000_000_000);
                assert_eq!(max_size, 100_000_000);
            }
            _ => panic!("Expected FileTooLarge error"),
        }
    }

    #[test]
    fn test_inode_not_found_construction() {
        let err = RecordError::inode_not_found(PathBuf::from("orphan.txt"));
        assert!(
            matches!(err, RecordError::InodeNotFound { path } if path == PathBuf::from("orphan.txt"))
        );
    }

    #[test]
    fn test_conflict_construction() {
        let err = RecordError::conflict(PathBuf::from("conflict.txt"), "merge conflict detected");
        match err {
            RecordError::Conflict { path, message } => {
                assert_eq!(path, PathBuf::from("conflict.txt"));
                assert_eq!(message, "merge conflict detected");
            }
            _ => panic!("Expected Conflict error"),
        }
    }

    #[test]
    fn test_invalid_state_construction() {
        let err = RecordError::invalid_state("working copy corrupted");
        assert!(
            matches!(err, RecordError::InvalidState { message } if message == "working copy corrupted")
        );
    }

    #[test]
    fn test_internal_error_construction() {
        let err = RecordError::internal("unexpected condition");
        assert!(
            matches!(err, RecordError::Internal { message } if message == "unexpected condition")
        );
    }

    // =========================================================================
    // Error Classification Tests
    // =========================================================================

    #[test]
    fn test_is_path_error() {
        assert!(RecordError::path_not_in_repo("x").is_path_error());
        assert!(RecordError::path_not_found(PathBuf::from("x")).is_path_error());
        assert!(RecordError::permission_denied(PathBuf::from("x")).is_path_error());
        assert!(RecordError::inode_not_found(PathBuf::from("x")).is_path_error());

        // Non-path errors
        assert!(!RecordError::diff("x").is_path_error());
        assert!(!RecordError::invalid_state("x").is_path_error());
        assert!(!RecordError::internal("x").is_path_error());
    }

    #[test]
    fn test_is_storage_error() {
        let io_err = RecordError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "test error",
        ));
        assert!(io_err.is_storage_error());

        let pristine_err = RecordError::Pristine(PristineError::StackNotFound {
            name: "test".to_string(),
        });
        assert!(pristine_err.is_storage_error());

        // Non-storage errors
        assert!(!RecordError::path_not_in_repo("x").is_storage_error());
        assert!(!RecordError::diff("x").is_storage_error());
    }

    #[test]
    fn test_is_recoverable() {
        // Recoverable errors
        assert!(RecordError::path_not_in_repo("x").is_recoverable());
        assert!(RecordError::path_not_found(PathBuf::from("x")).is_recoverable());
        assert!(RecordError::permission_denied(PathBuf::from("x")).is_recoverable());
        assert!(RecordError::conflict(PathBuf::from("x"), "y").is_recoverable());
        assert!(RecordError::file_too_large(PathBuf::from("x"), 100, 50).is_recoverable());

        // Non-recoverable errors
        assert!(!RecordError::internal("bug").is_recoverable());
        assert!(!RecordError::invalid_state("corrupted").is_recoverable());
    }

    // =========================================================================
    // Error Display Tests
    // =========================================================================

    #[test]
    fn test_error_display_path_not_in_repo() {
        let err = RecordError::path_not_in_repo("/outside/path");
        let display = format!("{}", err);
        assert!(display.contains("Path not in repository"));
        assert!(display.contains("/outside/path"));
    }

    #[test]
    fn test_error_display_path_not_found() {
        let err = RecordError::path_not_found(PathBuf::from("/missing/file.txt"));
        let display = format!("{}", err);
        assert!(display.contains("Path not found"));
        assert!(display.contains("missing/file.txt"));
    }

    #[test]
    fn test_error_display_permission_denied() {
        let err = RecordError::permission_denied(PathBuf::from("/restricted/file"));
        let display = format!("{}", err);
        assert!(display.contains("Permission denied"));
        assert!(display.contains("restricted/file"));
    }

    #[test]
    fn test_error_display_missing_dependency() {
        let hash = Hash::of(b"dependency");
        let err = RecordError::missing_dependency(hash);
        let display = format!("{}", err);
        assert!(display.contains("Missing dependency"));
    }

    #[test]
    fn test_error_display_diff() {
        let err = RecordError::diff("failed to compute LCS");
        let display = format!("{}", err);
        assert!(display.contains("Diff error"));
        assert!(display.contains("failed to compute LCS"));
    }

    #[test]
    fn test_error_display_encoding() {
        let err = RecordError::encoding(PathBuf::from("binary.dat"), "not valid UTF-8");
        let display = format!("{}", err);
        assert!(display.contains("Encoding error"));
        assert!(display.contains("binary.dat"));
        assert!(display.contains("not valid UTF-8"));
    }

    #[test]
    fn test_error_display_file_too_large() {
        let err = RecordError::file_too_large(PathBuf::from("large.bin"), 1000, 500);
        let display = format!("{}", err);
        assert!(display.contains("File too large"));
        assert!(display.contains("large.bin"));
        assert!(display.contains("1000"));
        assert!(display.contains("500"));
    }

    #[test]
    fn test_error_display_inode_not_found() {
        let err = RecordError::inode_not_found(PathBuf::from("orphan.txt"));
        let display = format!("{}", err);
        assert!(display.contains("Inode not found"));
        assert!(display.contains("orphan.txt"));
    }

    #[test]
    fn test_error_display_conflict() {
        let err = RecordError::conflict(PathBuf::from("merged.txt"), "both modified");
        let display = format!("{}", err);
        assert!(display.contains("Conflict"));
        assert!(display.contains("merged.txt"));
        assert!(display.contains("both modified"));
    }

    #[test]
    fn test_error_display_invalid_state() {
        let err = RecordError::invalid_state("working copy is corrupted");
        let display = format!("{}", err);
        assert!(display.contains("Invalid working copy state"));
        assert!(display.contains("corrupted"));
    }

    #[test]
    fn test_error_display_internal() {
        let err = RecordError::internal("invariant violated");
        let display = format!("{}", err);
        assert!(display.contains("Internal error"));
        assert!(display.contains("invariant violated"));
    }

    // =========================================================================
    // Error From Trait Tests
    // =========================================================================

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let record_err: RecordError = io_err.into();
        assert!(matches!(record_err, RecordError::Io(_)));
    }

    #[test]
    fn test_from_pristine_error() {
        let pristine_err = PristineError::StackNotFound {
            name: "test".to_string(),
        };
        let record_err: RecordError = pristine_err.into();
        assert!(matches!(record_err, RecordError::Pristine(_)));
    }

    // =========================================================================
    // Error Debug Tests
    // =========================================================================

    #[test]
    fn test_error_debug_format() {
        let err = RecordError::path_not_in_repo("/some/path");
        let debug = format!("{:?}", err);
        assert!(debug.contains("PathNotInRepo"));
        assert!(debug.contains("/some/path"));
    }

    #[test]
    fn test_error_debug_nested() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let record_err: RecordError = io_err.into();
        let debug = format!("{:?}", record_err);
        assert!(debug.contains("Io"));
    }

    // =========================================================================
    // RecordResult Type Tests
    // =========================================================================

    #[test]
    fn test_record_result_ok() {
        let result: RecordResult<i32> = Ok(42);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_record_result_err() {
        let result: RecordResult<i32> = Err(RecordError::internal("test"));
        assert!(result.is_err());
    }

    #[test]
    fn test_record_result_question_mark_operator() {
        fn inner() -> RecordResult<i32> {
            Err(RecordError::internal("inner error"))
        }

        fn outer() -> RecordResult<i32> {
            let _value = inner()?;
            Ok(42)
        }

        assert!(outer().is_err());
    }

    // =========================================================================
    // Edge Case Tests
    // =========================================================================

    #[test]
    fn test_empty_path_string() {
        let err = RecordError::path_not_in_repo("");
        assert!(matches!(err, RecordError::PathNotInRepo { path } if path.is_empty()));
    }

    #[test]
    fn test_empty_message() {
        let err = RecordError::diff("");
        assert!(matches!(err, RecordError::Diff { message } if message.is_empty()));
    }

    #[test]
    fn test_unicode_in_path() {
        let err = RecordError::path_not_found(PathBuf::from("/路径/文件.txt"));
        let display = format!("{}", err);
        assert!(display.contains("文件.txt"));
    }

    #[test]
    fn test_unicode_in_message() {
        let err = RecordError::diff("比较失败 - 无效的编码");
        let display = format!("{}", err);
        assert!(display.contains("比较失败"));
    }

    #[test]
    fn test_zero_size_file() {
        let err = RecordError::file_too_large(PathBuf::from("file"), 0, 0);
        let display = format!("{}", err);
        assert!(display.contains("0 bytes"));
    }

    #[test]
    fn test_max_u64_size() {
        let err = RecordError::file_too_large(PathBuf::from("file"), u64::MAX, u64::MAX - 1);
        // Should not panic on display
        let _ = format!("{}", err);
    }
}

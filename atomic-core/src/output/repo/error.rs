//! Error types for repository output operations.
//!
//! This module provides the [`OutputError`] type which covers all failure modes
//! that can occur when outputting repository state to the working copy.
//!
//! # Overview
//!
//! Output operations can fail for various reasons:
//!
//! - **I/O errors**: File creation, writing, or permission issues
//! - **Graph errors**: Missing vertices, invalid structure
//! - **Tree errors**: Missing paths, invalid inodes
//! - **Change store errors**: Missing changes, content retrieval failures
//!
//! # Example
//!
//! ```rust
//! use atomic_core::output::repo::OutputError;
//!
//! // Create errors from various sources
//! let io_err = OutputError::io(std::io::Error::new(
//!     std::io::ErrorKind::NotFound,
//!     "file not found"
//! ));
//!
//! let path_err = OutputError::path_not_found("src/missing.rs");
//!
//! // Check error type
//! assert!(path_err.is_not_found());
//! ```

use crate::pristine::PristineError;
use crate::types::Inode;
use std::fmt;

// OUTPUT ERROR

/// Errors that can occur during repository output operations.
///
/// This error type provides detailed information about what went wrong
/// during output, including the source error when available.
///
/// # Error Categories
///
/// | Variant | Description |
/// |---------|-------------|
/// | `Io` | Filesystem I/O error |
/// | `WorkingCopy` | Working copy operation failed |
/// | `ChangeStore` | Change store operation failed |
/// | `Graph` | Graph traversal error |
/// | `Pristine` | Database operation failed |
/// | `PathNotFound` | Path doesn't exist in tree |
/// | `InodeNotFound` | Inode doesn't exist |
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::OutputError;
/// use atomic_core::types::Inode;
///
/// // Different ways to create errors
/// let err = OutputError::path_not_found("missing/file.rs");
/// assert!(err.is_not_found());
///
/// let err = OutputError::inode_not_found(Inode::new(42));
/// assert!(err.is_not_found());
/// ```
#[derive(Debug)]
pub enum OutputError {
    /// I/O error during file operations.
    Io(std::io::Error),

    /// Working copy operation failed.
    ///
    /// This wraps errors from the working copy trait implementation,
    /// such as file creation or permission failures.
    WorkingCopy(Box<dyn std::error::Error + Send + Sync>),

    /// Change store operation failed.
    ///
    /// This wraps errors from the change store, such as missing
    /// changes or content retrieval failures.
    ChangeStore(Box<dyn std::error::Error + Send + Sync>),

    /// Graph traversal or query failed.
    ///
    /// This indicates an error in the repository's internal graph
    /// structure, such as missing vertices or invalid edges.
    Graph(Box<dyn std::error::Error + Send + Sync>),

    /// Pristine database operation failed.
    Pristine(Box<PristineError>),

    /// Path not found in the repository tree.
    PathNotFound {
        /// The path that was not found.
        path: String,
    },

    /// Inode not found in the repository.
    InodeNotFound {
        /// The inode that was not found.
        inode: Inode,
    },
}

impl OutputError {
    /// Create an I/O error.
    ///
    /// # Arguments
    ///
    /// * `err` - The underlying I/O error
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputError;
    ///
    /// let err = OutputError::io(std::io::Error::new(
    ///     std::io::ErrorKind::PermissionDenied,
    ///     "access denied"
    /// ));
    /// assert!(err.to_string().contains("I/O error"));
    /// ```
    pub fn io(err: std::io::Error) -> Self {
        Self::Io(err)
    }

    /// Create a working copy error.
    ///
    /// # Arguments
    ///
    /// * `err` - The underlying error from the working copy
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputError;
    ///
    /// let err = OutputError::working_copy(std::io::Error::new(
    ///     std::io::ErrorKind::NotFound,
    ///     "file not found"
    /// ));
    /// assert!(err.to_string().contains("Working copy"));
    /// ```
    pub fn working_copy<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::WorkingCopy(Box::new(err))
    }

    /// Create a change store error.
    ///
    /// # Arguments
    ///
    /// * `err` - The underlying error from the change store
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputError;
    ///
    /// let err = OutputError::change_store(std::io::Error::new(
    ///     std::io::ErrorKind::NotFound,
    ///     "change not found"
    /// ));
    /// assert!(err.to_string().contains("Change store"));
    /// ```
    pub fn change_store<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::ChangeStore(Box::new(err))
    }

    /// Create a graph error.
    ///
    /// # Arguments
    ///
    /// * `err` - The underlying graph error
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputError;
    ///
    /// let err = OutputError::graph(std::io::Error::new(
    ///     std::io::ErrorKind::InvalidData,
    ///     "corrupted graph"
    /// ));
    /// assert!(err.to_string().contains("Graph"));
    /// ```
    pub fn graph<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Graph(Box::new(err))
    }

    /// Create a path not found error.
    ///
    /// # Arguments
    ///
    /// * `path` - The path that was not found
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputError;
    ///
    /// let err = OutputError::path_not_found("src/missing.rs");
    /// assert!(err.is_not_found());
    /// assert!(err.to_string().contains("src/missing.rs"));
    /// ```
    pub fn path_not_found(path: impl Into<String>) -> Self {
        Self::PathNotFound { path: path.into() }
    }

    /// Create an inode not found error.
    ///
    /// # Arguments
    ///
    /// * `inode` - The inode that was not found
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputError;
    /// use atomic_core::types::Inode;
    ///
    /// let err = OutputError::inode_not_found(Inode::new(42));
    /// assert!(err.is_not_found());
    /// ```
    pub fn inode_not_found(inode: Inode) -> Self {
        Self::InodeNotFound { inode }
    }

    /// Check if this is a "not found" error.
    ///
    /// Returns `true` for `PathNotFound` and `InodeNotFound` variants.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputError;
    ///
    /// let path_err = OutputError::path_not_found("missing.rs");
    /// assert!(path_err.is_not_found());
    ///
    /// let io_err = OutputError::io(std::io::Error::new(
    ///     std::io::ErrorKind::Other,
    ///     "other error"
    /// ));
    /// assert!(!io_err.is_not_found());
    /// ```
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::PathNotFound { .. } | Self::InodeNotFound { .. })
    }

    /// Check if this is an I/O error.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::OutputError;
    ///
    /// let err = OutputError::io(std::io::Error::new(
    ///     std::io::ErrorKind::NotFound,
    ///     "not found"
    /// ));
    /// assert!(err.is_io());
    /// ```
    pub fn is_io(&self) -> bool {
        matches!(self, Self::Io(_))
    }

    /// Check if this is a working copy error.
    pub fn is_working_copy(&self) -> bool {
        matches!(self, Self::WorkingCopy(_))
    }

    /// Check if this is a change store error.
    pub fn is_change_store(&self) -> bool {
        matches!(self, Self::ChangeStore(_))
    }

    /// Check if this is a graph error.
    pub fn is_graph(&self) -> bool {
        matches!(self, Self::Graph(_))
    }

    /// Check if this is a pristine database error.
    pub fn is_pristine(&self) -> bool {
        matches!(self, Self::Pristine(_))
    }
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {}", err),
            Self::WorkingCopy(err) => write!(f, "Working copy error: {}", err),
            Self::ChangeStore(err) => write!(f, "Change store error: {}", err),
            Self::Graph(err) => write!(f, "Graph error: {}", err),
            Self::Pristine(err) => write!(f, "Pristine error: {}", err),
            Self::PathNotFound { path } => write!(f, "Path not found: {}", path),
            Self::InodeNotFound { inode } => write!(f, "Inode not found: {:?}", inode),
        }
    }
}

impl std::error::Error for OutputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::WorkingCopy(err) => Some(err.as_ref()),
            Self::ChangeStore(err) => Some(err.as_ref()),
            Self::Graph(err) => Some(err.as_ref()),
            Self::Pristine(err) => Some(err.as_ref()),
            Self::PathNotFound { .. } | Self::InodeNotFound { .. } => None,
        }
    }
}

impl From<std::io::Error> for OutputError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<PristineError> for OutputError {
    fn from(err: PristineError) -> Self {
        Self::Pristine(Box::new(err))
    }
}

// RESULT TYPE ALIAS

/// Result type for output operations.
///
/// This is a convenience alias for `Result<T, OutputError>`.
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::{OutputResult, OutputError};
///
/// fn do_output() -> OutputResult<usize> {
///     Ok(42)
/// }
///
/// fn failing_output() -> OutputResult<()> {
///     Err(OutputError::path_not_found("missing.rs"))
/// }
/// ```
pub type OutputResult<T> = Result<T, OutputError>;

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    // ------------------------------------------------------------------------
    // Constructor Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_io_error() {
        let err = OutputError::io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        ));

        assert!(err.is_io());
        assert!(!err.is_not_found());
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn test_working_copy_error() {
        let err = OutputError::working_copy(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        ));

        assert!(err.is_working_copy());
        assert!(err.to_string().contains("Working copy"));
    }

    #[test]
    fn test_change_store_error() {
        let err = OutputError::change_store(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "change not found",
        ));

        assert!(err.is_change_store());
        assert!(err.to_string().contains("Change store"));
    }

    #[test]
    fn test_graph_error() {
        let err = OutputError::graph(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "corrupted",
        ));

        assert!(err.is_graph());
        assert!(err.to_string().contains("Graph"));
    }

    #[test]
    fn test_path_not_found() {
        let err = OutputError::path_not_found("src/main.rs");

        assert!(err.is_not_found());
        assert!(!err.is_io());
        assert!(err.to_string().contains("src/main.rs"));
    }

    #[test]
    fn test_inode_not_found() {
        let err = OutputError::inode_not_found(Inode::new(42));

        assert!(err.is_not_found());
        assert!(err.to_string().contains("Inode not found"));
    }

    // ------------------------------------------------------------------------
    // From Trait Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let err: OutputError = io_err.into();

        assert!(err.is_io());
    }

    #[test]
    fn test_from_pristine_error() {
        let pristine_err = PristineError::ViewNotFound {
            name: "test".to_string(),
        };
        let err: OutputError = pristine_err.into();

        assert!(err.is_pristine());
    }

    // ------------------------------------------------------------------------
    // Error Source Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_error_source_io() {
        let err = OutputError::io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));

        assert!(err.source().is_some());
    }

    #[test]
    fn test_error_source_path_not_found() {
        let err = OutputError::path_not_found("test.rs");

        assert!(err.source().is_none());
    }

    // ------------------------------------------------------------------------
    // Display Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_display_io() {
        let err = OutputError::io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        ));
        let display = err.to_string();

        assert!(display.contains("I/O error"));
        assert!(display.contains("file not found"));
    }

    #[test]
    fn test_display_working_copy() {
        let err =
            OutputError::working_copy(std::io::Error::new(std::io::ErrorKind::Other, "failed"));
        let display = err.to_string();

        assert!(display.contains("Working copy"));
    }

    #[test]
    fn test_display_change_store() {
        let err =
            OutputError::change_store(std::io::Error::new(std::io::ErrorKind::Other, "failed"));
        let display = err.to_string();

        assert!(display.contains("Change store"));
    }

    #[test]
    fn test_display_graph() {
        let err = OutputError::graph(std::io::Error::new(std::io::ErrorKind::Other, "failed"));
        let display = err.to_string();

        assert!(display.contains("Graph"));
    }

    #[test]
    fn test_display_path_not_found() {
        let err = OutputError::path_not_found("missing/path.rs");
        let display = err.to_string();

        assert!(display.contains("Path not found"));
        assert!(display.contains("missing/path.rs"));
    }

    #[test]
    fn test_display_inode_not_found() {
        let err = OutputError::inode_not_found(Inode::new(123));
        let display = err.to_string();

        assert!(display.contains("Inode not found"));
    }

    // ------------------------------------------------------------------------
    // Debug Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_debug() {
        let err = OutputError::path_not_found("test.rs");
        let debug_str = format!("{:?}", err);

        assert!(debug_str.contains("PathNotFound"));
        assert!(debug_str.contains("test.rs"));
    }

    // ------------------------------------------------------------------------
    // Result Type Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_output_result_ok() {
        let result: OutputResult<i32> = Ok(42);
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_output_result_err() {
        let result: OutputResult<()> = Err(OutputError::path_not_found("test.rs"));
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------------
    // Type Check Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_all_type_checks() {
        let io = OutputError::io(std::io::Error::new(std::io::ErrorKind::Other, ""));
        assert!(io.is_io());
        assert!(!io.is_working_copy());
        assert!(!io.is_change_store());
        assert!(!io.is_graph());
        assert!(!io.is_pristine());
        assert!(!io.is_not_found());

        let wc = OutputError::working_copy(std::io::Error::new(std::io::ErrorKind::Other, ""));
        assert!(!wc.is_io());
        assert!(wc.is_working_copy());

        let cs = OutputError::change_store(std::io::Error::new(std::io::ErrorKind::Other, ""));
        assert!(cs.is_change_store());

        let g = OutputError::graph(std::io::Error::new(std::io::ErrorKind::Other, ""));
        assert!(g.is_graph());

        let p: OutputError = PristineError::ViewNotFound {
            name: "test".to_string(),
        }
        .into();
        assert!(p.is_pristine());

        let pnf = OutputError::path_not_found("test");
        assert!(pnf.is_not_found());

        let inf = OutputError::inode_not_found(Inode::ROOT);
        assert!(inf.is_not_found());
    }
}

//! Error types for the pristine storage layer
//!
//! This module defines error types that can occur during pristine database
//! operations. The error hierarchy is designed to provide clear, actionable
//! error messages while preserving the underlying cause for debugging.
//!
//! # Error Categories
//!
//! Errors are broadly categorized into:
//!
//! - **Database Errors**: Low-level redb errors (transaction, table, storage, commit)
//! - **Not Found Errors**: Missing stacks, changes, or hashes
//! - **Data Errors**: Invalid vertices, blocks, or corrupted data
//! - **Serialization Errors**: Failed to encode/decode data
//!
//! # Error Handling Pattern
//!
//! ```ignore
//! use atomic_core::pristine::{PristineResult, PristineError};
//!
//! fn get_stack_info(pristine: &Pristine, name: &str) -> PristineResult<String> {
//!     let txn = pristine.read_txn()?;  // Propagates database errors
//!
//!     let stack = txn.get_stack(name)?
//!         .ok_or_else(|| PristineError::StackNotFound {
//!             name: name.to_string()
//!         })?;
//!
//!     Ok(format!("Stack '{}' has {} changes", stack.name, stack.change_count))
//! }
//! ```
//!
//! # Error Conversion
//!
//! All redb error types implement `From` for `PristineError`, enabling
//! seamless use of the `?` operator:
//!
//! ```ignore
//! // These all work with `?`
//! let db = Database::create(path)?;           // DatabaseError → PristineError
//! let txn = db.begin_write()?;                // TransactionError → PristineError
//! let table = txn.open_table(TABLE)?;         // TableError → PristineError
//! txn.commit()?;                              // CommitError → PristineError
//! ```

use std::fmt;

/// Errors that can occur in pristine operations
///
/// This enum covers all error conditions that can arise when interacting
/// with the pristine database. Each variant includes contextual information
/// to help diagnose the issue.
///
/// # Examples
///
/// ```
/// use atomic_core::pristine::PristineError;
///
/// // Create a "not found" error
/// let err = PristineError::StackNotFound {
///     name: "feature-branch".to_string(),
/// };
/// assert!(err.to_string().contains("feature-branch"));
///
/// // Create a data error
/// let err = PristineError::BlockNotFound { change: 42, pos: 100 };
/// assert!(err.to_string().contains("42"));
/// ```
#[derive(Debug)]
pub enum PristineError {
    // =========================================================================
    // Database Errors (from redb)
    // =========================================================================
    /// Error opening or creating the database
    ///
    /// This typically occurs when:
    /// - The path is invalid or inaccessible
    /// - The database file is corrupted
    /// - Insufficient permissions
    Database(redb::DatabaseError),

    /// Error beginning or managing a transaction
    ///
    /// This typically occurs when:
    /// - Another write transaction is active (for write txn)
    /// - The database is closed
    Transaction(redb::TransactionError),

    /// Error opening or accessing a table
    ///
    /// This typically occurs when:
    /// - Table doesn't exist (shouldn't happen with our init)
    /// - Table schema mismatch
    Table(redb::TableError),

    /// Error during storage operations (read/write)
    ///
    /// This typically occurs when:
    /// - Disk I/O failure
    /// - Memory mapping issues
    /// - Data corruption
    Storage(redb::StorageError),

    /// Error committing a transaction
    ///
    /// This typically occurs when:
    /// - Disk full
    /// - I/O error during flush
    Commit(redb::CommitError),

    // =========================================================================
    // I/O Errors
    // =========================================================================
    /// General I/O error (file operations, etc.)
    Io(std::io::Error),

    // =========================================================================
    // Not Found Errors
    // =========================================================================
    /// Stack (view) not found in the database
    ///
    /// The requested stack name doesn't exist. This is common when:
    /// - Accessing a stack that hasn't been created
    /// - Typo in stack name
    /// - Stack was deleted
    StackNotFound {
        /// The name of the stack that wasn't found
        name: String,
    },

    /// Change not found by its internal ID
    ///
    /// The NodeId doesn't correspond to any registered change.
    ChangeNotFound {
        /// The internal ID that wasn't found
        id: u64,
    },

    /// Hash not found in the external→internal mapping
    ///
    /// The content hash isn't registered in this repository.
    HashNotFound {
        /// Base32 representation of the hash (truncated for display)
        hash: String,
    },

    // =========================================================================
    // Data Errors
    // =========================================================================
    /// Invalid span structure
    ///
    /// The span data is malformed or inconsistent.
    InvalidVertex {
        /// Description of what's wrong with the span
        message: String,
    },

    /// Block not found for the given position
    ///
    /// When navigating the graph, no span was found containing
    /// the specified position. This may indicate:
    /// - Corrupted graph data
    /// - Position from a different repository
    /// - Bug in graph construction
    BlockNotFound {
        /// The change ID being searched
        change: u64,
        /// The byte position being searched
        pos: u64,
    },

    /// Database state is inconsistent
    ///
    /// Internal invariants have been violated. This is a serious error
    /// that may indicate data corruption or a bug in the code.
    Inconsistent {
        /// Description of the inconsistency
        message: String,
    },

    // =========================================================================
    // Serialization Errors
    // =========================================================================
    /// Failed to serialize or deserialize data
    ///
    /// The data couldn't be encoded or decoded. This may indicate:
    /// - Corrupted stored data
    /// - Version mismatch
    /// - Invalid UTF-8 in strings
    Serialization {
        /// Description of the serialization failure
        message: String,
    },
}

impl fmt::Display for PristineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Database errors
            Self::Database(e) => write!(f, "database error: {}", e),
            Self::Transaction(e) => write!(f, "transaction error: {}", e),
            Self::Table(e) => write!(f, "table error: {}", e),
            Self::Storage(e) => write!(f, "storage error: {}", e),
            Self::Commit(e) => write!(f, "commit error: {}", e),

            // I/O errors
            Self::Io(e) => write!(f, "IO error: {}", e),

            // Not found errors
            Self::StackNotFound { name } => write!(f, "stack not found: {}", name),
            Self::ChangeNotFound { id } => write!(f, "change not found: {}", id),
            Self::HashNotFound { hash } => write!(f, "hash not found: {}", hash),

            // Data errors
            Self::InvalidVertex { message } => write!(f, "invalid node: {}", message),
            Self::BlockNotFound { change, pos } => {
                write!(f, "block not found for position {}:{}", change, pos)
            }
            Self::Inconsistent { message } => write!(f, "inconsistent state: {}", message),

            // Serialization errors
            Self::Serialization { message } => write!(f, "serialization error: {}", message),
        }
    }
}

impl std::error::Error for PristineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(e) => Some(e),
            Self::Transaction(e) => Some(e),
            Self::Table(e) => Some(e),
            Self::Storage(e) => Some(e),
            Self::Commit(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

// =============================================================================
// From Implementations for Error Conversion
// =============================================================================

impl From<redb::DatabaseError> for PristineError {
    fn from(e: redb::DatabaseError) -> Self {
        Self::Database(e)
    }
}

impl From<redb::TransactionError> for PristineError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Transaction(e)
    }
}

impl From<redb::TableError> for PristineError {
    fn from(e: redb::TableError) -> Self {
        Self::Table(e)
    }
}

impl From<redb::StorageError> for PristineError {
    fn from(e: redb::StorageError) -> Self {
        Self::Storage(e)
    }
}

impl From<redb::CommitError> for PristineError {
    fn from(e: redb::CommitError) -> Self {
        Self::Commit(e)
    }
}

impl From<std::io::Error> for PristineError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// =============================================================================
// Result Type Alias
// =============================================================================

/// Result type for pristine operations
///
/// This is a convenience alias that uses `PristineError` as the error type.
///
/// # Example
///
/// ```ignore
/// use atomic_core::pristine::{PristineResult, Pristine};
///
/// fn open_database(path: &str) -> PristineResult<Pristine> {
///     Pristine::open(path)
/// }
/// ```
pub type PristineResult<T> = Result<T, PristineError>;

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn test_error_display_stack_not_found() {
        let err = PristineError::StackNotFound {
            name: "main".to_string(),
        };
        assert_eq!(err.to_string(), "stack not found: main");
    }

    #[test]
    fn test_error_display_change_not_found() {
        let err = PristineError::ChangeNotFound { id: 42 };
        assert_eq!(err.to_string(), "change not found: 42");
    }

    #[test]
    fn test_error_display_block_not_found() {
        let err = PristineError::BlockNotFound { change: 1, pos: 100 };
        assert_eq!(err.to_string(), "block not found for position 1:100");
    }

    #[test]
    fn test_error_display_hash_not_found() {
        let err = PristineError::HashNotFound {
            hash: "ABCD1234".to_string(),
        };
        assert_eq!(err.to_string(), "hash not found: ABCD1234");
    }

    #[test]
    fn test_error_display_invalid_vertex() {
        let err = PristineError::InvalidVertex {
            message: "start > end".to_string(),
        };
        assert_eq!(err.to_string(), "invalid node: start > end");
    }

    #[test]
    fn test_error_display_inconsistent() {
        let err = PristineError::Inconsistent {
            message: "orphaned edge".to_string(),
        };
        assert_eq!(err.to_string(), "inconsistent state: orphaned edge");
    }

    #[test]
    fn test_error_display_serialization() {
        let err = PristineError::Serialization {
            message: "invalid UTF-8".to_string(),
        };
        assert_eq!(err.to_string(), "serialization error: invalid UTF-8");
    }

    #[test]
    fn test_error_is_error_trait() {
        // Verify that PristineError implements std::error::Error
        let err: Box<dyn std::error::Error> = Box::new(PristineError::Inconsistent {
            message: "test".to_string(),
        });
        assert!(err.to_string().contains("inconsistent"));
    }

    #[test]
    fn test_error_source_returns_none_for_custom_errors() {
        let err = PristineError::StackNotFound {
            name: "test".to_string(),
        };
        assert!(err.source().is_none());
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let pristine_err: PristineError = io_err.into();

        match pristine_err {
            PristineError::Io(_) => {} // Expected
            _ => panic!("Expected Io variant"),
        }
    }
}

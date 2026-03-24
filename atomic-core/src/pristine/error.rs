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
    // Database Errors (from redb)
    /// Error opening or creating the database
    ///
    /// This typically occurs when:
    /// - The path is invalid or inaccessible
    /// - The database file is corrupted
    /// - Insufficient permissions
    Database(Box<redb::DatabaseError>),

    /// Error beginning or managing a transaction
    ///
    /// This typically occurs when:
    /// - Another write transaction is active (for write txn)
    /// - The database is closed
    Transaction(Box<redb::TransactionError>),

    /// Error opening or accessing a table
    ///
    /// This typically occurs when:
    /// - Table doesn't exist (shouldn't happen with our init)
    /// - Table schema mismatch
    Table(Box<redb::TableError>),

    /// Error during storage operations (read/write)
    ///
    /// This typically occurs when:
    /// - Disk I/O failure
    /// - Memory mapping issues
    /// - Data corruption
    Storage(Box<redb::StorageError>),

    /// Error committing a transaction
    ///
    /// This typically occurs when:
    /// - Disk full
    /// - I/O error during flush
    Commit(Box<redb::CommitError>),

    // I/O Errors
    /// General I/O error (file operations, etc.)
    Io(std::io::Error),

    // Not Found Errors
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

    /// A stack with this name already exists
    ///
    /// Returned by `MutTxnT::create_stack` when attempting to create a
    /// stack whose name is already taken. Use `MutTxnT::open_or_create_stack`
    /// if "get or create" semantics are desired.
    StackAlreadyExists {
        /// The name of the stack that already exists
        name: String,
    },

    /// Cannot delete a shared stack
    ///
    /// Shared stacks (dev, release, main) write edges to the global `GRAPH`
    /// table and are the canonical record of promoted history. Deleting them
    /// would orphan edges in the global graph. Use `--force` with explicit
    /// cleanup if this is intentional.
    CannotDeleteSharedStack {
        /// The name of the shared stack
        name: String,
    },

    /// Cannot perform operation because the stack has child stacks
    ///
    /// An local workspace that other stacks reference as their parent cannot
    /// be deleted without first reparenting or deleting its children.
    StackHasChildren {
        /// The name of the stack that has children
        name: String,
        /// Names of the child stacks
        children: Vec<String>,
    },

    /// Parent stack cycle detected
    ///
    /// Setting the given parent would create a cycle in the stack ancestry
    /// chain (e.g., A → B → C → A). The overlay chain must be acyclic.
    StackCycleDetected {
        /// The stack being created or reparented
        name: String,
        /// The parent that would create a cycle
        parent_name: String,
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

    // Data Errors
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

    // Serialization Errors
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
            Self::StackAlreadyExists { name } => {
                write!(f, "stack already exists: {}", name)
            }
            Self::CannotDeleteSharedStack { name } => {
                write!(
                    f,
                    "cannot delete shared stack '{}': shared stacks are permanent",
                    name
                )
            }
            Self::StackHasChildren { name, children } => {
                write!(
                    f,
                    "cannot delete stack '{}': has child stacks: {}",
                    name,
                    children.join(", ")
                )
            }
            Self::StackCycleDetected { name, parent_name } => {
                write!(
                    f,
                    "cannot set parent of '{}' to '{}': would create a cycle",
                    name, parent_name
                )
            }
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
            Self::Database(e) => Some(e.as_ref()),
            Self::Transaction(e) => Some(e.as_ref()),
            Self::Table(e) => Some(e.as_ref()),
            Self::Storage(e) => Some(e.as_ref()),
            Self::Commit(e) => Some(e.as_ref()),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

// From Implementations for Error Conversion

impl From<redb::DatabaseError> for PristineError {
    fn from(e: redb::DatabaseError) -> Self {
        Self::Database(Box::new(e))
    }
}

impl From<redb::TransactionError> for PristineError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Transaction(Box::new(e))
    }
}

impl From<redb::TableError> for PristineError {
    fn from(e: redb::TableError) -> Self {
        Self::Table(Box::new(e))
    }
}

impl From<redb::StorageError> for PristineError {
    fn from(e: redb::StorageError) -> Self {
        Self::Storage(Box::new(e))
    }
}

impl From<redb::CommitError> for PristineError {
    fn from(e: redb::CommitError) -> Self {
        Self::Commit(Box::new(e))
    }
}

impl From<std::io::Error> for PristineError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// Result Type Alias

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

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn display_messages_include_context() {
        // Each variant should surface the data that helps a user diagnose the problem.
        let cases: Vec<(PristineError, &[&str])> = vec![
            (
                PristineError::StackNotFound {
                    name: "feature-x".into(),
                },
                &["stack not found", "feature-x"],
            ),
            (
                PristineError::StackAlreadyExists { name: "dev".into() },
                &["stack already exists", "dev"],
            ),
            (
                PristineError::CannotDeleteSharedStack {
                    name: "main".into(),
                },
                &["cannot delete shared stack", "main"],
            ),
            (
                PristineError::StackHasChildren {
                    name: "service-auth".into(),
                    children: vec!["feature-login".into(), "bug-fix".into()],
                },
                &[
                    "cannot delete stack",
                    "service-auth",
                    "feature-login",
                    "bug-fix",
                ],
            ),
            (
                PristineError::StackCycleDetected {
                    name: "a".into(),
                    parent_name: "b".into(),
                },
                &["cannot set parent", "a", "b", "cycle"],
            ),
            (
                PristineError::ChangeNotFound { id: 42 },
                &["change not found", "42"],
            ),
            (
                PristineError::HashNotFound {
                    hash: "ABCD1234".into(),
                },
                &["hash not found", "ABCD1234"],
            ),
            (
                PristineError::BlockNotFound {
                    change: 7,
                    pos: 256,
                },
                &["block not found", "7", "256"],
            ),
            (
                PristineError::InvalidVertex {
                    message: "start > end".into(),
                },
                &["invalid node", "start > end"],
            ),
            (
                PristineError::Inconsistent {
                    message: "orphaned edge".into(),
                },
                &["inconsistent", "orphaned edge"],
            ),
            (
                PristineError::Serialization {
                    message: "invalid UTF-8".into(),
                },
                &["serialization", "invalid UTF-8"],
            ),
        ];

        for (err, expected_fragments) in cases {
            let msg = err.to_string();
            for fragment in expected_fragments {
                assert!(msg.contains(fragment), "{msg:?} missing {fragment:?}");
            }
        }
    }

    #[test]
    fn io_error_preserves_source_chain() {
        let original = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err: PristineError = original.into();

        // The source chain should be accessible for logging/debugging.
        let source = err.source().expect("Io variant should expose source");
        assert!(source.to_string().contains("access denied"));
    }

    #[test]
    fn custom_variants_have_no_source() {
        // Non-wrapper variants shouldn't claim a source error exists.
        let custom_errors: Vec<PristineError> = vec![
            PristineError::StackNotFound { name: "x".into() },
            PristineError::StackAlreadyExists { name: "x".into() },
            PristineError::CannotDeleteSharedStack { name: "x".into() },
            PristineError::StackHasChildren {
                name: "x".into(),
                children: vec!["y".into()],
            },
            PristineError::StackCycleDetected {
                name: "a".into(),
                parent_name: "b".into(),
            },
            PristineError::ChangeNotFound { id: 1 },
            PristineError::HashNotFound { hash: "x".into() },
            PristineError::InvalidVertex {
                message: "x".into(),
            },
            PristineError::BlockNotFound { change: 0, pos: 0 },
            PristineError::Inconsistent {
                message: "x".into(),
            },
            PristineError::Serialization {
                message: "x".into(),
            },
        ];

        for err in custom_errors {
            assert!(err.source().is_none(), "{err:?} should not have a source");
        }
    }

    #[test]
    fn from_conversions_round_trip_through_question_mark() {
        // Simulate the ? operator flow: redb/io errors -> PristineResult
        fn fallible_io() -> PristineResult<()> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"))?
        }

        let result = fallible_io();
        assert!(matches!(result, Err(PristineError::Io(_))));
    }

    #[test]
    fn errors_are_send_and_sync() {
        // PristineError must be thread-safe for async storage backends.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PristineError>();
    }
}

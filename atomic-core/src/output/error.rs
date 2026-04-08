//! Error types for working copy output operations
//!
//! This module defines the error hierarchy for outputting graph state to the
//! working copy (file system). Errors can occur during:
//!
//! - File tree traversal
//! - Content retrieval from changes
//! - File system operations
//! - Conflict detection and handling
//!
//! # Error Hierarchy
//!
//! ```text
//! OutputError (top-level)
//! ├── Pristine errors (database/graph issues)
//! │   ├── Graph traversal failures
//! │   ├── Missing vertices or edges
//! │   └── Transaction errors
//! ├── WorkingCopy errors (file system issues)
//! │   ├── Permission denied
//! │   ├── File not found
//! │   └── I/O errors
//! └── ChangeStore errors (change file issues)
//!     ├── Change not found
//!     ├── Corrupted content
//!     └── Hash mismatch
//! ```
//!
//! # Design Philosophy
//!
//! The error types are designed to:
//!
//! 1. **Preserve context**: Each error carries information about what went wrong
//! 2. **Enable recovery**: Errors distinguish between recoverable and fatal issues
//! 3. **Support debugging**: Error messages include relevant identifiers
//! 4. **Chain causes**: Underlying errors are preserved via `#[from]` attributes
//!
//! # Example
//!
//! ```rust
//! use atomic_core::output::{OutputError, OutputResult};
//!
//! fn output_file(path: &str) -> OutputResult<()> {
//!     // Simulate an I/O error
//!     Err(OutputError::Io(std::io::Error::new(
//!         std::io::ErrorKind::PermissionDenied,
//!         "Cannot write to file",
//!     )))
//! }
//!
//! match output_file("/some/path") {
//!     Ok(()) => println!("File written successfully"),
//!     Err(OutputError::Io(e)) => eprintln!("I/O error: {}", e),
//!     Err(e) => eprintln!("Other error: {}", e),
//! }
//! ```

use crate::pristine::PristineError;
use crate::types::{GraphNode, Hash, Inode, NodeId, Position};
use std::path::PathBuf;
use thiserror::Error;

/// Top-level error type for output operations.
///
/// This enum encompasses all errors that can occur during the process of
/// outputting the graph state to the working copy.
#[derive(Debug, Error)]
pub enum OutputError {
    /// Error from the pristine database layer.
    ///
    /// This includes graph traversal failures, missing data, and
    /// transaction issues.
    #[error("Pristine error: {0}")]
    Pristine(#[from] PristineError),

    /// I/O error during file system operations.
    ///
    /// This includes read/write failures, permission issues, and
    /// missing directories.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Error reading change contents.
    ///
    /// The change file could not be read or is corrupted.
    #[error("Change store error: {message}")]
    ChangeStore {
        /// Description of what went wrong
        message: String,
        /// The hash of the change that caused the error, if known
        hash: Option<Hash>,
    },

    /// A graph node was not found in the graph.
    ///
    /// This typically indicates a corrupted graph or incomplete sync.
    #[error("GraphNode not found: {node:?} in change {change_id:?}")]
    NodeNotFound {
        /// The graph node that could not be found
        node: GraphNode<NodeId>,
        /// The change that should contain it
        change_id: Option<NodeId>,
    },

    /// Position could not be resolved to a graph node.
    ///
    /// This happens when an edge points to a non-existent position.
    #[error("Position not found: {position:?}")]
    PositionNotFound {
        /// The position that could not be resolved
        position: Position<NodeId>,
    },

    /// An inode could not be resolved to a path.
    ///
    /// The file tree mapping is incomplete or corrupted.
    #[error("Inode {inode:?} has no corresponding path")]
    InodeNotFound {
        /// The inode that could not be resolved
        inode: Inode,
    },

    /// Path could not be resolved to an inode.
    ///
    /// The requested path does not exist in the repository.
    #[error("Path not found: {path}")]
    PathNotFound {
        /// The path that could not be resolved
        path: String,
    },

    /// File content encoding error.
    ///
    /// The content could not be decoded from the stored format.
    #[error("Encoding error for file {path}: {message}")]
    Encoding {
        /// The file path
        path: String,
        /// Description of the encoding issue
        message: String,
    },

    /// The file is not writable.
    ///
    /// The working copy indicates this path should not be modified.
    #[error("File is not writable: {path}")]
    NotWritable {
        /// The path that cannot be written
        path: String,
    },

    /// Conflict detected during output.
    ///
    /// Multiple valid orderings exist for the file content.
    #[error("Conflict in file {path}: {conflict_type}")]
    Conflict {
        /// The file path
        path: String,
        /// Type of conflict (order, zombie, cyclic)
        conflict_type: ConflictType,
        /// Number of conflicting sides
        sides: usize,
    },

    /// A required change is missing from the change store.
    ///
    /// This prevents content retrieval for some vertices.
    #[error("Missing change: {hash}")]
    MissingChange {
        /// The hash of the missing change
        hash: Hash,
    },

    /// Internal error that should not occur.
    ///
    /// This indicates a bug in the output code.
    #[error("Internal error: {message}")]
    Internal {
        /// Description of the internal error
        message: String,
    },
}

/// Types of conflicts that can occur during output.
///
/// These correspond to different kinds of ambiguity in the graph that
/// require human intervention to resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictType {
    /// Multiple valid orderings for content.
    ///
    /// Two or more changes inserted content at the same position,
    /// and there's no clear ordering.
    Order,

    /// Deleted content that still has live connections.
    ///
    /// Content was deleted by one change but modified by another.
    Zombie,

    /// Cyclic dependencies in the graph.
    ///
    /// The graph has a cycle that prevents linear output.
    Cyclic,

    /// Multiple names for the same file.
    ///
    /// The file has been renamed by concurrent changes.
    Name,
}

impl std::fmt::Display for ConflictType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConflictType::Order => write!(f, "order conflict"),
            ConflictType::Zombie => write!(f, "zombie conflict"),
            ConflictType::Cyclic => write!(f, "cyclic conflict"),
            ConflictType::Name => write!(f, "name conflict"),
        }
    }
}

/// Result type alias for output operations.
pub type OutputResult<T> = Result<T, OutputError>;

/// Error that occurred during file content retrieval.
///
/// This is a more specific error for content-related failures,
/// which can be converted to the broader `OutputError`.
#[derive(Debug, Error)]
pub enum ContentError {
    /// I/O error reading change file.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Change file not found.
    #[error("Change not found: {hash}")]
    ChangeNotFound {
        /// The hash of the missing change
        hash: Hash,
    },

    /// Content is truncated or corrupted.
    #[error("Content truncated: expected {expected} bytes, got {actual}")]
    Truncated {
        /// Expected content length
        expected: usize,
        /// Actual content length
        actual: usize,
    },

    /// Graph node content could not be retrieved.
    #[error("Failed to get contents for node {node:?}")]
    VertexContent {
        /// The node that failed
        node: GraphNode<NodeId>,
    },
}

impl From<ContentError> for OutputError {
    fn from(err: ContentError) -> Self {
        match err {
            ContentError::Io(e) => OutputError::Io(e),
            ContentError::ChangeNotFound { hash } => OutputError::MissingChange { hash },
            ContentError::Truncated { expected, actual } => OutputError::ChangeStore {
                message: format!(
                    "Content truncated: expected {} bytes, got {}",
                    expected, actual
                ),
                hash: None,
            },
            ContentError::VertexContent { node } => OutputError::NodeNotFound {
                node,
                change_id: Some(node.change),
            },
        }
    }
}

/// Result type alias for content operations.
pub type ContentResult<T> = Result<T, ContentError>;

/// Error during tree traversal.
///
/// This captures issues that occur while walking the file tree structure.
#[derive(Debug, Error)]
pub enum TreeError {
    /// Pristine database error.
    #[error("Pristine error: {0}")]
    Pristine(#[from] PristineError),

    /// A directory entry points to a non-existent inode.
    #[error("Orphan directory entry: {name} -> {inode:?}")]
    OrphanEntry {
        /// Name of the directory entry
        name: String,
        /// The inode it points to
        inode: Inode,
    },

    /// Circular reference in the file tree.
    #[error("Circular reference detected at path: {path}")]
    CircularReference {
        /// The path where the cycle was detected
        path: PathBuf,
    },

    /// Maximum tree depth exceeded.
    #[error("Maximum tree depth exceeded: {depth} (limit: {limit})")]
    MaxDepthExceeded {
        /// Current depth
        depth: usize,
        /// Maximum allowed depth
        limit: usize,
    },
}

impl From<TreeError> for OutputError {
    fn from(err: TreeError) -> Self {
        match err {
            TreeError::Pristine(e) => OutputError::Pristine(e),
            TreeError::OrphanEntry { name, inode } => OutputError::Internal {
                message: format!("Orphan directory entry: {} -> {:?}", name, inode),
            },
            TreeError::CircularReference { path } => OutputError::Internal {
                message: format!("Circular reference at: {}", path.display()),
            },
            TreeError::MaxDepthExceeded { depth, limit } => OutputError::Internal {
                message: format!("Tree depth {} exceeds limit {}", depth, limit),
            },
        }
    }
}

/// Result type alias for tree operations.
pub type TreeResult<T> = Result<T, TreeError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    // -- ContentError → OutputError conversion: this is the real contract.
    //    The output layer receives content errors from the change store and
    //    must map them to the right OutputError variant so callers can
    //    distinguish "missing change" from "corrupted content" from "I/O".

    #[test]
    fn content_io_error_surfaces_as_output_io() {
        let content_err =
            ContentError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "boom"));
        let output_err: OutputError = content_err.into();
        assert!(
            matches!(output_err, OutputError::Io(_)),
            "IO errors must stay as IO so callers can retry"
        );
    }

    #[test]
    fn content_not_found_becomes_missing_change() {
        let hash = Hash::of(b"some change content");
        let content_err = ContentError::ChangeNotFound { hash };
        let output_err: OutputError = content_err.into();

        // This distinction matters: MissingChange triggers a fetch in sync,
        // while ChangeStore errors indicate local corruption.
        match output_err {
            OutputError::MissingChange { hash: h } => assert_eq!(h, hash),
            other => panic!("expected MissingChange, got {other}"),
        }
    }

    #[test]
    fn content_truncated_becomes_change_store_error() {
        let content_err = ContentError::Truncated {
            expected: 1024,
            actual: 512,
        };
        let output_err: OutputError = content_err.into();

        match output_err {
            OutputError::ChangeStore { message, .. } => {
                assert!(message.contains("1024"), "should mention expected size");
                assert!(message.contains("512"), "should mention actual size");
            }
            other => panic!("expected ChangeStore, got {other}"),
        }
    }

    #[test]
    fn content_vertex_error_carries_change_id_for_debugging() {
        let node = GraphNode {
            change: NodeId::new(42),
            start: ChangePosition::new(0),
            end: ChangePosition::new(100),
        };
        let content_err = ContentError::VertexContent { node };
        let output_err: OutputError = content_err.into();

        match output_err {
            OutputError::NodeNotFound { change_id, .. } => {
                assert_eq!(change_id, Some(NodeId::new(42)));
            }
            other => panic!("expected NodeNotFound, got {other}"),
        }
    }

    // -- TreeError → OutputError conversion: tree traversal errors that
    //    aren't pristine-related become Internal errors (they indicate bugs).

    #[test]
    fn tree_pristine_error_stays_pristine_not_internal() {
        let tree_err = TreeError::Pristine(PristineError::HashNotFound { hash: "abc".into() });
        let output_err: OutputError = tree_err.into();
        assert!(
            matches!(output_err, OutputError::Pristine(_)),
            "pristine errors must not be downgraded to Internal"
        );
    }

    #[test]
    fn tree_structural_errors_become_internal() {
        // Orphan entries, cycles, and depth overflows are bugs, not user errors.
        let structural_errors: Vec<TreeError> = vec![
            TreeError::OrphanEntry {
                name: "ghost.txt".into(),
                inode: Inode::new(999),
            },
            TreeError::CircularReference {
                path: PathBuf::from("/a/b/a"),
            },
            TreeError::MaxDepthExceeded {
                depth: 500,
                limit: 100,
            },
        ];

        for tree_err in structural_errors {
            let desc = format!("{tree_err}");
            let output_err: OutputError = tree_err.into();
            assert!(
                matches!(output_err, OutputError::Internal { .. }),
                "{desc} should become Internal"
            );
        }
    }

    // -- Error propagation chains: verify ? works across the full hierarchy.

    #[test]
    fn io_chains_through_content_to_output() {
        fn read_content() -> ContentResult<Vec<u8>> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"))?
        }

        fn output_file() -> OutputResult<Vec<u8>> {
            Ok(read_content()?)
        }

        // The IO error must survive two ? hops without being swallowed.
        assert!(matches!(output_file(), Err(OutputError::Io(_))));
    }

    #[test]
    fn pristine_chains_through_tree_to_output() {
        fn walk_tree() -> TreeResult<()> {
            Err(PristineError::ViewNotFound {
                name: "main".into(),
            })?
        }

        fn output_repo() -> OutputResult<()> {
            walk_tree()?;
            Ok(())
        }

        assert!(matches!(output_repo(), Err(OutputError::Pristine(_))));
    }

    // -- ConflictType: used as a key in conflict tracking maps.

    #[test]
    fn conflict_types_are_distinct_in_hash_sets() {
        use std::collections::HashSet;
        let all = [
            ConflictType::Order,
            ConflictType::Zombie,
            ConflictType::Cyclic,
            ConflictType::Name,
        ];
        let set: HashSet<_> = all.iter().copied().collect();
        assert_eq!(set.len(), 4, "all four conflict types must be distinct");

        // Duplicates should collapse.
        let mut with_dup: HashSet<ConflictType> = set;
        with_dup.insert(ConflictType::Order);
        assert_eq!(with_dup.len(), 4);
    }

    #[test]
    fn conflict_type_display_is_human_readable() {
        // These strings end up in conflict markers that users see in files.
        assert_eq!(ConflictType::Order.to_string(), "order conflict");
        assert_eq!(ConflictType::Zombie.to_string(), "zombie conflict");
        assert_eq!(ConflictType::Cyclic.to_string(), "cyclic conflict");
        assert_eq!(ConflictType::Name.to_string(), "name conflict");
    }

    // -- Display messages: verify they carry actionable context.

    #[test]
    fn conflict_display_includes_path_and_type() {
        let err = OutputError::Conflict {
            path: "src/main.rs".into(),
            conflict_type: ConflictType::Order,
            sides: 3,
        };
        let msg = err.to_string();
        assert!(msg.contains("src/main.rs"), "must show which file: {msg}");
        assert!(msg.contains("order conflict"), "must show kind: {msg}");
    }

    #[test]
    fn encoding_error_shows_file_and_reason() {
        let err = OutputError::Encoding {
            path: "data/日本語.txt".into(),
            message: "invalid UTF-8 at byte 42".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("日本語.txt"), "must show path: {msg}");
        assert!(msg.contains("byte 42"), "must show reason: {msg}");
    }

    #[test]
    fn missing_change_display_includes_hash() {
        let hash = Hash::of(b"important change");
        let err = OutputError::MissingChange { hash };
        let msg = err.to_string();
        // The hash in the message lets users fetch the missing change.
        assert!(
            msg.len() > 20,
            "should include the hash representation: {msg}"
        );
    }
}

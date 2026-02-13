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

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChangePosition;

    // -------------------------------------------------------------------------
    // OutputError Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_output_error_display_io() {
        let err = OutputError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        ));
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn test_output_error_display_change_store() {
        let hash = Hash::of(b"test");
        let err = OutputError::ChangeStore {
            message: "corrupted".to_string(),
            hash: Some(hash),
        };
        assert!(err.to_string().contains("corrupted"));
    }

    #[test]
    fn test_output_error_display_vertex_not_found() {
        let node = GraphNode {
            change: NodeId::new(42),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };
        let err = OutputError::NodeNotFound {
            node,
            change_id: Some(NodeId::new(42)),
        };
        assert!(err.to_string().contains("GraphNode not found"));
    }

    #[test]
    fn test_output_error_display_position_not_found() {
        let position = Position {
            change: NodeId::new(1),
            pos: ChangePosition::new(100),
        };
        let err = OutputError::PositionNotFound { position };
        assert!(err.to_string().contains("Position not found"));
    }

    #[test]
    fn test_output_error_display_inode_not_found() {
        let err = OutputError::InodeNotFound {
            inode: Inode::new(123),
        };
        assert!(err.to_string().contains("Inode"));
    }

    #[test]
    fn test_output_error_display_path_not_found() {
        let err = OutputError::PathNotFound {
            path: "/foo/bar.txt".to_string(),
        };
        assert!(err.to_string().contains("/foo/bar.txt"));
    }

    #[test]
    fn test_output_error_display_encoding() {
        let err = OutputError::Encoding {
            path: "test.txt".to_string(),
            message: "invalid UTF-8".to_string(),
        };
        assert!(err.to_string().contains("Encoding error"));
        assert!(err.to_string().contains("test.txt"));
    }

    #[test]
    fn test_output_error_display_not_writable() {
        let err = OutputError::NotWritable {
            path: "/readonly/file".to_string(),
        };
        assert!(err.to_string().contains("not writable"));
    }

    #[test]
    fn test_output_error_display_conflict() {
        let err = OutputError::Conflict {
            path: "src/main.rs".to_string(),
            conflict_type: ConflictType::Order,
            sides: 2,
        };
        assert!(err.to_string().contains("Conflict"));
        assert!(err.to_string().contains("src/main.rs"));
    }

    #[test]
    fn test_output_error_display_missing_change() {
        let hash = Hash::of(b"missing");
        let err = OutputError::MissingChange { hash };
        assert!(err.to_string().contains("Missing change"));
    }

    #[test]
    fn test_output_error_display_internal() {
        let err = OutputError::Internal {
            message: "unexpected state".to_string(),
        };
        assert!(err.to_string().contains("Internal error"));
    }

    #[test]
    fn test_output_error_from_pristine() {
        let pristine_err = PristineError::HashNotFound {
            hash: "test_hash".to_string(),
        };
        let output_err: OutputError = pristine_err.into();
        assert!(matches!(output_err, OutputError::Pristine(_)));
    }

    #[test]
    fn test_output_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let output_err: OutputError = io_err.into();
        assert!(matches!(output_err, OutputError::Io(_)));
    }

    // -------------------------------------------------------------------------
    // ConflictType Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_conflict_type_display() {
        assert_eq!(ConflictType::Order.to_string(), "order conflict");
        assert_eq!(ConflictType::Zombie.to_string(), "zombie conflict");
        assert_eq!(ConflictType::Cyclic.to_string(), "cyclic conflict");
        assert_eq!(ConflictType::Name.to_string(), "name conflict");
    }

    #[test]
    fn test_conflict_type_equality() {
        assert_eq!(ConflictType::Order, ConflictType::Order);
        assert_ne!(ConflictType::Order, ConflictType::Zombie);
    }

    #[test]
    fn test_conflict_type_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ConflictType::Order);
        set.insert(ConflictType::Zombie);
        set.insert(ConflictType::Order); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_conflict_type_clone() {
        let original = ConflictType::Cyclic;
        let cloned = original;
        assert_eq!(original, cloned);
    }

    #[test]
    fn test_conflict_type_debug() {
        let debug = format!("{:?}", ConflictType::Name);
        assert!(debug.contains("Name"));
    }

    // -------------------------------------------------------------------------
    // ContentError Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_content_error_display_io() {
        let err = ContentError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        assert!(err.to_string().contains("I/O error"));
    }

    #[test]
    fn test_content_error_display_not_found() {
        let err = ContentError::ChangeNotFound {
            hash: Hash::of(b"test"),
        };
        assert!(err.to_string().contains("Change not found"));
    }

    #[test]
    fn test_content_error_display_truncated() {
        let err = ContentError::Truncated {
            expected: 100,
            actual: 50,
        };
        let msg = err.to_string();
        assert!(msg.contains("truncated"));
        assert!(msg.contains("100"));
        assert!(msg.contains("50"));
    }

    #[test]
    fn test_content_error_display_vertex_content() {
        let node = GraphNode {
            change: NodeId::new(1),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };
        let err = ContentError::VertexContent { node };
        assert!(err.to_string().contains("Failed to get contents"));
    }

    #[test]
    fn test_content_error_to_output_error_io() {
        let content_err = ContentError::Io(std::io::Error::new(std::io::ErrorKind::Other, "test"));
        let output_err: OutputError = content_err.into();
        assert!(matches!(output_err, OutputError::Io(_)));
    }

    #[test]
    fn test_content_error_to_output_error_not_found() {
        let hash = Hash::of(b"missing");
        let content_err = ContentError::ChangeNotFound { hash };
        let output_err: OutputError = content_err.into();
        assert!(matches!(output_err, OutputError::MissingChange { .. }));
    }

    #[test]
    fn test_content_error_to_output_error_truncated() {
        let content_err = ContentError::Truncated {
            expected: 100,
            actual: 50,
        };
        let output_err: OutputError = content_err.into();
        assert!(matches!(output_err, OutputError::ChangeStore { .. }));
    }

    #[test]
    fn test_content_error_to_output_error_vertex() {
        let node = GraphNode {
            change: NodeId::new(42),
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
        };
        let content_err = ContentError::VertexContent { node };
        let output_err: OutputError = content_err.into();
        assert!(matches!(output_err, OutputError::NodeNotFound { .. }));
    }

    // -------------------------------------------------------------------------
    // TreeError Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_tree_error_display_pristine() {
        let err = TreeError::Pristine(PristineError::HashNotFound {
            hash: "test_hash".to_string(),
        });
        assert!(err.to_string().contains("Pristine error"));
    }

    #[test]
    fn test_tree_error_display_orphan() {
        let err = TreeError::OrphanEntry {
            name: "orphan.txt".to_string(),
            inode: Inode::new(999),
        };
        let msg = err.to_string();
        assert!(msg.contains("Orphan"));
        assert!(msg.contains("orphan.txt"));
    }

    #[test]
    fn test_tree_error_display_circular() {
        let err = TreeError::CircularReference {
            path: PathBuf::from("/a/b/c"),
        };
        assert!(err.to_string().contains("Circular reference"));
    }

    #[test]
    fn test_tree_error_display_max_depth() {
        let err = TreeError::MaxDepthExceeded {
            depth: 1000,
            limit: 100,
        };
        let msg = err.to_string();
        assert!(msg.contains("depth exceeded"));
        assert!(msg.contains("1000"));
        assert!(msg.contains("100"));
    }

    #[test]
    fn test_tree_error_to_output_error_pristine() {
        let tree_err = TreeError::Pristine(PristineError::HashNotFound {
            hash: "test_hash".to_string(),
        });
        let output_err: OutputError = tree_err.into();
        assert!(matches!(output_err, OutputError::Pristine(_)));
    }

    #[test]
    fn test_tree_error_to_output_error_orphan() {
        let tree_err = TreeError::OrphanEntry {
            name: "test".to_string(),
            inode: Inode::new(1),
        };
        let output_err: OutputError = tree_err.into();
        assert!(matches!(output_err, OutputError::Internal { .. }));
    }

    #[test]
    fn test_tree_error_to_output_error_circular() {
        let tree_err = TreeError::CircularReference {
            path: PathBuf::from("/test"),
        };
        let output_err: OutputError = tree_err.into();
        assert!(matches!(output_err, OutputError::Internal { .. }));
    }

    #[test]
    fn test_tree_error_to_output_error_max_depth() {
        let tree_err = TreeError::MaxDepthExceeded {
            depth: 500,
            limit: 100,
        };
        let output_err: OutputError = tree_err.into();
        assert!(matches!(output_err, OutputError::Internal { .. }));
    }

    // -------------------------------------------------------------------------
    // Result Type Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_output_result_ok() {
        let result: OutputResult<i32> = Ok(42);
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_output_result_err() {
        let result: OutputResult<i32> = Err(OutputError::Internal {
            message: "test".to_string(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_content_result_ok() {
        let result: ContentResult<String> = Ok("content".to_string());
        assert_eq!(result.unwrap(), "content");
    }

    #[test]
    fn test_content_result_err() {
        let result: ContentResult<String> = Err(ContentError::Truncated {
            expected: 10,
            actual: 5,
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_tree_result_ok() {
        let result: TreeResult<Vec<String>> = Ok(vec!["a".into(), "b".into()]);
        assert_eq!(result.unwrap().len(), 2);
    }

    #[test]
    fn test_tree_result_err() {
        let result: TreeResult<()> = Err(TreeError::MaxDepthExceeded {
            depth: 100,
            limit: 50,
        });
        assert!(result.is_err());
    }

    // -------------------------------------------------------------------------
    // Error Chaining Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_error_chaining_io_to_content_to_output() {
        // Create an IO error
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "original error");

        // Convert to ContentError
        let content_err: ContentError = io_err.into();
        assert!(matches!(content_err, ContentError::Io(_)));

        // Convert to OutputError
        let output_err: OutputError = content_err.into();
        assert!(matches!(output_err, OutputError::Io(_)));
    }

    #[test]
    fn test_error_chaining_pristine_to_tree_to_output() {
        // Create a PristineError
        let pristine_err = PristineError::HashNotFound {
            hash: "test_hash".to_string(),
        };

        // Convert to TreeError
        let tree_err: TreeError = pristine_err.into();
        assert!(matches!(tree_err, TreeError::Pristine(_)));

        // Convert to OutputError
        let output_err: OutputError = tree_err.into();
        assert!(matches!(output_err, OutputError::Pristine(_)));
    }

    // -------------------------------------------------------------------------
    // Debug Trait Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_output_error_debug() {
        let err = OutputError::Internal {
            message: "test debug".to_string(),
        };
        let debug = format!("{:?}", err);
        assert!(debug.contains("Internal"));
        assert!(debug.contains("test debug"));
    }

    #[test]
    fn test_content_error_debug() {
        let err = ContentError::Truncated {
            expected: 100,
            actual: 50,
        };
        let debug = format!("{:?}", err);
        assert!(debug.contains("Truncated"));
    }

    #[test]
    fn test_tree_error_debug() {
        let err = TreeError::CircularReference {
            path: PathBuf::from("/test"),
        };
        let debug = format!("{:?}", err);
        assert!(debug.contains("CircularReference"));
    }

    // -------------------------------------------------------------------------
    // Edge Cases and Special Values
    // -------------------------------------------------------------------------

    #[test]
    fn test_empty_path_error() {
        let err = OutputError::PathNotFound {
            path: String::new(),
        };
        assert!(err.to_string().contains("Path not found"));
    }

    #[test]
    fn test_unicode_path_error() {
        let err = OutputError::PathNotFound {
            path: "日本語/テスト.txt".to_string(),
        };
        assert!(err.to_string().contains("日本語"));
    }

    #[test]
    fn test_long_path_error() {
        let long_path = "a/".repeat(100) + "file.txt";
        let err = OutputError::PathNotFound {
            path: long_path.clone(),
        };
        assert!(err.to_string().contains(&long_path));
    }

    #[test]
    fn test_zero_conflict_sides() {
        let err = OutputError::Conflict {
            path: "test.txt".to_string(),
            conflict_type: ConflictType::Order,
            sides: 0,
        };
        // Should still display, even if 0 sides doesn't make practical sense
        assert!(err.to_string().contains("Conflict"));
    }

    #[test]
    fn test_many_conflict_sides() {
        let err = OutputError::Conflict {
            path: "test.txt".to_string(),
            conflict_type: ConflictType::Order,
            sides: 100,
        };
        assert!(err.to_string().contains("Conflict"));
    }

    #[test]
    fn test_root_node_vertex_error() {
        let node = GraphNode::ROOT;
        let err = OutputError::NodeNotFound {
            node,
            change_id: None,
        };
        assert!(err.to_string().contains("GraphNode not found"));
    }

    #[test]
    fn test_node_not_found_error_max() {
        let node = GraphNode::MAX;
        let err = OutputError::NodeNotFound {
            node,
            change_id: None,
        };
        assert!(err.to_string().contains("GraphNode not found"));
    }
}

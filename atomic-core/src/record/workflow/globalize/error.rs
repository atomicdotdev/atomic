use super::*;

/// Errors that can occur during globalization.
///
/// These errors indicate problems converting local file changes to graph
/// operations. Most are recoverable by checking that files exist and are
/// properly tracked before recording.
#[derive(Debug, Error)]
pub enum GlobalizeError {
    /// The file path was not found in the repository tree.
    ///
    /// This typically means the file is not tracked. Use `add` to track it
    /// before recording.
    #[error("Path not found in repository: {path}")]
    PathNotFound {
        /// The path that was not found
        path: String,
    },

    /// The inode has no associated graph position.
    ///
    /// This is an internal consistency error - tracked files should always
    /// have a graph position.
    #[error("Inode {inode} has no graph position")]
    InodeNotFound {
        /// The inode that has no position
        inode: Inode,
    },

    /// Cannot find the parent directory for a file.
    ///
    /// This occurs when trying to add a file to a directory that doesn't
    /// exist in the graph.
    #[error("Parent directory not found for path: {path}")]
    ParentNotFound {
        /// The path whose parent was not found
        path: String,
    },

    /// Cannot find the graph node containing a specific position.
    ///
    /// This occurs when trying to find context for an insertion point
    /// that doesn't correspond to any existing graph node.
    #[error("No graph node found at position {position:?}")]
    NodeNotFound {
        /// The position that has no graph node
        position: Position<NodeId>,
    },

    /// Cannot determine context for content insertion.
    ///
    /// Context is required to properly position new content in the graph.
    #[error("Cannot determine context for insertion at {path}:{line}")]
    MissingContext {
        /// The file path
        path: String,
        /// The line number
        line: u64,
    },

    /// The file has no content to globalize.
    ///
    /// This is not necessarily an error - empty files may be intentional.
    #[error("File has no content: {path}")]
    EmptyFile {
        /// The path of the empty file
        path: String,
    },

    /// A database error occurred during globalization.
    #[error("Database error: {0}")]
    Pristine(#[from] PristineError),

    /// The recorded file is missing required information.
    #[error("Recorded file missing {field}: {path}")]
    MissingField {
        /// The path of the file
        path: String,
        /// The missing field name
        field: &'static str,
    },

    /// Invalid line number for the file.
    #[error("Invalid line number {line} for file {path} (max: {max_line})")]
    InvalidLine {
        /// The file path
        path: String,
        /// The invalid line number
        line: u64,
        /// The maximum valid line number
        max_line: u64,
    },
}

/// Result type for globalization operations.
pub type GlobalizeResult<T> = Result<T, GlobalizeError>;

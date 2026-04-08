//! Types for repository materialization.
//!
//! Contains [`MaterializeOptions`], [`MaterializeError`], and [`OutputItem`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::SystemTime;

use crate::types::{Inode, NodeId, Position};

use super::super::file::FileOutputOptions;

// ============================================================================
// MATERIALIZE OPTIONS
// ============================================================================

/// Options for materializing the repository view to the working copy.
///
/// Controls how the repository is materialized to the working copy, including
/// filtering, optimization, and conflict handling options.
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::MaterializeOptions;
///
/// let opts = MaterializeOptions::new()
///     .prefix("src/")
///     .include_deleted(true);
/// ```
#[derive(Debug, Clone)]
pub struct MaterializeOptions {
    /// Prefix to filter output paths.
    ///
    /// Only files under this prefix will be output. Empty string means
    /// all files.
    pub prefix: String,

    /// Only output files modified after this time.
    pub if_modified_since: Option<SystemTime>,

    /// Whether to output files with name conflicts.
    pub output_name_conflicts: bool,

    /// Include deleted content in output.
    pub include_deleted: bool,

    /// Maximum vertices per file.
    pub max_vertices_per_file: Option<usize>,

    /// Salt for deterministic name conflict resolution.
    pub salt: u64,

    /// Whether to enable parallel output.
    pub parallel: bool,

    /// Number of worker threads for parallel output.
    pub num_workers: usize,

    /// Optional filter to only include vertices from specific changes.
    ///
    /// When set, only vertices whose `change_id` is in this set (or is ROOT)
    /// will be included in the output. This enables stack-aware output where
    /// switching stacks shows the content as it was when only that stack's
    /// changes were applied.
    ///
    /// The filter is wrapped in `Arc` for efficient sharing across files.
    pub change_filter: Option<Arc<HashSet<NodeId>>>,
}

impl MaterializeOptions {
    /// Create new options with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a change filter for stack-aware output.
    pub fn with_change_filter(mut self, filter: HashSet<NodeId>) -> Self {
        self.change_filter = Some(Arc::new(filter));
        self
    }

    /// Set a change filter from an existing Arc (avoids cloning).
    pub fn with_change_filter_arc(mut self, filter: Arc<HashSet<NodeId>>) -> Self {
        self.change_filter = Some(filter);
        self
    }

    /// Set the prefix filter.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Set the modification time filter.
    pub fn if_modified_since(mut self, time: SystemTime) -> Self {
        self.if_modified_since = Some(time);
        self
    }

    /// Set whether to output name conflicts.
    pub fn output_name_conflicts(mut self, output: bool) -> Self {
        self.output_name_conflicts = output;
        self
    }

    /// Set whether to include deleted content.
    pub fn include_deleted(mut self, include: bool) -> Self {
        self.include_deleted = include;
        self
    }

    /// Set the maximum vertices per file.
    pub fn max_vertices_per_file(mut self, max: usize) -> Self {
        self.max_vertices_per_file = Some(max);
        self
    }

    /// Set the salt for name conflict resolution.
    pub fn salt(mut self, salt: u64) -> Self {
        self.salt = salt;
        self
    }

    /// Enable parallel output.
    pub fn parallel(mut self, parallel: bool) -> Self {
        self.parallel = parallel;
        self
    }

    /// Set the number of worker threads.
    pub fn num_workers(mut self, num: usize) -> Self {
        self.num_workers = num;
        self
    }

    /// Convert to file output options.
    pub(crate) fn to_file_options(&self) -> FileOutputOptions {
        let mut opts = FileOutputOptions::new();
        if self.include_deleted {
            opts = opts.include_deleted(true);
        }
        if let Some(max) = self.max_vertices_per_file {
            opts = opts.max_vertices(max);
        }
        opts
    }

    /// Check if a path matches the prefix filter.
    pub fn matches_prefix(&self, path: &str) -> bool {
        if self.prefix.is_empty() {
            true
        } else {
            path.starts_with(&self.prefix)
        }
    }
}

impl Default for MaterializeOptions {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            if_modified_since: None,
            output_name_conflicts: true,
            include_deleted: false,
            max_vertices_per_file: None,
            salt: 0,
            parallel: false,
            num_workers: 1,
            change_filter: None,
        }
    }
}

// ============================================================================
// MATERIALIZE ERROR
// ============================================================================

/// Error type for materialize operations.
#[derive(Debug)]
pub enum MaterializeError<WE> {
    /// Error from the pristine database.
    Pristine(crate::pristine::PristineError),

    /// Error from the change store.
    ChangeStore(String),

    /// I/O error.
    Io(std::io::Error),

    /// Working copy error.
    WorkingCopy(WE),

    /// Tree traversal error.
    TreeError(String),
}

impl<WE: std::fmt::Debug> std::fmt::Display for MaterializeError<WE> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pristine(e) => write!(f, "Pristine error: {}", e),
            Self::ChangeStore(e) => write!(f, "Change store error: {}", e),
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::WorkingCopy(e) => write!(f, "Working copy error: {:?}", e),
            Self::TreeError(e) => write!(f, "Tree traversal error: {}", e),
        }
    }
}

impl<WE: std::fmt::Debug + std::error::Error + 'static> std::error::Error for MaterializeError<WE> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pristine(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::WorkingCopy(e) => Some(e),
            _ => None,
        }
    }
}

impl<WE> From<std::io::Error> for MaterializeError<WE> {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl<WE> From<crate::pristine::PristineError> for MaterializeError<WE> {
    fn from(e: crate::pristine::PristineError) -> Self {
        Self::Pristine(e)
    }
}

// ============================================================================
// OUTPUT ITEM
// ============================================================================

/// An item to be output (file or directory).
///
/// This is used during tree traversal to collect items that need to be
/// output to the working copy.
#[derive(Debug, Clone)]
pub struct OutputItem {
    /// Path in the working copy.
    pub path: String,

    /// Inode for this item.
    pub inode: Inode,

    /// Position in the graph (for files).
    pub position: Position<NodeId>,

    /// Whether this is a directory.
    pub is_directory: bool,

    /// File metadata (permissions, type).
    pub metadata: crate::output::traits::FileMetadata,
}

impl OutputItem {
    /// Create a new file output item.
    pub fn file(path: impl Into<String>, inode: Inode, position: Position<NodeId>) -> Self {
        Self {
            path: path.into(),
            inode,
            position,
            is_directory: false,
            metadata: crate::output::traits::FileMetadata::file(),
        }
    }

    /// Create a new directory output item.
    pub fn directory(path: impl Into<String>, inode: Inode) -> Self {
        Self {
            path: path.into(),
            inode,
            position: Position::ROOT,
            is_directory: true,
            metadata: crate::output::traits::FileMetadata::directory(),
        }
    }

    /// Set the metadata for this item.
    pub fn with_metadata(mut self, metadata: crate::output::traits::FileMetadata) -> Self {
        self.metadata = metadata;
        self
    }
}

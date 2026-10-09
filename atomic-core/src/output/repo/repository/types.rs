//! Types for repository materialization.
//!
//! Contains [`MaterializedEntry`], [`MaterializeOptions`], [`MaterializeError`],
//! and [`OutputItem`].

use std::collections::HashSet;
use std::time::SystemTime;

use crate::pristine::GraphVisibilityClosure;
use crate::types::{Inode, NodeId, Position};

use super::super::file::{FileOutputError, FileOutputOptions};

// ============================================================================
// MATERIALIZED ENTRY
// ============================================================================

/// The lifecycle presence and rendered content of a repository entry.
///
/// Presence is explicit: a present file with zero bytes is distinct from an
/// absent file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaterializedEntry {
    /// The path is absent from the materialized view.
    Absent {
        /// Path in the working copy.
        path: String,
        /// Last known inode, when one is available.
        inode: Option<Inode>,
        /// Whether the absent entry is a directory.
        directory: bool,
    },
    /// The path is present in the materialized view.
    Present {
        /// Path in the working copy.
        path: String,
        /// Stable inode identity for the file.
        inode: Inode,
        /// Rendered file content, which may be empty.
        bytes: Vec<u8>,
    },
}

impl MaterializedEntry {
    /// Create an absent entry with an optional last-known inode.
    pub fn absent(path: impl Into<String>, inode: Option<Inode>) -> Self {
        Self::Absent {
            path: path.into(),
            inode,
            directory: false,
        }
    }

    /// Create an absent directory entry with its last-known inode.
    pub fn absent_directory(path: impl Into<String>, inode: Inode) -> Self {
        Self::Absent {
            path: path.into(),
            inode: Some(inode),
            directory: true,
        }
    }

    /// Create a present entry with its inode and rendered bytes.
    pub fn present(path: impl Into<String>, inode: Inode, bytes: impl Into<Vec<u8>>) -> Self {
        Self::Present {
            path: path.into(),
            inode,
            bytes: bytes.into(),
        }
    }

    /// Return the working-copy path for this entry.
    pub fn path(&self) -> &str {
        match self {
            Self::Absent { path, .. } | Self::Present { path, .. } => path,
        }
    }

    /// Return the inode identity when known.
    pub fn inode(&self) -> Option<Inode> {
        match self {
            Self::Absent { inode, .. } => *inode,
            Self::Present { inode, .. } => Some(*inode),
        }
    }

    /// Return rendered bytes for a present entry.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Absent { .. } => None,
            Self::Present { bytes, .. } => Some(bytes),
        }
    }

    /// Return whether this entry is present in the materialized view.
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Present { .. })
    }

    /// Return whether this entry is absent from the materialized view.
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent { .. })
    }

    /// Return whether this lifecycle entry denotes a directory.
    pub fn is_directory(&self) -> bool {
        matches!(
            self,
            Self::Absent {
                directory: true,
                ..
            }
        )
    }
}

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

    /// Optional validated dependency closure for graph traversal.
    ///
    /// When set, only vertices whose `change_id` is in this closure (or is
    /// ROOT) are included in output. `None` explicitly selects the unfiltered
    /// ambient graph; an empty closure is still an active filter.
    pub graph_visibility: Option<GraphVisibilityClosure>,

    /// Optional set of specific file paths to materialize.
    ///
    /// When set, only files whose path is in this set will be written.
    /// All other files are skipped. This enables selective materialization
    /// after operations like `insert` that only affect a subset of files.
    pub only_paths: Option<HashSet<String>>,
}

impl MaterializeOptions {
    /// Create new options with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set validated graph visibility for view-aware output.
    pub fn with_graph_visibility(mut self, visibility: GraphVisibilityClosure) -> Self {
        self.graph_visibility = Some(visibility);
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

    /// Set specific paths to materialize.
    ///
    /// Only files in this set will be written to the working copy.
    /// Other files are skipped entirely (no graph traversal, no I/O).
    pub fn only_paths(mut self, paths: HashSet<String>) -> Self {
        self.only_paths = Some(paths);
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
            graph_visibility: None,
            only_paths: None,
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

    /// A file could not be output to the working copy.
    FileOutput {
        /// Path whose output failed.
        path: String,
        /// Typed file-output failure.
        source: FileOutputError<WE>,
    },
}

impl<WE: std::fmt::Debug> std::fmt::Display for MaterializeError<WE> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pristine(e) => write!(f, "Pristine error: {}", e),
            Self::ChangeStore(e) => write!(f, "Change store error: {}", e),
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::WorkingCopy(e) => write!(f, "Working copy error: {:?}", e),
            Self::TreeError(e) => write!(f, "Tree traversal error: {}", e),
            Self::FileOutput { path, source } => {
                write!(f, "Failed to materialize {}: {}", path, source)
            }
        }
    }
}

impl<WE: std::fmt::Debug + std::error::Error + 'static> std::error::Error for MaterializeError<WE> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pristine(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::WorkingCopy(e) => Some(e),
            Self::FileOutput { source, .. } => Some(source),
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
        Self::directory_at(path, inode, Position::ROOT)
    }

    /// Create a directory output item backed by its graph inode position.
    pub fn directory_at(path: impl Into<String>, inode: Inode, position: Position<NodeId>) -> Self {
        Self {
            path: path.into(),
            inode,
            position,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_empty_content_is_not_absent() {
        let present = MaterializedEntry::present("empty.txt", Inode::ROOT, Vec::new());
        let absent = MaterializedEntry::absent("empty.txt", Some(Inode::ROOT));

        assert_ne!(present, absent);
        assert!(present.is_present());
        assert_eq!(present.bytes(), Some(&[][..]));
        assert!(absent.is_absent());
        assert_eq!(absent.bytes(), None);
    }

    #[test]
    fn materialized_entry_accessors_preserve_identity() {
        let entry = MaterializedEntry::present("src/lib.rs", Inode::ROOT, b"content".to_vec());

        assert_eq!(entry.path(), "src/lib.rs");
        assert_eq!(entry.inode(), Some(Inode::ROOT));
        assert_eq!(entry.bytes(), Some(b"content".as_slice()));

        let absent = MaterializedEntry::absent("removed.rs", None);
        assert_eq!(absent.path(), "removed.rs");
        assert_eq!(absent.inode(), None);
    }
}

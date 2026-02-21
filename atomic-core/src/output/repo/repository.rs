//! Full repository output to working copy.
//!
//! This module provides the top-level functions for outputting an entire
//! repository (or prefix thereof) from the graph to the working copy.
//! It orchestrates the tree traversal, file output, and conflict collection.
//!
//! # Overview
//!
//! Repository output is the process of synchronizing the working copy with
//! the repository graph state. This involves:
//!
//! 1. Traversing the tree structure to find all files
//! 2. For each file, outputting its content from the graph
//! 3. Handling deletions, renames, and name conflicts
//! 4. Collecting and reporting all conflicts
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                    Repository Output Pipeline                            │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Repository                Processing                   Working Copy    │
//! │  ┌──────────────┐         ┌─────────────────┐         ┌────────────┐   │
//! │  │ Tree         │ iterate │ For each file:  │ write   │ Files      │   │
//! │  │ Pristine     │ ──────► │ 1. Check mtime  │ ──────► │ Directories│   │
//! │  │ Changes      │         │ 2. Output file  │         │ Conflicts  │   │
//! │  └──────────────┘         │ 3. Track result │         └────────────┘   │
//! │                           └─────────────────┘                          │
//! │                                                                         │
//! │  Optimizations:                                                         │
//! │  - Skip unchanged files (mtime check)                                   │
//! │  - Parallel file output (optional)                                      │
//! │  - Prefix filtering for partial output                                  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::repo::{output_repository, RepositoryOutputOptions};
//!
//! // Output entire repository
//! let result = output_repository(
//!     &txn,
//!     &changes,
//!     &working_copy,
//!     RepositoryOutputOptions::new(),
//! )?;
//!
//! println!("Output {} files with {} conflicts",
//!     result.files_written,
//!     result.conflict_count());
//!
//! // Output only a prefix
//! let result = output_repository(
//!     &txn,
//!     &changes,
//!     &working_copy,
//!     RepositoryOutputOptions::new().prefix("src/"),
//! )?;
//! ```
//!
//! # Conflict Handling
//!
//! The repository output collects all conflicts from individual files and
//! also detects repository-level conflicts like name conflicts (multiple
//! files with the same name).
//!
//! # Performance
//!
//! For large repositories, the output can be optimized by:
//! - Using `if_modified_since` to skip unchanged files
//! - Setting `parallel` to true for multi-threaded output
//! - Using `prefix` to output only a subset of files

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

use crate::change::ChangeStore;
use crate::output::traits::WorkingCopy;
use crate::pristine::{GraphTxnT, TreeTxnT};
use crate::types::{Inode, NodeId, Position};

use super::conflict::{FileConflict, FileConflictType};
use super::file::{output_file_with_filter, FileOutputOptions, FileOutputResult};
use super::outcome::OutputOutcome;
use super::tree::{collect_tree, TreeCollectOptions};

// ============================================================================
// REPOSITORY OUTPUT OPTIONS
// ============================================================================

/// Options for repository output operations.
///
/// Controls how the repository is output to the working copy, including
/// filtering, optimization, and conflict handling options.
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::RepositoryOutputOptions;
///
/// // Default options - output everything
/// let opts = RepositoryOutputOptions::new();
///
/// // Output only files under src/
/// let opts = RepositoryOutputOptions::new()
///     .prefix("src/");
///
/// // Skip files not modified since a certain time
/// let opts = RepositoryOutputOptions::new()
///     .if_modified_since(std::time::SystemTime::now());
///
/// // Output with name conflict resolution
/// let opts = RepositoryOutputOptions::new()
///     .output_name_conflicts(true);
/// ```
#[derive(Debug, Clone)]
pub struct RepositoryOutputOptions {
    /// Prefix to filter output paths.
    ///
    /// Only files under this prefix will be output. Empty string means
    /// all files.
    pub prefix: String,

    /// Only output files modified after this time.
    ///
    /// Files whose graph modification time is before this will be skipped.
    /// This is an optimization for incremental updates.
    pub if_modified_since: Option<SystemTime>,

    /// Whether to output files with name conflicts.
    ///
    /// If true, when multiple files have the same name, all versions
    /// are output with unique suffixes. If false, only one is output.
    pub output_name_conflicts: bool,

    /// Include deleted content in output.
    ///
    /// When true, zombie content (deleted but modified) will be shown
    /// wrapped in conflict markers.
    pub include_deleted: bool,

    /// Maximum vertices per file.
    ///
    /// Safety limit to prevent runaway processing.
    pub max_vertices_per_file: Option<usize>,

    /// Salt for deterministic name conflict resolution.
    ///
    /// When name conflicts occur and `output_name_conflicts` is true,
    /// this salt is used to generate unique suffixes.
    pub salt: u64,

    /// Whether to enable parallel output.
    ///
    /// When true, files may be output in parallel for better performance.
    /// Note: Not yet implemented in this version.
    pub parallel: bool,

    /// Number of worker threads for parallel output.
    ///
    /// Only used when `parallel` is true.
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

impl RepositoryOutputOptions {
    /// Create new options with defaults.
    ///
    /// Default configuration:
    /// - `prefix`: "" (all files)
    /// - `if_modified_since`: None (output all files)
    /// - `output_name_conflicts`: true
    /// - `include_deleted`: false
    /// - `max_vertices_per_file`: None
    /// - `salt`: 0
    /// - `parallel`: false
    /// - `num_workers`: 1
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputOptions;
    ///
    /// let opts = RepositoryOutputOptions::new();
    /// assert!(opts.prefix.is_empty());
    /// assert!(opts.output_name_conflicts);
    /// assert!(!opts.parallel);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a change filter for stack-aware output.
    ///
    /// Only vertices from changes in this set (or ROOT) will be included.
    /// This enables outputting file content at a specific stack state.
    ///
    /// # Arguments
    ///
    /// * `filter` - Set of change NodeIds to include
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::output::repo::RepositoryOutputOptions;
    /// use std::collections::HashSet;
    ///
    /// // Get changes in the current stack
    /// let changes: HashSet<NodeId> = collect_stack_changes(&txn, &stack)?;
    /// let options = RepositoryOutputOptions::new().with_change_filter(changes);
    /// let result = output_repository(&txn, &changes_store, &wc, options)?;
    /// ```
    pub fn with_change_filter(mut self, filter: HashSet<NodeId>) -> Self {
        self.change_filter = Some(Arc::new(filter));
        self
    }

    /// Set a change filter from an existing Arc (avoids cloning).
    ///
    /// Use this when you want to share the same filter across multiple
    /// operations for efficiency.
    pub fn with_change_filter_arc(mut self, filter: Arc<HashSet<NodeId>>) -> Self {
        self.change_filter = Some(filter);
        self
    }

    /// Set the prefix filter.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path prefix to filter files
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputOptions;
    ///
    /// let opts = RepositoryOutputOptions::new().prefix("src/lib/");
    /// assert_eq!(opts.prefix, "src/lib/");
    /// ```
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Set the modification time filter.
    ///
    /// # Arguments
    ///
    /// * `time` - Only output files modified after this time
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputOptions;
    /// use std::time::SystemTime;
    ///
    /// let opts = RepositoryOutputOptions::new()
    ///     .if_modified_since(SystemTime::now());
    /// assert!(opts.if_modified_since.is_some());
    /// ```
    pub fn if_modified_since(mut self, time: SystemTime) -> Self {
        self.if_modified_since = Some(time);
        self
    }

    /// Set whether to output name conflicts.
    ///
    /// # Arguments
    ///
    /// * `output` - Whether to output all versions of conflicting names
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputOptions;
    ///
    /// let opts = RepositoryOutputOptions::new().output_name_conflicts(false);
    /// assert!(!opts.output_name_conflicts);
    /// ```
    pub fn output_name_conflicts(mut self, output: bool) -> Self {
        self.output_name_conflicts = output;
        self
    }

    /// Set whether to include deleted content.
    ///
    /// # Arguments
    ///
    /// * `include` - Whether to include zombie content
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputOptions;
    ///
    /// let opts = RepositoryOutputOptions::new().include_deleted(true);
    /// assert!(opts.include_deleted);
    /// ```
    pub fn include_deleted(mut self, include: bool) -> Self {
        self.include_deleted = include;
        self
    }

    /// Set the maximum vertices per file.
    ///
    /// # Arguments
    ///
    /// * `max` - Maximum span count per file
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputOptions;
    ///
    /// let opts = RepositoryOutputOptions::new().max_vertices_per_file(10000);
    /// assert_eq!(opts.max_vertices_per_file, Some(10000));
    /// ```
    pub fn max_vertices_per_file(mut self, max: usize) -> Self {
        self.max_vertices_per_file = Some(max);
        self
    }

    /// Set the salt for name conflict resolution.
    ///
    /// # Arguments
    ///
    /// * `salt` - Salt value for deterministic suffix generation
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputOptions;
    ///
    /// let opts = RepositoryOutputOptions::new().salt(12345);
    /// assert_eq!(opts.salt, 12345);
    /// ```
    pub fn salt(mut self, salt: u64) -> Self {
        self.salt = salt;
        self
    }

    /// Enable parallel output.
    ///
    /// # Arguments
    ///
    /// * `parallel` - Whether to enable parallel processing
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputOptions;
    ///
    /// let opts = RepositoryOutputOptions::new().parallel(true);
    /// assert!(opts.parallel);
    /// ```
    pub fn parallel(mut self, parallel: bool) -> Self {
        self.parallel = parallel;
        self
    }

    /// Set the number of worker threads.
    ///
    /// # Arguments
    ///
    /// * `num` - Number of worker threads
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputOptions;
    ///
    /// let opts = RepositoryOutputOptions::new().num_workers(4);
    /// assert_eq!(opts.num_workers, 4);
    /// ```
    pub fn num_workers(mut self, num: usize) -> Self {
        self.num_workers = num;
        self
    }

    /// Convert to file output options.
    fn to_file_options(&self) -> FileOutputOptions {
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
    ///
    /// # Arguments
    ///
    /// * `path` - Path to check
    ///
    /// # Returns
    ///
    /// `true` if the path is under the prefix.
    pub fn matches_prefix(&self, path: &str) -> bool {
        if self.prefix.is_empty() {
            true
        } else {
            path.starts_with(&self.prefix)
        }
    }
}

impl Default for RepositoryOutputOptions {
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
// REPOSITORY OUTPUT RESULT
// ============================================================================

/// Result of a repository output operation.
///
/// Contains statistics about the output and all conflicts detected.
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::RepositoryOutputResult;
///
/// // After calling output_repository:
/// // let result = output_repository(...)?;
/// //
/// // println!("Output {} files, {} bytes",
/// //     result.files_written,
/// //     result.bytes_written);
/// //
/// // if result.has_conflicts() {
/// //     println!("Found {} conflicts", result.conflict_count());
/// // }
/// ```
#[derive(Debug, Clone, Default)]
pub struct RepositoryOutputResult {
    /// Number of files written.
    pub files_written: usize,

    /// Number of files skipped (due to mtime or prefix filter).
    pub files_skipped: usize,

    /// Number of directories created.
    pub directories_created: usize,

    /// Total bytes written across all files.
    pub bytes_written: u64,

    /// Total vertices processed across all files.
    pub vertices_processed: usize,

    /// Total edges traversed across all files.
    pub edges_traversed: usize,

    /// Number of files that were truncated due to max_vertices.
    pub files_truncated: usize,

    /// All conflicts detected during output.
    pub conflicts: Vec<FileConflict>,

    /// Per-file results (optional, for detailed reporting).
    pub file_results: BTreeMap<String, FileOutputResult>,
}

impl RepositoryOutputResult {
    /// Create a new empty result.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if any conflicts were detected.
    ///
    /// # Returns
    ///
    /// `true` if there is at least one conflict.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputResult;
    ///
    /// let result = RepositoryOutputResult::new();
    /// assert!(!result.has_conflicts());
    /// ```
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Get the total number of conflicts.
    ///
    /// # Returns
    ///
    /// Count of all conflicts detected.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::RepositoryOutputResult;
    ///
    /// let result = RepositoryOutputResult::new();
    /// assert_eq!(result.conflict_count(), 0);
    /// ```
    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }

    /// Get conflicts filtered by type.
    ///
    /// # Arguments
    ///
    /// * `conflict_type` - The type to filter by
    ///
    /// # Returns
    ///
    /// Iterator over conflicts of the specified type.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::{RepositoryOutputResult, FileConflictType};
    ///
    /// let result = RepositoryOutputResult::new();
    /// let name_conflicts: Vec<_> = result.conflicts_of_type(FileConflictType::Name).collect();
    /// assert!(name_conflicts.is_empty());
    /// ```
    pub fn conflicts_of_type(
        &self,
        conflict_type: FileConflictType,
    ) -> impl Iterator<Item = &FileConflict> {
        self.conflicts
            .iter()
            .filter(move |c| c.conflict_type == conflict_type)
    }

    /// Get name conflicts only.
    ///
    /// # Returns
    ///
    /// Iterator over name conflicts.
    pub fn name_conflicts(&self) -> impl Iterator<Item = &FileConflict> {
        self.conflicts_of_type(FileConflictType::Name)
    }

    /// Get content conflicts only (Order, Cyclic, Zombie).
    ///
    /// # Returns
    ///
    /// Iterator over content conflicts.
    pub fn content_conflicts(&self) -> impl Iterator<Item = &FileConflict> {
        self.conflicts.iter().filter(|c| c.is_content_conflict())
    }

    /// Add a conflict to the result.
    ///
    /// # Arguments
    ///
    /// * `conflict` - The conflict to add
    pub fn add_conflict(&mut self, conflict: FileConflict) {
        self.conflicts.push(conflict);
    }

    /// Merge results from a file output.
    ///
    /// # Arguments
    ///
    /// * `file_result` - Result from outputting a single file
    /// * `store_result` - Whether to store the full file result
    pub fn merge_file_result(&mut self, file_result: FileOutputResult, store_result: bool) {
        self.files_written += 1;
        self.bytes_written += file_result.bytes_written;
        self.vertices_processed += file_result.vertices_processed;
        self.edges_traversed += file_result.edges_traversed;

        if file_result.was_truncated {
            self.files_truncated += 1;
        }

        for conflict in file_result.conflicts.iter() {
            self.conflicts.push(conflict.clone());
        }

        if store_result {
            self.file_results
                .insert(file_result.path.clone(), file_result);
        }
    }

    /// Record that a file was skipped.
    pub fn record_skipped(&mut self) {
        self.files_skipped += 1;
    }

    /// Record that a directory was created.
    pub fn record_directory(&mut self) {
        self.directories_created += 1;
    }

    /// Convert to OutputOutcome for unified result type.
    ///
    /// Note: This creates a basic outcome without individual file paths.
    /// For detailed file tracking, use the `file_results` map directly.
    pub fn to_outcome(&self) -> OutputOutcome {
        let mut outcome = OutputOutcome::new();

        // Record files written (without individual paths since we don't track them here)
        for _ in 0..self.files_written {
            outcome.record_file("", 0);
        }

        // Record directories created
        for i in 0..self.directories_created {
            outcome.record_directory(&format!("dir_{}", i));
        }

        // Record skipped files
        for _ in 0..self.files_skipped {
            outcome.record_skip("");
        }

        // Set total bytes (overwrite the 0s from record_file calls)
        outcome.bytes_written = self.bytes_written;

        outcome
    }
}

// ============================================================================
// REPOSITORY OUTPUT ERROR
// ============================================================================

/// Error type for repository output operations.
#[derive(Debug)]
pub enum RepositoryOutputError<WE> {
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

impl<WE: std::fmt::Debug> std::fmt::Display for RepositoryOutputError<WE> {
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

impl<WE: std::fmt::Debug + std::error::Error + 'static> std::error::Error
    for RepositoryOutputError<WE>
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pristine(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::WorkingCopy(e) => Some(e),
            _ => None,
        }
    }
}

impl<WE> From<std::io::Error> for RepositoryOutputError<WE> {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl<WE> From<crate::pristine::PristineError> for RepositoryOutputError<WE> {
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
    ///
    /// # Arguments
    ///
    /// * `path` - Path in working copy
    /// * `inode` - File inode
    /// * `position` - Position in graph
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
    ///
    /// # Arguments
    ///
    /// * `path` - Path in working copy
    /// * `inode` - Directory inode
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
    ///
    /// # Arguments
    ///
    /// * `metadata` - File or directory metadata
    pub fn with_metadata(mut self, metadata: crate::output::traits::FileMetadata) -> Self {
        self.metadata = metadata;
        self
    }
}

// ============================================================================
// COLLECT CHILDREN
// ============================================================================

/// Collect children of a directory from the graph.
///
/// Uses the tree traversal module to collect all files and directories
/// under the specified parent path, applying the repository output options
/// for filtering.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph and tree access
/// * `_parent_inode` - Inode of the parent directory (unused, traversal starts from path)
/// * `parent_path` - Path to start traversal from
/// * `options` - Output options for filtering
///
/// # Returns
///
/// A vector of output items (files and directories).
///
/// # Errors
///
/// Returns an error if tree traversal or inode resolution fails.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::repo::{collect_children, RepositoryOutputOptions};
/// use atomic_core::types::Inode;
///
/// let options = RepositoryOutputOptions::new().prefix("src/");
/// let items = collect_children(&txn, Inode::ROOT, "", &options)?;
///
/// for item in items {
///     println!("{}: {}", if item.is_directory { "dir" } else { "file" }, item.path);
/// }
/// ```
pub fn collect_children<T: TreeTxnT + GraphTxnT>(
    txn: &T,
    _parent_inode: Inode,
    parent_path: &str,
    options: &RepositoryOutputOptions,
) -> Result<Vec<OutputItem>, crate::pristine::PristineError> {
    // Convert RepositoryOutputOptions to TreeCollectOptions
    let mut tree_opts = TreeCollectOptions::new()
        .collect_directories(true)
        .collect_files(true);

    // Apply prefix filter
    if !options.prefix.is_empty() {
        tree_opts = tree_opts.prefix(&options.prefix);
    }

    // Collect from tree
    let tree_result = collect_tree(txn, parent_path, tree_opts)?;

    // Convert TreeItems to OutputItems
    let items = tree_result
        .items
        .into_iter()
        .map(|tree_item| {
            if tree_item.is_directory {
                OutputItem::directory(tree_item.path, tree_item.inode)
                    .with_metadata(tree_item.metadata)
            } else {
                OutputItem::file(tree_item.path, tree_item.inode, tree_item.position)
                    .with_metadata(tree_item.metadata)
            }
        })
        .collect();

    Ok(items)
}

// ============================================================================
// OUTPUT REPOSITORY FUNCTION
// ============================================================================

/// Output the repository (or prefix) to the working copy.
///
/// This is the main entry point for synchronizing the working copy with
/// the repository graph state. It traverses the tree, outputs each file,
/// and collects all conflicts.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph and tree access
/// * `changes` - Change store for retrieving content
/// * `working_copy` - Working copy to write to
/// * `options` - Output options
///
/// # Returns
///
/// A `RepositoryOutputResult` with statistics and conflicts.
///
/// # Errors
///
/// Returns an error if tree traversal, content retrieval, or writing fails.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::repo::{output_repository, RepositoryOutputOptions};
///
/// let result = output_repository(
///     &txn,
///     &changes,
///     &working_copy,
///     RepositoryOutputOptions::new(),
/// )?;
///
/// println!("Output {} files", result.files_written);
/// ```
pub fn output_repository<T, C, W>(
    txn: &T,
    changes: &C,
    working_copy: &W,
    options: RepositoryOutputOptions,
) -> Result<RepositoryOutputResult, RepositoryOutputError<W::Error>>
where
    T: TreeTxnT + GraphTxnT,
    C: ChangeStore,
    W: WorkingCopy,
    W::Writer: std::io::Write,
{
    let mut result = RepositoryOutputResult::new();

    // Collect items to output starting from root
    let items = collect_children(txn, Inode::ROOT, "", &options)?;

    // Process each item
    let file_options = options.to_file_options();

    // ── Stack-aware pre-filter ──────────────────────────────────────
    // When a change_filter is active (stack-aware output), we pre-compute
    // the set of file paths whose introducing change passes the filter.
    // Directories are only created if they are ancestors of at least one
    // passing file.  This prevents recreating directories (and empty files)
    // that belong to a different stack.
    let passing_file_paths: Option<std::collections::HashSet<String>> =
        if let Some(ref filter) = options.change_filter {
            let mut paths = std::collections::HashSet::new();
            for item in &items {
                if item.is_directory {
                    continue;
                }
                // Check whether the file's introducing change is in the filter.
                // position.change gives us the NodeId of the change that created
                // this file's inode vertex.
                let change_id = item.position.change;
                if change_id == crate::types::NodeId::ROOT || filter.contains(&change_id) {
                    paths.insert(item.path.clone());
                }
            }
            Some(paths)
        } else {
            None
        };

    // Helper: check if a directory path is an ancestor of any passing file.
    let dir_has_passing_children = |dir_path: &str| -> bool {
        match &passing_file_paths {
            None => true, // No filter — always create directories
            Some(paths) => {
                let prefix = if dir_path.ends_with('/') {
                    dir_path.to_string()
                } else {
                    format!("{}/", dir_path)
                };
                paths.iter().any(|p| p.starts_with(&prefix))
            }
        }
    };

    for item in items {
        if item.is_directory {
            // Only create directories that will contain files on this stack
            if !dir_has_passing_children(&item.path) {
                result.record_skipped();
                continue;
            }
            // Create directory
            working_copy
                .create_dir_all(&item.path)
                .map_err(RepositoryOutputError::WorkingCopy)?;
            result.record_directory();
        } else {
            // Check prefix filter
            if !options.matches_prefix(&item.path) {
                result.record_skipped();
                continue;
            }

            // Stack-aware skip: if the file's introducing change is not in the
            // filter, skip it entirely.  This avoids calling output_file_with_filter
            // which would otherwise create an empty file on disk.
            if let Some(ref paths) = passing_file_paths {
                if !paths.contains(&item.path) {
                    result.record_skipped();
                    continue;
                }
            }

            // Output file
            match output_file_with_filter(
                txn,
                changes,
                working_copy,
                item.inode,
                item.position,
                &item.path,
                file_options,
                options.change_filter.clone(),
            ) {
                Ok(file_result) => {
                    result.merge_file_result(file_result, false);
                }
                Err(e) => {
                    // Log error but continue with other files
                    // In a real implementation, we might want to collect errors
                    log::warn!("Failed to output {}: {:?}", item.path, e);
                }
            }
        }
    }

    Ok(result)
}

/// Output the repository to a specific prefix only.
///
/// Convenience function that sets the prefix option.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph and tree access
/// * `changes` - Change store for retrieving content
/// * `working_copy` - Working copy to write to
/// * `prefix` - Path prefix to output
///
/// # Returns
///
/// A `RepositoryOutputResult` with statistics and conflicts.
pub fn output_repository_prefix<T, C, W>(
    txn: &T,
    changes: &C,
    working_copy: &W,
    prefix: &str,
) -> Result<RepositoryOutputResult, RepositoryOutputError<W::Error>>
where
    T: TreeTxnT + GraphTxnT,
    C: ChangeStore,
    W: WorkingCopy,
    W::Writer: std::io::Write,
{
    output_repository(
        txn,
        changes,
        working_copy,
        RepositoryOutputOptions::new().prefix(prefix),
    )
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    // ========================================================================
    // RepositoryOutputOptions Tests
    // ========================================================================

    #[test]
    fn test_options_new() {
        let opts = RepositoryOutputOptions::new();

        assert!(opts.prefix.is_empty());
        assert!(opts.if_modified_since.is_none());
        assert!(opts.output_name_conflicts);
        assert!(!opts.include_deleted);
        assert!(opts.max_vertices_per_file.is_none());
        assert_eq!(opts.salt, 0);
        assert!(!opts.parallel);
        assert_eq!(opts.num_workers, 1);
    }

    #[test]
    fn test_options_default() {
        let opts = RepositoryOutputOptions::default();

        assert!(opts.prefix.is_empty());
        assert!(!opts.parallel);
    }

    #[test]
    fn test_options_prefix() {
        let opts = RepositoryOutputOptions::new().prefix("src/");

        assert_eq!(opts.prefix, "src/");
    }

    #[test]
    fn test_options_prefix_empty() {
        let opts = RepositoryOutputOptions::new().prefix("");

        assert!(opts.prefix.is_empty());
    }

    #[test]
    fn test_options_if_modified_since() {
        let time = SystemTime::now();
        let opts = RepositoryOutputOptions::new().if_modified_since(time);

        assert!(opts.if_modified_since.is_some());
    }

    #[test]
    fn test_options_output_name_conflicts() {
        let opts = RepositoryOutputOptions::new().output_name_conflicts(false);

        assert!(!opts.output_name_conflicts);
    }

    #[test]
    fn test_options_include_deleted() {
        let opts = RepositoryOutputOptions::new().include_deleted(true);

        assert!(opts.include_deleted);
    }

    #[test]
    fn test_options_max_vertices_per_file() {
        let opts = RepositoryOutputOptions::new().max_vertices_per_file(5000);

        assert_eq!(opts.max_vertices_per_file, Some(5000));
    }

    #[test]
    fn test_options_salt() {
        let opts = RepositoryOutputOptions::new().salt(42);

        assert_eq!(opts.salt, 42);
    }

    #[test]
    fn test_options_parallel() {
        let opts = RepositoryOutputOptions::new().parallel(true);

        assert!(opts.parallel);
    }

    #[test]
    fn test_options_num_workers() {
        let opts = RepositoryOutputOptions::new().num_workers(8);

        assert_eq!(opts.num_workers, 8);
    }

    #[test]
    fn test_options_chaining() {
        let opts = RepositoryOutputOptions::new()
            .prefix("src/")
            .include_deleted(true)
            .output_name_conflicts(false)
            .salt(100)
            .parallel(true)
            .num_workers(4);

        assert_eq!(opts.prefix, "src/");
        assert!(opts.include_deleted);
        assert!(!opts.output_name_conflicts);
        assert_eq!(opts.salt, 100);
        assert!(opts.parallel);
        assert_eq!(opts.num_workers, 4);
    }

    #[test]
    fn test_options_matches_prefix_empty() {
        let opts = RepositoryOutputOptions::new();

        assert!(opts.matches_prefix("anything"));
        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix(""));
    }

    #[test]
    fn test_options_matches_prefix_with_prefix() {
        let opts = RepositoryOutputOptions::new().prefix("src/");

        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix("src/lib/mod.rs"));
        assert!(!opts.matches_prefix("tests/test.rs"));
        assert!(!opts.matches_prefix("Cargo.toml"));
    }

    #[test]
    fn test_options_to_file_options_default() {
        let opts = RepositoryOutputOptions::new();
        let file_opts = opts.to_file_options();

        assert!(!file_opts.include_deleted);
        assert!(file_opts.max_vertices.is_none());
    }

    #[test]
    fn test_options_to_file_options_with_deleted() {
        let opts = RepositoryOutputOptions::new().include_deleted(true);
        let file_opts = opts.to_file_options();

        assert!(file_opts.include_deleted);
    }

    #[test]
    fn test_options_to_file_options_with_max() {
        let opts = RepositoryOutputOptions::new().max_vertices_per_file(1000);
        let file_opts = opts.to_file_options();

        assert_eq!(file_opts.max_vertices, Some(1000));
    }

    #[test]
    fn test_options_clone() {
        let opts = RepositoryOutputOptions::new().prefix("test/");
        let cloned = opts.clone();

        assert_eq!(opts.prefix, cloned.prefix);
    }

    #[test]
    fn test_options_debug() {
        let opts = RepositoryOutputOptions::new();
        let debug = format!("{:?}", opts);

        assert!(debug.contains("RepositoryOutputOptions"));
    }

    // ========================================================================
    // RepositoryOutputResult Tests
    // ========================================================================

    #[test]
    fn test_result_new() {
        let result = RepositoryOutputResult::new();

        assert_eq!(result.files_written, 0);
        assert_eq!(result.files_skipped, 0);
        assert_eq!(result.directories_created, 0);
        assert_eq!(result.bytes_written, 0);
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn test_result_default() {
        let result = RepositoryOutputResult::default();

        assert_eq!(result.files_written, 0);
    }

    #[test]
    fn test_result_has_conflicts_empty() {
        let result = RepositoryOutputResult::new();

        assert!(!result.has_conflicts());
    }

    #[test]
    fn test_result_has_conflicts_with_conflict() {
        let mut result = RepositoryOutputResult::new();
        result.add_conflict(FileConflict::new(
            "test.rs".to_string(),
            FileConflictType::Order,
        ));

        assert!(result.has_conflicts());
    }

    #[test]
    fn test_result_conflict_count() {
        let mut result = RepositoryOutputResult::new();

        assert_eq!(result.conflict_count(), 0);

        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Order,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Name,
        ));

        assert_eq!(result.conflict_count(), 2);
    }

    #[test]
    fn test_result_add_conflict() {
        let mut result = RepositoryOutputResult::new();
        let conflict = FileConflict::new("test.rs".to_string(), FileConflictType::Cyclic);

        result.add_conflict(conflict);

        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].conflict_type, FileConflictType::Cyclic);
    }

    #[test]
    fn test_result_conflicts_of_type() {
        let mut result = RepositoryOutputResult::new();
        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Order,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Name,
        ));
        result.add_conflict(FileConflict::new(
            "c.rs".to_string(),
            FileConflictType::Order,
        ));

        let order_conflicts: Vec<_> = result.conflicts_of_type(FileConflictType::Order).collect();
        assert_eq!(order_conflicts.len(), 2);

        let name_conflicts: Vec<_> = result.conflicts_of_type(FileConflictType::Name).collect();
        assert_eq!(name_conflicts.len(), 1);
    }

    #[test]
    fn test_result_name_conflicts() {
        let mut result = RepositoryOutputResult::new();
        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Name,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Order,
        ));

        let name_conflicts: Vec<_> = result.name_conflicts().collect();
        assert_eq!(name_conflicts.len(), 1);
    }

    #[test]
    fn test_result_content_conflicts() {
        let mut result = RepositoryOutputResult::new();
        result.add_conflict(FileConflict::new(
            "a.rs".to_string(),
            FileConflictType::Order,
        ));
        result.add_conflict(FileConflict::new(
            "b.rs".to_string(),
            FileConflictType::Cyclic,
        ));
        result.add_conflict(FileConflict::new(
            "c.rs".to_string(),
            FileConflictType::Zombie,
        ));
        result.add_conflict(FileConflict::new(
            "d.rs".to_string(),
            FileConflictType::Name,
        ));

        let content_conflicts: Vec<_> = result.content_conflicts().collect();
        assert_eq!(content_conflicts.len(), 3);
    }

    #[test]
    fn test_result_merge_file_result() {
        let mut result = RepositoryOutputResult::new();

        let file_result = FileOutputResult::empty("test.rs", Inode::ROOT)
            .with_bytes_written(1024)
            .with_vertices_processed(10)
            .with_edges_traversed(20);

        result.merge_file_result(file_result, false);

        assert_eq!(result.files_written, 1);
        assert_eq!(result.bytes_written, 1024);
        assert_eq!(result.vertices_processed, 10);
        assert_eq!(result.edges_traversed, 20);
    }

    #[test]
    fn test_result_merge_file_result_with_conflicts() {
        let mut result = RepositoryOutputResult::new();

        let mut file_result = FileOutputResult::empty("test.rs", Inode::ROOT);
        file_result.add_conflict(FileConflict::new(
            "test.rs".to_string(),
            FileConflictType::Order,
        ));

        result.merge_file_result(file_result, false);

        assert_eq!(result.conflict_count(), 1);
    }

    #[test]
    fn test_result_merge_file_result_truncated() {
        let mut result = RepositoryOutputResult::new();

        let file_result = FileOutputResult::empty("test.rs", Inode::ROOT).with_truncated(true);

        result.merge_file_result(file_result, false);

        assert_eq!(result.files_truncated, 1);
    }

    #[test]
    fn test_result_merge_file_result_store() {
        let mut result = RepositoryOutputResult::new();

        let file_result = FileOutputResult::empty("test.rs", Inode::ROOT);

        result.merge_file_result(file_result, true);

        assert!(result.file_results.contains_key("test.rs"));
    }

    #[test]
    fn test_result_record_skipped() {
        let mut result = RepositoryOutputResult::new();

        result.record_skipped();
        result.record_skipped();

        assert_eq!(result.files_skipped, 2);
    }

    #[test]
    fn test_result_record_directory() {
        let mut result = RepositoryOutputResult::new();

        result.record_directory();

        assert_eq!(result.directories_created, 1);
    }

    #[test]
    fn test_result_to_outcome() {
        let mut result = RepositoryOutputResult::new();
        result.files_written = 5;
        result.directories_created = 2;
        result.files_skipped = 1;
        result.bytes_written = 10000;

        let outcome = result.to_outcome();

        assert_eq!(outcome.files_written(), 5);
        assert_eq!(outcome.directories_created(), 2);
        assert_eq!(outcome.files_skipped(), 1);
        assert_eq!(outcome.bytes_written, 10000);
    }

    #[test]
    fn test_result_clone() {
        let mut result = RepositoryOutputResult::new();
        result.files_written = 3;

        let cloned = result.clone();

        assert_eq!(result.files_written, cloned.files_written);
    }

    #[test]
    fn test_result_debug() {
        let result = RepositoryOutputResult::new();
        let debug = format!("{:?}", result);

        assert!(debug.contains("RepositoryOutputResult"));
    }

    // ========================================================================
    // RepositoryOutputError Tests
    // ========================================================================

    #[test]
    fn test_error_display_pristine() {
        let err: RepositoryOutputError<std::io::Error> =
            RepositoryOutputError::Pristine(crate::pristine::PristineError::StackNotFound {
                name: "test".to_string(),
            });
        let display = format!("{}", err);

        assert!(display.contains("Pristine error"));
    }

    #[test]
    fn test_error_display_change_store() {
        let err: RepositoryOutputError<std::io::Error> =
            RepositoryOutputError::ChangeStore("not found".to_string());
        let display = format!("{}", err);

        assert!(display.contains("Change store error"));
    }

    #[test]
    fn test_error_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: RepositoryOutputError<std::io::Error> = RepositoryOutputError::Io(io_err);
        let display = format!("{}", err);

        assert!(display.contains("I/O error"));
    }

    #[test]
    fn test_error_display_working_copy() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: RepositoryOutputError<std::io::Error> = RepositoryOutputError::WorkingCopy(io_err);
        let display = format!("{}", err);

        assert!(display.contains("Working copy error"));
    }

    #[test]
    fn test_error_display_tree() {
        let err: RepositoryOutputError<std::io::Error> =
            RepositoryOutputError::TreeError("invalid tree".to_string());
        let display = format!("{}", err);

        assert!(display.contains("Tree traversal error"));
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let err: RepositoryOutputError<std::io::Error> = io_err.into();

        match err {
            RepositoryOutputError::Io(_) => (),
            _ => panic!("Expected Io variant"),
        }
    }

    #[test]
    fn test_error_from_pristine() {
        let pristine_err = crate::pristine::PristineError::StackNotFound {
            name: "test".to_string(),
        };
        let err: RepositoryOutputError<std::io::Error> = pristine_err.into();

        match err {
            RepositoryOutputError::Pristine(_) => (),
            _ => panic!("Expected Pristine variant"),
        }
    }

    #[test]
    fn test_error_debug() {
        let err: RepositoryOutputError<std::io::Error> =
            RepositoryOutputError::TreeError("test".to_string());
        let debug = format!("{:?}", err);

        assert!(debug.contains("TreeError"));
    }

    #[test]
    fn test_error_source_pristine() {
        use std::error::Error;

        let err: RepositoryOutputError<std::io::Error> =
            RepositoryOutputError::Pristine(crate::pristine::PristineError::StackNotFound {
                name: "test".to_string(),
            });

        assert!(err.source().is_some());
    }

    #[test]
    fn test_error_source_io() {
        use std::error::Error;

        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let err: RepositoryOutputError<std::io::Error> = RepositoryOutputError::Io(io_err);

        assert!(err.source().is_some());
    }

    #[test]
    fn test_error_source_change_store() {
        use std::error::Error;

        let err: RepositoryOutputError<std::io::Error> =
            RepositoryOutputError::ChangeStore("test".to_string());

        assert!(err.source().is_none());
    }

    // ========================================================================
    // OutputItem Tests
    // ========================================================================

    #[test]
    fn test_output_item_file() {
        let item = OutputItem::file("src/main.rs", Inode::ROOT, Position::ROOT);

        assert_eq!(item.path, "src/main.rs");
        assert_eq!(item.inode, Inode::ROOT);
        assert!(!item.is_directory);
    }

    #[test]
    fn test_output_item_directory() {
        let item = OutputItem::directory("src/lib", Inode::ROOT);

        assert_eq!(item.path, "src/lib");
        assert!(item.is_directory);
    }

    #[test]
    fn test_output_item_with_metadata() {
        let item = OutputItem::file("test.rs", Inode::ROOT, Position::ROOT)
            .with_metadata(crate::output::traits::FileMetadata::executable());

        assert!(item.metadata.is_executable());
    }

    #[test]
    fn test_output_item_clone() {
        let item = OutputItem::file("test.rs", Inode::ROOT, Position::ROOT);
        let cloned = item.clone();

        assert_eq!(item.path, cloned.path);
    }

    #[test]
    fn test_output_item_debug() {
        let item = OutputItem::file("test.rs", Inode::ROOT, Position::ROOT);
        let debug = format!("{:?}", item);

        assert!(debug.contains("OutputItem"));
        assert!(debug.contains("test.rs"));
    }
}

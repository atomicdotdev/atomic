//! Single file output from the repository graph.
//!
//! This module provides functionality for outputting a single file from the
//! repository graph to the working copy. It is the core building block for
//! the full repository output operation.
//!
//! # Overview
//!
//! File output is the process of reconstructing a file's content from the
//! repository graph and writing it to the working copy. This involves:
//!
//! 1. Retrieving the alive graph for the file's inode position
//! 2. Computing the topological order of vertices (via Tarjan's SCC)
//! 3. Writing content to the working copy with conflict markers if needed
//! 4. Tracking and returning any conflicts detected
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                       File Output Pipeline                               │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input                    Processing                    Output          │
//! │  ┌──────────────┐        ┌─────────────────┐          ┌────────────┐   │
//! │  │ - Inode      │        │ 1. retrieve     │          │ - File on  │   │
//! │  │ - Position   │ ─────▶ │ 2. order (SCC)  │ ───────▶ │   disk     │   │
//! │  │ - Options    │        │ 3. write        │          │ - Conflicts│   │
//! │  └──────────────┘        └─────────────────┘          └────────────┘   │
//! │                                                                         │
//! │  Error Handling:                                                        │
//! │  - Graph retrieval errors → FileOutputError::Graph                      │
//! │  - Content retrieval errors → FileOutputError::ChangeStore              │
//! │  - Write errors → FileOutputError::Io                                   │
//! │  - Working copy errors → FileOutputError::WorkingCopy                   │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::repo::{output_file, FileOutputOptions};
//! use atomic_core::types::Inode;
//!
//! // Output a single file
//! let options = FileOutputOptions::new();
//! let result = output_file(
//!     &txn,
//!     &changes,
//!     &working_copy,
//!     inode,
//!     position,
//!     "src/main.rs",
//!     options,
//! )?;
//!
//! // Check for conflicts
//! if result.has_conflicts() {
//!     println!("File has {} conflicts", result.conflicts.len());
//! }
//!
//! println!("Wrote {} bytes", result.bytes_written);
//! ```
//!
//! # Conflict Handling
//!
//! When conflicts are detected, the file is written with conflict markers:
//!
//! ```text
//! Normal content here
//! >>>>>>> 1 [ABCDEF12]
//! Content from one side
//! ======= 1 [GHIJKL34]
//! Content from other side
//! <<<<<<< 1
//! More normal content
//! ```
//!
//! The conflict markers include:
//! - Conflict ID (for matching begin/end markers)
//! - Change hash (to identify which change introduced the content)
//!
//! # Performance
//!
//! File output is O(V + E) where V is the number of vertices in the file's
//! subgraph and E is the number of edges. The SCC computation adds O(V + E)
//! for Tarjan's algorithm, giving overall O(V + E) complexity.

use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;

use crate::change::ChangeStore;
use crate::output::alive::{compute_order, retrieve_graph, RetrieveOptions};
use crate::output::traits::{WorkingCopy, Writer};
use crate::pristine::GraphTxnT;
use crate::types::{Hash, Inode, NodeId, Position};

use super::conflict::{FileConflict, FileConflictType};
use super::content::{output_graph_content_resolved, resolve_conflicts_semantically};
use super::error::OutputError;

// FILE OUTPUT OPTIONS

/// Options for single file output.
///
/// Controls how a file is retrieved from the graph and written to the
/// working copy.
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::FileOutputOptions;
///
/// // Default options
/// let opts = FileOutputOptions::new();
///
/// // Include deleted content (for showing full conflicts)
/// let opts = FileOutputOptions::new()
///     .include_deleted(true);
///
/// // Limit span count (for safety)
/// let opts = FileOutputOptions::new()
///     .max_vertices(10000);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileOutputOptions {
    /// Include deleted vertices in output.
    ///
    /// When true, deleted content will be included, wrapped in zombie
    /// conflict markers. Useful for showing complete conflict state.
    pub include_deleted: bool,

    /// Maximum vertices to process.
    ///
    /// Safety limit to prevent runaway processing on corrupted or
    /// extremely large files.
    pub max_vertices: Option<usize>,

    /// Whether to flush the writer after output.
    ///
    /// Defaults to true for safety. Set to false if you want to
    /// control flushing yourself.
    pub flush_after_write: bool,
}

impl FileOutputOptions {
    /// Create new options with defaults.
    ///
    /// Default configuration:
    /// - `include_deleted`: false
    /// - `max_vertices`: None (no limit)
    /// - `flush_after_write`: true
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileOutputOptions;
    ///
    /// let opts = FileOutputOptions::new();
    /// assert!(!opts.include_deleted);
    /// assert!(opts.max_vertices.is_none());
    /// assert!(opts.flush_after_write);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to include deleted vertices.
    ///
    /// # Arguments
    ///
    /// * `include` - Whether to include deleted content
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileOutputOptions;
    ///
    /// let opts = FileOutputOptions::new().include_deleted(true);
    /// assert!(opts.include_deleted);
    /// ```
    pub fn include_deleted(mut self, include: bool) -> Self {
        self.include_deleted = include;
        self
    }

    /// Set the maximum number of vertices to process.
    ///
    /// # Arguments
    ///
    /// * `max` - Maximum span count
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileOutputOptions;
    ///
    /// let opts = FileOutputOptions::new().max_vertices(5000);
    /// assert_eq!(opts.max_vertices, Some(5000));
    /// ```
    pub fn max_vertices(mut self, max: usize) -> Self {
        self.max_vertices = Some(max);
        self
    }

    /// Set whether to flush the writer after output.
    ///
    /// # Arguments
    ///
    /// * `flush` - Whether to flush after writing
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileOutputOptions;
    ///
    /// let opts = FileOutputOptions::new().flush_after_write(false);
    /// assert!(!opts.flush_after_write);
    /// ```
    pub fn flush_after_write(mut self, flush: bool) -> Self {
        self.flush_after_write = flush;
        self
    }

    /// Convert to retrieve options for the alive graph module.
    fn to_retrieve_options(self) -> RetrieveOptions {
        let mut opts = RetrieveOptions::default();
        if self.include_deleted {
            opts = opts.include_deleted(true);
        }
        if let Some(max) = self.max_vertices {
            opts = opts.max_vertices(max);
        }
        opts
    }
}

impl Default for FileOutputOptions {
    fn default() -> Self {
        Self {
            include_deleted: false,
            max_vertices: None,
            flush_after_write: true,
        }
    }
}

// FILE OUTPUT RESULT

/// Result of outputting a single file.
///
/// Contains information about what was written and any conflicts detected.
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::FileOutputResult;
///
/// // After calling output_file:
/// // let result = output_file(...)?;
/// //
/// // if result.has_conflicts() {
/// //     for conflict in &result.conflicts {
/// //         println!("Conflict at line {}", conflict.line().unwrap_or(0));
/// //     }
/// // }
/// //
/// // println!("Wrote {} bytes", result.bytes_written);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutputResult {
    /// Path of the file that was output.
    pub path: String,

    /// Inode of the file.
    pub inode: Inode,

    /// Number of bytes written to the file.
    pub bytes_written: u64,

    /// Number of vertices processed.
    pub vertices_processed: usize,

    /// Number of edges traversed during retrieval.
    pub edges_traversed: usize,

    /// Whether the graph retrieval was truncated.
    pub was_truncated: bool,

    /// Conflicts detected during output.
    pub conflicts: Vec<FileConflict>,
}

impl FileOutputResult {
    /// Create a new result.
    ///
    /// # Arguments
    ///
    /// * `path` - File path
    /// * `inode` - File inode
    fn new(path: impl Into<String>, inode: Inode) -> Self {
        Self {
            path: path.into(),
            inode,
            bytes_written: 0,
            vertices_processed: 0,
            edges_traversed: 0,
            was_truncated: false,
            conflicts: Vec::new(),
        }
    }

    /// Check if any conflicts were detected.
    ///
    /// # Returns
    ///
    /// `true` if at least one conflict exists.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileOutputResult;
    /// use atomic_core::types::Inode;
    ///
    /// let result = FileOutputResult::empty("test.rs", Inode::ROOT);
    /// assert!(!result.has_conflicts());
    /// ```
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Get the number of conflicts.
    ///
    /// # Returns
    ///
    /// Count of conflicts detected.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileOutputResult;
    /// use atomic_core::types::Inode;
    ///
    /// let result = FileOutputResult::empty("test.rs", Inode::ROOT);
    /// assert_eq!(result.conflict_count(), 0);
    /// ```
    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }

    /// Create an empty result (for testing or dry-run).
    ///
    /// # Arguments
    ///
    /// * `path` - File path
    /// * `inode` - File inode
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::FileOutputResult;
    /// use atomic_core::types::Inode;
    ///
    /// let result = FileOutputResult::empty("src/lib.rs", Inode::ROOT);
    /// assert_eq!(result.path, "src/lib.rs");
    /// assert_eq!(result.bytes_written, 0);
    /// ```
    pub fn empty(path: impl Into<String>, inode: Inode) -> Self {
        Self::new(path, inode)
    }

    /// Add a conflict to the result.
    ///
    /// # Arguments
    ///
    /// * `conflict` - The conflict to add
    pub fn add_conflict(&mut self, conflict: FileConflict) {
        self.conflicts.push(conflict);
    }

    /// Set the bytes written count.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Number of bytes written
    pub fn with_bytes_written(mut self, bytes: u64) -> Self {
        self.bytes_written = bytes;
        self
    }

    /// Set the vertices processed count.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of vertices processed
    pub fn with_vertices_processed(mut self, count: usize) -> Self {
        self.vertices_processed = count;
        self
    }

    /// Set the edges traversed count.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of edges traversed
    pub fn with_edges_traversed(mut self, count: usize) -> Self {
        self.edges_traversed = count;
        self
    }

    /// Set the truncation flag.
    ///
    /// # Arguments
    ///
    /// * `truncated` - Whether retrieval was truncated
    pub fn with_truncated(mut self, truncated: bool) -> Self {
        self.was_truncated = truncated;
        self
    }

    /// Set conflicts.
    ///
    /// # Arguments
    ///
    /// * `conflicts` - Vector of conflicts
    pub fn with_conflicts(mut self, conflicts: Vec<FileConflict>) -> Self {
        self.conflicts = conflicts;
        self
    }
}

// FILE OUTPUT ERROR

/// Error type for file output operations.
///
/// Wraps the various error types that can occur during file output into
/// a unified error type.
#[derive(Debug)]
pub enum FileOutputError<WE> {
    /// Error retrieving the graph from pristine.
    Graph(crate::pristine::PristineError),

    /// Error retrieving content from change store.
    ChangeStore(String),

    /// I/O error during write.
    Io(std::io::Error),

    /// Working copy error.
    WorkingCopy(WE),

    /// File position not found.
    PositionNotFound(Position<NodeId>),

    /// Inode not found in the tree.
    InodeNotFound(Inode),
}

impl<WE: std::fmt::Debug> std::fmt::Display for FileOutputError<WE> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Graph(e) => write!(f, "Graph error: {}", e),
            Self::ChangeStore(e) => write!(f, "Change store error: {}", e),
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::WorkingCopy(e) => write!(f, "Working copy error: {:?}", e),
            Self::PositionNotFound(pos) => write!(f, "Position not found: {:?}", pos),
            Self::InodeNotFound(inode) => write!(f, "Inode not found: {:?}", inode),
        }
    }
}

impl<WE: std::fmt::Debug + std::error::Error + 'static> std::error::Error for FileOutputError<WE> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::WorkingCopy(e) => Some(e),
            _ => None,
        }
    }
}

impl<WE> From<std::io::Error> for FileOutputError<WE> {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl<WE> From<crate::pristine::PristineError> for FileOutputError<WE> {
    fn from(e: crate::pristine::PristineError) -> Self {
        Self::Graph(e)
    }
}

impl<WE> From<OutputError> for FileOutputError<WE> {
    fn from(e: OutputError) -> Self {
        match e {
            OutputError::Io(e) => Self::Io(e),
            OutputError::ChangeStore(e) => Self::ChangeStore(e.to_string()),
            _ => Self::ChangeStore(e.to_string()),
        }
    }
}

// OUTPUT FILE FUNCTION

/// Output a single file from the repository graph to the working copy.
///
/// This is the core function for reconstructing file content from the graph.
/// It retrieves the alive graph for the file, computes the span ordering,
/// and writes the content to the working copy with conflict markers if needed.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph access
/// * `changes` - Change store for retrieving span content
/// * `working_copy` - Working copy to write to
/// * `inode` - Inode of the file
/// * `position` - Position in the graph (file's root span)
/// * `path` - Path to write the file to
/// * `options` - Output options
///
/// # Returns
///
/// A `FileOutputResult` containing statistics and any conflicts detected.
///
/// # Errors
///
/// Returns an error if:
/// - Graph retrieval fails
/// - Content retrieval fails
/// - Writing to the working copy fails
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::repo::{output_file, FileOutputOptions};
///
/// let result = output_file(
///     &txn,
///     &changes,
///     &working_copy,
///     inode,
///     position,
///     "src/main.rs",
///     FileOutputOptions::new(),
/// )?;
///
/// println!("Output {} bytes with {} conflicts",
///     result.bytes_written,
///     result.conflict_count());
/// ```
pub fn output_file<T, C, W>(
    txn: &T,
    changes: &C,
    working_copy: &W,
    inode: Inode,
    position: Position<NodeId>,
    path: &str,
    options: FileOutputOptions,
) -> Result<FileOutputResult, FileOutputError<W::Error>>
where
    T: GraphTxnT,
    C: ChangeStore,
    W: WorkingCopy,
    W::Writer: Write,
{
    output_file_with_filter(
        txn,
        changes,
        working_copy,
        inode,
        position,
        path,
        options,
        None,
    )
}

/// Output a single file from the graph with an optional change filter.
///
/// This is the core file output function that supports stack-aware output.
/// When a `change_filter` is provided, only vertices from changes in the filter
/// (or ROOT) will be included in the output.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph access
/// * `changes` - Change store for retrieving content
/// * `working_copy` - Working copy to write the file to
/// * `inode` - The file's inode
/// * `position` - Starting position in the graph
/// * `path` - Path where the file should be written
/// * `options` - Output options
/// * `change_filter` - Optional set of change NodeIds to include
///
/// # Returns
///
/// A `FileOutputResult` containing statistics and any conflicts.
#[allow(clippy::too_many_arguments)]
pub fn output_file_with_filter<T, C, W>(
    txn: &T,
    changes: &C,
    working_copy: &W,
    inode: Inode,
    position: Position<NodeId>,
    path: &str,
    options: FileOutputOptions,
    change_filter: Option<Arc<HashSet<NodeId>>>,
) -> Result<FileOutputResult, FileOutputError<W::Error>>
where
    T: GraphTxnT,
    C: ChangeStore,
    W: WorkingCopy,
    W::Writer: Write,
{
    // Initialize result
    let mut result = FileOutputResult::new(path, inode);

    // Retrieve the alive graph with optional change filter
    let mut retrieve_opts = options.to_retrieve_options();
    if let Some(filter) = change_filter {
        retrieve_opts = retrieve_opts.with_change_filter_arc(filter);
    }
    let retrieve_result = retrieve_graph(txn, position, retrieve_opts)?;

    result.vertices_processed = retrieve_result.graph.len_vertices();
    result.edges_traversed = retrieve_result.edges_traversed;
    result.was_truncated = retrieve_result.truncated;

    // Handle empty graph.
    //
    // When a change_filter is active (stack-aware output), an empty graph
    // means the file has no content on the target stack.  In that case we
    // must NOT create the file — it belongs to a different stack and should
    // not appear in the working copy.
    //
    // Without a change_filter the empty graph represents a genuinely empty
    // file, so we create it as before.
    if retrieve_result.graph.is_empty() {
        if retrieve_result.was_filtered {
            // File has no vertices after filtering — skip it entirely.
            return Ok(result);
        }
        // No filter active: create an empty file on disk.
        let writer = working_copy
            .write_file(path, inode)
            .map_err(FileOutputError::WorkingCopy)?;
        let mut writer = Writer::new(writer);
        if options.flush_after_write {
            writer.inner_mut().flush()?;
        }
        return Ok(result);
    }

    // Compute SCC ordering
    let mut graph = retrieve_result.graph;
    let order = compute_order(&mut graph);

    // Attempt semantic merge for any conflicting SCCs
    let resolved = resolve_conflicts_semantically(txn, changes, &graph, &order);

    // Create writer
    let file_writer = working_copy
        .write_file(path, inode)
        .map_err(FileOutputError::WorkingCopy)?;
    let mut writer = Writer::new(file_writer);

    // Hash function for conflict markers
    let hash_fn = |node_id: NodeId| -> Option<Hash> {
        if node_id.is_root() {
            return None;
        }
        txn.get_external(node_id).ok().flatten()
    };

    // Output the graph content (with semantic merge resolution)
    output_graph_content_resolved(changes, hash_fn, &graph, &order, &mut writer, &resolved)?;

    // Flush if requested
    if options.flush_after_write {
        writer.inner_mut().flush()?;
    }

    // Extract conflicts from order result.
    // Only count SCCs that were NOT resolved by the semantic merge engine.
    let effectively_resolved = resolved.resolved_count();
    let remaining_cyclic = order.cyclic_conflicts.saturating_sub(effectively_resolved);

    if remaining_cyclic > 0 {
        let mut conflict_id: u32 = 0;
        for scc in &order.sccs {
            if scc.len() > 1 && resolved.get_merged(scc[0]).is_none() {
                conflict_id += 1;
                let file_conflict = FileConflict::new(path.to_string(), FileConflictType::Cyclic)
                    .with_id(conflict_id);
                result.add_conflict(file_conflict);
            }
        }
    }

    Ok(result)
}

/// Output a file to a buffer instead of the working copy.
///
/// This is useful for testing, previewing, or cases where you want
/// the content in memory rather than written to disk.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph access
/// * `changes` - Change store for retrieving span content
/// * `position` - Position in the graph
/// * `options` - Output options
///
/// # Returns
///
/// A tuple of (content bytes, conflicts).
///
/// # Errors
///
/// Returns an error if graph or content retrieval fails.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::repo::{output_file_to_buffer, FileOutputOptions};
///
/// let (content, conflicts) = output_file_to_buffer(
///     &txn,
///     &changes,
///     position,
///     FileOutputOptions::new(),
/// )?;
///
/// let text = String::from_utf8_lossy(&content);
/// println!("File content:\n{}", text);
/// ```
pub fn output_file_to_buffer<T, C>(
    txn: &T,
    changes: &C,
    position: Position<NodeId>,
    options: FileOutputOptions,
) -> Result<(Vec<u8>, Vec<FileConflict>), OutputError>
where
    T: GraphTxnT,
    C: ChangeStore,
{
    // Retrieve the alive graph
    let retrieve_opts = options.to_retrieve_options();
    let retrieve_result = retrieve_graph(txn, position, retrieve_opts)
        .map_err(|e| OutputError::Pristine(Box::new(e)))?;

    // Handle empty graph
    if retrieve_result.graph.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    // Compute SCC ordering
    let mut graph = retrieve_result.graph;
    let order = compute_order(&mut graph);

    // Create buffer writer
    let buffer = Vec::new();
    let mut writer = Writer::new(buffer);

    // Hash function to convert NodeId to Hash using the transaction.
    // This is required for the ChangeStore to load the correct change file
    // and retrieve the content bytes for each span.
    let hash_fn = |node_id: NodeId| -> Option<Hash> {
        // Handle ROOT node - it has no hash
        if node_id.is_root() {
            return None;
        }
        // Use transaction's get_external to convert NodeId to Hash
        txn.get_external(node_id).ok().flatten()
    };

    // Attempt semantic merge for any conflicting SCCs
    let resolved = resolve_conflicts_semantically(txn, changes, &graph, &order);

    // Output the graph content (with semantic merge resolution)
    output_graph_content_resolved(changes, hash_fn, &graph, &order, &mut writer, &resolved)?;

    // Extract buffer
    let content = writer.into_inner();

    // Extract conflicts from cyclic SCCs (only those NOT resolved by semantic merge)
    let mut conflicts = Vec::new();
    let mut conflict_id: u32 = 0;
    for scc in &order.sccs {
        if scc.len() > 1 && resolved.get_merged(scc[0]).is_none() {
            conflict_id += 1;
            conflicts.push(
                FileConflict::new(String::new(), FileConflictType::Cyclic).with_id(conflict_id),
            );
        }
    }

    Ok((content, conflicts))
}

/// Output a file's content to a buffer with explicit retrieve options.
///
/// This is a lower-level function that allows passing custom [`RetrieveOptions`]
/// directly, enabling features like change filtering for state-based content
/// retrieval.
///
/// # State-Based Content Retrieval
///
/// The primary use case for this function is retrieving file content at a
/// specific historical state. By setting a change filter in the options,
/// you can retrieve content as it existed before or after a specific change:
///
/// ```text
/// Full Graph Timeline:
///   seq 0    seq 1    seq 2    seq 3    seq 4
///   ──┬────────┬────────┬────────┬────────┬──
///     │        │        │        │        │
///   Add A   Edit A   Add B   Edit A   Edit B
///
/// To see file A before seq 3:
///   filter = {change_0, change_1}  → content from seq 0 + seq 1
///
/// To see file A after seq 3:
///   filter = {change_0, change_1, change_3}  → includes the edit
/// ```
///
/// # Arguments
///
/// * `txn` - Transaction providing graph access
/// * `changes` - Change store for span content
/// * `position` - Starting position in the graph (file's inode position)
/// * `file_options` - Basic file output options (max_vertices, include_deleted)
/// * `retrieve_options` - Advanced retrieve options including change filter
///
/// # Returns
///
/// A tuple of:
/// - `Vec<u8>` - The file content at the filtered state
/// - `Vec<FileConflict>` - Any conflicts detected
///
/// # Errors
///
/// Returns `OutputError` if graph traversal, content retrieval, or writing fails.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::alive::RetrieveOptions;
/// use atomic_core::output::repo::{output_file_to_buffer_with_options, FileOutputOptions};
/// use std::collections::HashSet;
///
/// // Get changes applied before a specific change
/// let change_set: HashSet<NodeId> = get_changes_up_to_sequence(&txn, &stack, 5)?;
///
/// // Create retrieve options with the filter
/// let retrieve_opts = RetrieveOptions::new().with_change_filter(change_set);
/// let file_opts = FileOutputOptions::new();
///
/// // Get content at that historical state
/// let (content, conflicts) = output_file_to_buffer_with_options(
///     &txn,
///     &changes,
///     position,
///     file_opts,
///     retrieve_opts,
/// )?;
///
/// println!("Content at historical state: {} bytes", content.len());
/// ```
///
/// # Performance
///
/// The change filter is applied during graph traversal, so vertices from
/// excluded changes are never visited. This is efficient even for large
/// repositories with extensive history.
pub fn output_file_to_buffer_with_options<T, C>(
    txn: &T,
    changes: &C,
    position: Position<NodeId>,
    _file_options: FileOutputOptions,
    retrieve_options: RetrieveOptions,
) -> Result<(Vec<u8>, Vec<FileConflict>), OutputError>
where
    T: GraphTxnT,
    C: ChangeStore,
{
    // Retrieve the alive graph with the provided options (including change filter)
    let retrieve_result = retrieve_graph(txn, position, retrieve_options)
        .map_err(|e| OutputError::Pristine(Box::new(e)))?;

    // Handle empty graph (no content at this state)
    if retrieve_result.graph.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    // Compute SCC ordering
    let mut graph = retrieve_result.graph;
    let order = compute_order(&mut graph);

    // Create buffer writer
    let buffer = Vec::new();
    let mut writer = Writer::new(buffer);

    // Hash function to convert NodeId to Hash using the transaction.
    // This is required for the ChangeStore to load the correct change file
    // and retrieve the content bytes for each span.
    let hash_fn = |node_id: NodeId| -> Option<Hash> {
        // Handle ROOT node - it has no hash
        if node_id.is_root() {
            return None;
        }
        // Use transaction's get_external to convert NodeId to Hash
        txn.get_external(node_id).ok().flatten()
    };

    // Attempt semantic merge for any conflicting SCCs
    let resolved = resolve_conflicts_semantically(txn, changes, &graph, &order);

    // Output the graph content (with semantic merge resolution)
    output_graph_content_resolved(changes, hash_fn, &graph, &order, &mut writer, &resolved)?;

    // Extract buffer
    let content = writer.into_inner();

    // Extract conflicts from cyclic SCCs (only those NOT resolved by semantic merge)
    let mut conflicts = Vec::new();
    let mut conflict_id: u32 = 0;
    for scc in &order.sccs {
        if scc.len() > 1 && resolved.get_merged(scc[0]).is_none() {
            conflict_id += 1;
            conflicts.push(
                FileConflict::new(String::new(), FileConflictType::Cyclic).with_id(conflict_id),
            );
        }
    }

    Ok((content, conflicts))
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;

    // FileOutputOptions Tests

    #[test]
    fn test_options_new() {
        let opts = FileOutputOptions::new();

        assert!(!opts.include_deleted);
        assert!(opts.max_vertices.is_none());
        assert!(opts.flush_after_write);
    }

    #[test]
    fn test_options_default() {
        let opts = FileOutputOptions::default();

        assert!(!opts.include_deleted);
        assert!(opts.max_vertices.is_none());
        assert!(opts.flush_after_write);
    }

    #[test]
    fn test_options_include_deleted() {
        let opts = FileOutputOptions::new().include_deleted(true);

        assert!(opts.include_deleted);
    }

    #[test]
    fn test_options_include_deleted_false() {
        let opts = FileOutputOptions::new()
            .include_deleted(true)
            .include_deleted(false);

        assert!(!opts.include_deleted);
    }

    #[test]
    fn test_options_max_vertices() {
        let opts = FileOutputOptions::new().max_vertices(1000);

        assert_eq!(opts.max_vertices, Some(1000));
    }

    #[test]
    fn test_options_max_vertices_zero() {
        let opts = FileOutputOptions::new().max_vertices(0);

        assert_eq!(opts.max_vertices, Some(0));
    }

    #[test]
    fn test_options_flush_after_write() {
        let opts = FileOutputOptions::new().flush_after_write(false);

        assert!(!opts.flush_after_write);
    }

    #[test]
    fn test_options_flush_after_write_true() {
        let opts = FileOutputOptions::new()
            .flush_after_write(false)
            .flush_after_write(true);

        assert!(opts.flush_after_write);
    }

    #[test]
    fn test_options_chaining() {
        let opts = FileOutputOptions::new()
            .include_deleted(true)
            .max_vertices(5000)
            .flush_after_write(false);

        assert!(opts.include_deleted);
        assert_eq!(opts.max_vertices, Some(5000));
        assert!(!opts.flush_after_write);
    }

    #[test]
    fn test_options_clone() {
        let opts = FileOutputOptions::new().max_vertices(100);
        let cloned = opts;

        assert_eq!(opts, cloned);
    }

    #[test]
    fn test_options_debug() {
        let opts = FileOutputOptions::new();
        let debug = format!("{:?}", opts);

        assert!(debug.contains("FileOutputOptions"));
    }

    #[test]
    fn test_options_to_retrieve_options_default() {
        let opts = FileOutputOptions::new();
        let retrieve = opts.to_retrieve_options();

        assert!(!retrieve.include_deleted);
        assert!(retrieve.max_vertices.is_none());
    }

    #[test]
    fn test_options_to_retrieve_options_with_deleted() {
        let opts = FileOutputOptions::new().include_deleted(true);
        let retrieve = opts.to_retrieve_options();

        assert!(retrieve.include_deleted);
    }

    #[test]
    fn test_options_to_retrieve_options_with_max() {
        let opts = FileOutputOptions::new().max_vertices(500);
        let retrieve = opts.to_retrieve_options();

        assert_eq!(retrieve.max_vertices, Some(500));
    }

    // FileOutputResult Tests

    #[test]
    fn test_result_new() {
        let result = FileOutputResult::new("test.rs", Inode::ROOT);

        assert_eq!(result.path, "test.rs");
        assert_eq!(result.inode, Inode::ROOT);
        assert_eq!(result.bytes_written, 0);
        assert_eq!(result.vertices_processed, 0);
        assert_eq!(result.edges_traversed, 0);
        assert!(!result.was_truncated);
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn test_result_empty() {
        let result = FileOutputResult::empty("lib.rs", Inode::ROOT);

        assert_eq!(result.path, "lib.rs");
        assert_eq!(result.bytes_written, 0);
    }

    #[test]
    fn test_result_has_conflicts_empty() {
        let result = FileOutputResult::empty("test.rs", Inode::ROOT);

        assert!(!result.has_conflicts());
    }

    #[test]
    fn test_result_has_conflicts_with_conflict() {
        let mut result = FileOutputResult::empty("test.rs", Inode::ROOT);
        result.add_conflict(FileConflict::new(
            "test.rs".to_string(),
            FileConflictType::Order,
        ));

        assert!(result.has_conflicts());
    }

    #[test]
    fn test_result_conflict_count() {
        let mut result = FileOutputResult::empty("test.rs", Inode::ROOT);

        assert_eq!(result.conflict_count(), 0);

        result.add_conflict(FileConflict::new(
            "test.rs".to_string(),
            FileConflictType::Order,
        ));
        assert_eq!(result.conflict_count(), 1);

        result.add_conflict(FileConflict::new(
            "test.rs".to_string(),
            FileConflictType::Cyclic,
        ));
        assert_eq!(result.conflict_count(), 2);
    }

    #[test]
    fn test_result_add_conflict() {
        let mut result = FileOutputResult::empty("test.rs", Inode::ROOT);
        let conflict = FileConflict::new("test.rs".to_string(), FileConflictType::Zombie);

        result.add_conflict(conflict);

        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].conflict_type, FileConflictType::Zombie);
    }

    #[test]
    fn test_result_with_bytes_written() {
        let result = FileOutputResult::empty("test.rs", Inode::ROOT).with_bytes_written(1024);

        assert_eq!(result.bytes_written, 1024);
    }

    #[test]
    fn test_result_with_vertices_processed() {
        let result = FileOutputResult::empty("test.rs", Inode::ROOT).with_vertices_processed(50);

        assert_eq!(result.vertices_processed, 50);
    }

    #[test]
    fn test_result_with_edges_traversed() {
        let result = FileOutputResult::empty("test.rs", Inode::ROOT).with_edges_traversed(100);

        assert_eq!(result.edges_traversed, 100);
    }

    #[test]
    fn test_result_with_truncated() {
        let result = FileOutputResult::empty("test.rs", Inode::ROOT).with_truncated(true);

        assert!(result.was_truncated);
    }

    #[test]
    fn test_result_with_conflicts() {
        let conflicts = vec![
            FileConflict::new("test.rs".to_string(), FileConflictType::Order),
            FileConflict::new("test.rs".to_string(), FileConflictType::Cyclic),
        ];
        let result =
            FileOutputResult::empty("test.rs", Inode::ROOT).with_conflicts(conflicts.clone());

        assert_eq!(result.conflict_count(), 2);
    }

    #[test]
    fn test_result_builder_chain() {
        let result = FileOutputResult::empty("test.rs", Inode::ROOT)
            .with_bytes_written(2048)
            .with_vertices_processed(25)
            .with_edges_traversed(50)
            .with_truncated(false);

        assert_eq!(result.bytes_written, 2048);
        assert_eq!(result.vertices_processed, 25);
        assert_eq!(result.edges_traversed, 50);
        assert!(!result.was_truncated);
    }

    #[test]
    fn test_result_clone() {
        let mut result = FileOutputResult::empty("test.rs", Inode::ROOT);
        result.add_conflict(FileConflict::new(
            "test.rs".to_string(),
            FileConflictType::Order,
        ));

        let cloned = result.clone();

        assert_eq!(result.path, cloned.path);
        assert_eq!(result.conflict_count(), cloned.conflict_count());
    }

    #[test]
    fn test_result_debug() {
        let result = FileOutputResult::empty("test.rs", Inode::ROOT);
        let debug = format!("{:?}", result);

        assert!(debug.contains("FileOutputResult"));
        assert!(debug.contains("test.rs"));
    }

    #[test]
    fn test_result_eq() {
        let result1 = FileOutputResult::empty("test.rs", Inode::ROOT);
        let result2 = FileOutputResult::empty("test.rs", Inode::ROOT);

        assert_eq!(result1, result2);
    }

    #[test]
    fn test_result_ne_path() {
        let result1 = FileOutputResult::empty("test.rs", Inode::ROOT);
        let result2 = FileOutputResult::empty("other.rs", Inode::ROOT);

        assert_ne!(result1, result2);
    }

    // FileOutputError Tests

    #[test]
    fn test_error_display_graph() {
        let err: FileOutputError<std::io::Error> =
            FileOutputError::Graph(crate::pristine::PristineError::ViewNotFound {
                name: "test".to_string(),
            });
        let display = format!("{}", err);

        assert!(display.contains("Graph error"));
    }

    #[test]
    fn test_error_display_change_store() {
        let err: FileOutputError<std::io::Error> =
            FileOutputError::ChangeStore("content not found".to_string());
        let display = format!("{}", err);

        assert!(display.contains("Change store error"));
        assert!(display.contains("content not found"));
    }

    #[test]
    fn test_error_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: FileOutputError<std::io::Error> = FileOutputError::Io(io_err);
        let display = format!("{}", err);

        assert!(display.contains("I/O error"));
    }

    #[test]
    fn test_error_display_working_copy() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err: FileOutputError<std::io::Error> = FileOutputError::WorkingCopy(io_err);
        let display = format!("{}", err);

        assert!(display.contains("Working copy error"));
    }

    #[test]
    fn test_error_display_position_not_found() {
        let pos = Position::ROOT;
        let err: FileOutputError<std::io::Error> = FileOutputError::PositionNotFound(pos);
        let display = format!("{}", err);

        assert!(display.contains("Position not found"));
    }

    #[test]
    fn test_error_display_inode_not_found() {
        let err: FileOutputError<std::io::Error> = FileOutputError::InodeNotFound(Inode::ROOT);
        let display = format!("{}", err);

        assert!(display.contains("Inode not found"));
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test error");
        let err: FileOutputError<std::io::Error> = io_err.into();

        match err {
            FileOutputError::Io(_) => (),
            _ => panic!("Expected Io variant"),
        }
    }

    #[test]
    fn test_error_from_pristine() {
        let pristine_err = crate::pristine::PristineError::ViewNotFound {
            name: "test".to_string(),
        };
        let err: FileOutputError<std::io::Error> = pristine_err.into();

        match err {
            FileOutputError::Graph(_) => (),
            _ => panic!("Expected Graph variant"),
        }
    }

    #[test]
    fn test_error_from_output_error_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let output_err = OutputError::Io(io_err);
        let err: FileOutputError<std::io::Error> = output_err.into();

        match err {
            FileOutputError::Io(_) => (),
            _ => panic!("Expected Io variant"),
        }
    }

    #[test]
    fn test_error_from_output_error_change_store() {
        let boxed_err: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        let output_err = OutputError::ChangeStore(boxed_err);
        let err: FileOutputError<std::io::Error> = output_err.into();

        match err {
            FileOutputError::ChangeStore(s) => assert!(s.contains("missing")),
            _ => panic!("Expected ChangeStore variant"),
        }
    }

    #[test]
    fn test_error_debug() {
        let err: FileOutputError<std::io::Error> = FileOutputError::ChangeStore("test".to_string());
        let debug = format!("{:?}", err);

        assert!(debug.contains("ChangeStore"));
    }

    #[test]
    fn test_error_source_graph() {
        use std::error::Error;

        let err: FileOutputError<std::io::Error> =
            FileOutputError::Graph(crate::pristine::PristineError::ViewNotFound {
                name: "test".to_string(),
            });

        assert!(err.source().is_some());
    }

    #[test]
    fn test_error_source_io() {
        use std::error::Error;

        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let err: FileOutputError<std::io::Error> = FileOutputError::Io(io_err);

        assert!(err.source().is_some());
    }

    #[test]
    fn test_error_source_change_store() {
        use std::error::Error;

        let err: FileOutputError<std::io::Error> = FileOutputError::ChangeStore("test".to_string());

        assert!(err.source().is_none());
    }

    #[test]
    fn test_error_source_position_not_found() {
        use std::error::Error;

        let err: FileOutputError<std::io::Error> =
            FileOutputError::PositionNotFound(Position::ROOT);

        assert!(err.source().is_none());
    }

    #[test]
    fn test_error_source_inode_not_found() {
        use std::error::Error;

        let err: FileOutputError<std::io::Error> = FileOutputError::InodeNotFound(Inode::ROOT);

        assert!(err.source().is_none());
    }
}

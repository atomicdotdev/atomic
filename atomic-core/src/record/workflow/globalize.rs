//! Globalization of local hunks to graph operations.
//!
//! This module converts local working copy changes (represented as [`BuiltHunk`])
//! into graph-compatible operations ([`GraphOp<Option<Hash>>`]) that can be applied
//! to the repository graph.
//!
//! # Overview
//!
//! "Globalization" is the process of converting local, file-centric change
//! representations into the global graph coordinate system used by Atomic.
//! This involves:
//!
//! 1. **Position Resolution**: Converting file paths and line numbers to graph
//!    positions (vertices and edges)
//! 2. **Span Creation**: Building [`Insertion`] structures that insert content
//!    into the graph with proper context
//! 3. **Edge Creation**: Building [`EdgeUpdate`] structures that mark existing
//!    content as deleted
//! 4. **Dependency Tracking**: Recording which existing changes the new change
//!    depends on
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      Globalization Pipeline                             │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  BuiltHunk                 GlobalizeContext              GraphOp<Option<H>>│
//! │  ┌──────────────┐         ┌───────────────┐            ┌──────────────┐│
//! │  │ path: String │         │ txn: &T       │            │ FileAdd {    ││
//! │  │ line: u64    │  ────►  │ stack: &Stack │  ────►     │   add_name   ││
//! │  │ kind: Insert │         │ content_pos   │            │   add_inode  ││
//! │  │ content: ..  │         │ dependencies  │            │   contents   ││
//! │  └──────────────┘         └───────────────┘            │ }            ││
//! │                                                         └──────────────┘│
//! │                                                                         │
//! │  Position Resolution:                                                   │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ path "src/main.rs"  ──►  inode(42)  ──►  Position(change, pos)   │  │
//! │  │ line 100            ──►  find span containing line 100         │  │
//! │  │                     ──►  predecessors / successors               │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Concepts
//!
//! ## Context
//!
//! When inserting new content, we specify **context** - the vertices that should
//! come before (`predecessors`) and after (`successors`) the new content. This
//! allows Atomic to correctly position content even when merging independent
//! changes.
//!
//! ## Inode Resolution
//!
//! Files are identified by **inodes** - stable identifiers that survive renames.
//! The globalization process resolves file paths to inodes, then inodes to graph
//! positions.
//!
//! ## Content Positions
//!
//! New content is appended to a content buffer. The `ChangePosition` values in
//! `Insertion` reference byte ranges within this buffer.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::globalize::{
//!     GlobalizeContext, GlobalizeOptions, globalize_recorded_file,
//! };
//!
//! // Set up context
//! let mut ctx = GlobalizeContext::new(&txn, content_buffer);
//!
//! // Globalize a recorded file
//! let hunks = globalize_recorded_file(&mut ctx, &recorded_file, &options)?;
//!
//! // The result contains graph-ready hunks
//! for graph_op in &hunks {
//!     change.add_hunk(graph_op.clone());
//! }
//! ```
//!
//! # Error Handling
//!
//! Globalization can fail for several reasons:
//!
//! - **Path not found**: The file path doesn't exist in the tree
//! - **Inode not found**: The inode has no graph position
//! - **Position not found**: Cannot find the span for a line number
//! - **Missing context**: Cannot determine up/down context for insertion
//!
//! See [`GlobalizeError`] for the complete list.

use std::collections::HashSet;
use std::fmt;

use crate::output::alive::{retrieve_graph, RetrieveOptions};

use thiserror::Error;

use crate::change::{Atom, EdgeUpdate, Encoding, GraphOp, Insertion, NewEdge};
use crate::pristine::{GraphTxnT, PristineError, TreeTxnT};
use crate::types::{ChangePosition, EdgeFlags, GraphNode, Hash, Inode, NodeId, Position};

use super::graph_op::{BuiltHunk, BuiltHunkKind};
use super::record::RecordedFile;

// ============================================================================
// ERROR TYPES
// ============================================================================

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

// ============================================================================
// CONFIGURATION
// ============================================================================

/// Configuration options for globalization.
///
/// Controls how local hunks are converted to graph operations.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::globalize::GlobalizeOptions;
///
/// let options = GlobalizeOptions::new()
///     .include_empty_files(false)
///     .validate_positions(true);
///
/// assert!(!options.get_include_empty_files());
/// assert!(options.get_validate_positions());
/// ```
#[derive(Debug, Clone)]
pub struct GlobalizeOptions {
    /// Whether to include empty files in the output.
    ///
    /// If false, files with no content hunks are skipped.
    /// Default: false
    include_empty_files: bool,

    /// Whether to validate that positions exist in the graph.
    ///
    /// Enabling this adds overhead but catches errors early.
    /// Default: true
    validate_positions: bool,

    /// Maximum content size per graph_op (bytes).
    ///
    /// Larger hunks are split. 0 means no limit.
    /// Default: 0 (no limit)
    max_hunk_size: usize,

    /// Default encoding for files without detected encoding.
    ///
    /// Default: UTF-8
    default_encoding: Encoding,
}

impl GlobalizeOptions {
    /// Create new options with default values.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::globalize::GlobalizeOptions;
    ///
    /// let options = GlobalizeOptions::new();
    /// assert!(options.get_validate_positions());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to include empty files.
    ///
    /// # Arguments
    ///
    /// * `include` - Whether to include files with no content
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::globalize::GlobalizeOptions;
    ///
    /// let options = GlobalizeOptions::new().include_empty_files(true);
    /// assert!(options.get_include_empty_files());
    /// ```
    #[must_use]
    pub fn include_empty_files(mut self, include: bool) -> Self {
        self.include_empty_files = include;
        self
    }

    /// Set whether to validate positions.
    ///
    /// # Arguments
    ///
    /// * `validate` - Whether to validate graph positions
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::globalize::GlobalizeOptions;
    ///
    /// let options = GlobalizeOptions::new().validate_positions(false);
    /// assert!(!options.get_validate_positions());
    /// ```
    #[must_use]
    pub fn validate_positions(mut self, validate: bool) -> Self {
        self.validate_positions = validate;
        self
    }

    /// Set maximum graph_op size.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum bytes per graph_op (0 = no limit)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::globalize::GlobalizeOptions;
    ///
    /// let options = GlobalizeOptions::new().max_hunk_size(1024 * 1024);
    /// assert_eq!(options.get_max_hunk_size(), 1024 * 1024);
    /// ```
    #[must_use]
    pub fn max_hunk_size(mut self, size: usize) -> Self {
        self.max_hunk_size = size;
        self
    }

    /// Set default encoding.
    ///
    /// # Arguments
    ///
    /// * `encoding` - Default encoding for files
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::globalize::GlobalizeOptions;
    /// use atomic_core::change::Encoding;
    ///
    /// let options = GlobalizeOptions::new().default_encoding(Encoding::Binary);
    /// assert_eq!(options.get_default_encoding(), Encoding::Binary);
    /// ```
    #[must_use]
    pub fn default_encoding(mut self, encoding: Encoding) -> Self {
        self.default_encoding = encoding;
        self
    }

    /// Get whether empty files are included.
    #[must_use]
    pub fn get_include_empty_files(&self) -> bool {
        self.include_empty_files
    }

    /// Get whether positions are validated.
    #[must_use]
    pub fn get_validate_positions(&self) -> bool {
        self.validate_positions
    }

    /// Get maximum graph_op size.
    #[must_use]
    pub fn get_max_hunk_size(&self) -> usize {
        self.max_hunk_size
    }

    /// Get default encoding.
    #[must_use]
    pub fn get_default_encoding(&self) -> Encoding {
        self.default_encoding
    }
}

impl Default for GlobalizeOptions {
    fn default() -> Self {
        Self {
            include_empty_files: false,
            validate_positions: true,
            max_hunk_size: 0,
            default_encoding: Encoding::Utf8,
        }
    }
}

// ============================================================================
// CONTEXT
// ============================================================================

/// Context for globalization operations.
///
/// Holds state needed during the globalization process, including:
/// - A reference to the transaction for graph lookups
/// - The content buffer for accumulating new content
/// - Dependency tracking
/// - Position caching for performance
///
/// # Lifetime Parameters
///
/// - `'txn`: The lifetime of the transaction reference
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::globalize::GlobalizeContext;
///
/// let mut ctx = GlobalizeContext::new(&txn);
///
/// // Append content and get position
/// let (start, end) = ctx.append_content(b"Hello, world!");
///
/// // Track a dependency
/// ctx.add_dependency(existing_change_hash);
/// ```
pub struct GlobalizeContext<'txn, T> {
    /// Reference to the transaction for graph lookups.
    txn: &'txn T,

    /// Content buffer for new content.
    ///
    /// Hunks reference byte ranges within this buffer.
    content: Vec<u8>,

    /// Current position in the content buffer.
    content_position: u64,

    /// Dependencies collected during globalization.
    ///
    /// These are hashes of changes that the new change depends on.
    dependencies: HashSet<Hash>,

    /// Cache of resolved inodes.
    ///
    /// Maps paths to their resolved inodes for performance.
    inode_cache: std::collections::HashMap<String, Inode>,

    /// Cache of inode positions.
    ///
    /// Maps inodes to their graph positions.
    position_cache: std::collections::HashMap<Inode, Position<NodeId>>,
}

impl<'txn, T> GlobalizeContext<'txn, T>
where
    T: GraphTxnT + TreeTxnT,
{
    /// Create a new globalization context.
    ///
    /// # Arguments
    ///
    /// * `txn` - Transaction for graph lookups
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let ctx = GlobalizeContext::new(&txn);
    /// assert!(ctx.dependencies().is_empty());
    /// ```
    pub fn new(txn: &'txn T) -> Self {
        Self {
            txn,
            content: Vec::new(),
            content_position: 0,
            dependencies: HashSet::new(),
            inode_cache: std::collections::HashMap::new(),
            position_cache: std::collections::HashMap::new(),
        }
    }

    /// Create a context with pre-allocated content buffer.
    ///
    /// # Arguments
    ///
    /// * `txn` - Transaction for graph lookups
    /// * `capacity` - Initial capacity for content buffer
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let ctx = GlobalizeContext::with_capacity(&txn, 1024 * 1024);
    /// ```
    pub fn with_capacity(txn: &'txn T, capacity: usize) -> Self {
        Self {
            txn,
            content: Vec::with_capacity(capacity),
            content_position: 0,
            dependencies: HashSet::new(),
            inode_cache: std::collections::HashMap::new(),
            position_cache: std::collections::HashMap::new(),
        }
    }

    /// Append content to the buffer and return the position range.
    ///
    /// # Arguments
    ///
    /// * `data` - Content bytes to append
    ///
    /// # Returns
    ///
    /// A tuple of (start_position, end_position) for the appended content.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (start, end) = ctx.append_content(b"Hello");
    /// assert_eq!(end - start, 5);
    /// ```
    pub fn append_content(&mut self, data: &[u8]) -> (ChangePosition, ChangePosition) {
        let start = ChangePosition::new(self.content_position);
        self.content.extend_from_slice(data);
        self.content_position += data.len() as u64;
        let end = ChangePosition::new(self.content_position);
        (start, end)
    }

    /// Add a dependency on an existing change.
    ///
    /// Dependencies are automatically deduplicated.
    ///
    /// # Arguments
    ///
    /// * `hash` - Hash of the change to depend on
    pub fn add_dependency(&mut self, hash: Hash) {
        self.dependencies.insert(hash);
    }

    /// Add a dependency by node ID, looking up the hash.
    ///
    /// # Arguments
    ///
    /// * `node_id` - Internal ID of the change
    ///
    /// # Returns
    ///
    /// Ok(()) if the dependency was added, or an error if the node ID
    /// has no associated hash.
    pub fn add_dependency_by_id(&mut self, node_id: NodeId) -> GlobalizeResult<()> {
        if node_id == NodeId::ROOT {
            // Root node has no hash dependency
            return Ok(());
        }
        if let Some(hash) = self.txn.get_external(node_id)? {
            self.dependencies.insert(hash);
        }
        Ok(())
    }

    /// Get the collected dependencies.
    ///
    /// # Returns
    ///
    /// A reference to the set of dependency hashes.
    #[must_use]
    pub fn dependencies(&self) -> &HashSet<Hash> {
        &self.dependencies
    }

    /// Get the dependencies as a sorted vector.
    ///
    /// Sorting ensures deterministic change hashes.
    ///
    /// # Returns
    ///
    /// A vector of dependency hashes in sorted order.
    #[must_use]
    pub fn dependencies_sorted(&self) -> Vec<Hash> {
        let mut deps: Vec<Hash> = self.dependencies.iter().copied().collect();
        deps.sort();
        deps
    }

    /// Take ownership of the content buffer.
    ///
    /// After calling this, the context's content buffer is empty.
    ///
    /// # Returns
    ///
    /// The accumulated content bytes.
    #[must_use]
    pub fn take_content(self) -> Vec<u8> {
        self.content
    }

    /// Get a reference to the content buffer.
    #[must_use]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Get the current content position (total bytes appended).
    #[must_use]
    pub fn content_len(&self) -> u64 {
        self.content_position
    }

    /// Get a reference to the transaction.
    #[must_use]
    pub fn txn(&self) -> &'txn T {
        self.txn
    }

    /// Get the external hash for a node ID.
    ///
    /// # Arguments
    ///
    /// * `node_id` - Internal node ID to look up
    ///
    /// # Returns
    ///
    /// The external hash if found, None if the node ID is ROOT or not found.
    #[must_use]
    pub fn get_external(&self, node_id: NodeId) -> Option<Hash> {
        if node_id == NodeId::ROOT {
            return None;
        }
        self.txn.get_external(node_id).ok().flatten()
    }

    /// Clear the caches.
    ///
    /// Call this if the underlying graph has changed.
    pub fn clear_caches(&mut self) {
        self.inode_cache.clear();
        self.position_cache.clear();
    }

    /// Get cache statistics for debugging.
    #[must_use]
    pub fn cache_stats(&self) -> CacheStats {
        CacheStats {
            inode_cache_size: self.inode_cache.len(),
            position_cache_size: self.position_cache.len(),
        }
    }
}

/// Statistics about the globalization context caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    /// Number of entries in the inode cache.
    pub inode_cache_size: usize,
    /// Number of entries in the position cache.
    pub position_cache_size: usize,
}

impl fmt::Display for CacheStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CacheStats {{ inodes: {}, positions: {} }}",
            self.inode_cache_size, self.position_cache_size
        )
    }
}

// ============================================================================
// POSITION RESOLUTION
// ============================================================================

/// Resolve a file path to its inode.
///
/// This function looks up the stable file identifier (inode) for a given
/// path in the repository tree.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `path` - The file path to resolve
///
/// # Returns
///
/// The inode for the file, or an error if the path is not found.
///
/// # Example
///
/// ```rust,ignore
/// let inode = resolve_path_to_inode(&mut ctx, "src/main.rs")?;
/// ```
pub fn resolve_path_to_inode<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    path: &str,
) -> GlobalizeResult<Inode>
where
    T: GraphTxnT + TreeTxnT,
{
    // Check cache first
    if let Some(&inode) = ctx.inode_cache.get(path) {
        return Ok(inode);
    }

    // Look up in tree
    let inode = ctx
        .txn
        .get_inode(path)?
        .ok_or_else(|| GlobalizeError::PathNotFound {
            path: path.to_string(),
        })?;

    // Cache the result
    ctx.inode_cache.insert(path.to_string(), inode);

    Ok(inode)
}

/// Resolve an inode to its graph position.
///
/// This function looks up the position in the repository graph where
/// a file's content root is located.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `inode` - The inode to resolve
///
/// # Returns
///
/// The graph position for the inode, or an error if not found.
///
/// # Example
///
/// ```rust,ignore
/// let position = resolve_inode_to_position(&mut ctx, inode)?;
/// ```
pub fn resolve_inode_to_position<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    inode: Inode,
) -> GlobalizeResult<Position<NodeId>>
where
    T: GraphTxnT + TreeTxnT,
{
    // Check cache first
    if let Some(&pos) = ctx.position_cache.get(&inode) {
        return Ok(pos);
    }

    // Look up in pristine
    let pos = ctx
        .txn
        .inode_position(inode)?
        .ok_or(GlobalizeError::InodeNotFound { inode })?;

    // Cache the result
    ctx.position_cache.insert(inode, pos);

    Ok(pos)
}

/// Resolve a file path to its graph position.
///
/// This is a convenience function that combines path-to-inode and
/// inode-to-position resolution.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `path` - The file path to resolve
///
/// # Returns
///
/// The graph position for the file, or an error if not found.
///
/// # Example
///
/// ```rust,ignore
/// let position = resolve_file_position(&mut ctx, "src/main.rs")?;
/// ```
pub fn resolve_file_position<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    path: &str,
) -> GlobalizeResult<Position<NodeId>>
where
    T: GraphTxnT + TreeTxnT,
{
    let inode = resolve_path_to_inode(ctx, path)?;
    resolve_inode_to_position(ctx, inode)
}

/// Resolve the parent directory's inode for a given path.
///
/// This is used when adding new files - we need to know the parent
/// directory to add the new filename entry.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `path` - The file path whose parent to find
///
/// # Returns
///
/// The inode of the parent directory, or an error if not found.
///
/// # Example
///
/// ```rust,ignore
/// // For path "src/lib/mod.rs", returns inode of "src/lib"
/// let parent_inode = resolve_parent_inode(&mut ctx, "src/lib/mod.rs")?;
/// ```
pub fn resolve_parent_inode<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    path: &str,
) -> GlobalizeResult<Inode>
where
    T: GraphTxnT + TreeTxnT,
{
    // Find the parent path
    let parent_path = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    if parent_path.is_empty() {
        // File is at repository root - use the root inode
        // The root directory has a special empty path
        ctx.txn
            .get_inode("")?
            .ok_or_else(|| GlobalizeError::ParentNotFound {
                path: path.to_string(),
            })
    } else {
        resolve_path_to_inode(ctx, &parent_path)
    }
}

// ============================================================================
// VERTEX CREATION
// ============================================================================

/// Create a Insertion for adding a filename to a parent directory.
///
/// When adding a new file, we first need to add its name as a span
/// in the parent directory's graph. This function creates that span.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `parent_inode` - The inode of the parent directory
/// * `filename` - The filename to add (just the name, not full path)
///
/// # Returns
///
/// A `Insertion` structure ready to be included in a graph_op.
///
/// # Example
///
/// ```rust,ignore
/// let name_vertex = create_name_vertex(&mut ctx, parent_inode, "new_file.rs")?;
/// ```
pub fn create_name_vertex<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    parent_inode: Inode,
    filename: &str,
) -> GlobalizeResult<Insertion<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    // Get the parent's graph position
    let parent_pos = resolve_inode_to_position(ctx, parent_inode)?;

    // Track dependency on the parent's change
    ctx.add_dependency_by_id(parent_pos.change)?;

    // Append the filename to content buffer
    let filename_bytes = filename.as_bytes();
    let (start, end) = ctx.append_content(filename_bytes);

    // The predecessors is the parent directory's position
    // For a directory entry, we use FOLDER flag
    Ok(Insertion {
        predecessors: vec![position_to_option_hash(parent_pos)],
        successors: vec![],
        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
        start,
        end,
        inode: position_to_option_hash(parent_pos),
    })
}

/// Create a Insertion for a file's inode entry.
///
/// Every file has an inode span that serves as the root of its content
/// graph. This function creates that span.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `name_pos` - Position of the filename span (from `create_name_vertex`)
///
/// # Returns
///
/// A `Insertion` structure for the inode.
///
/// # Note
///
/// The inode span is typically empty (start == end) as it just serves
/// as a reference point for the content graph.
///
/// # Example
///
/// ```rust,ignore
/// let inode_vertex = create_inode_vertex(&mut ctx, name_position)?;
/// ```
pub fn create_inode_vertex<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    name_pos: Position<Option<Hash>>,
) -> GlobalizeResult<Insertion<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    // Inode span has the name as its predecessors
    // It's an empty span (no content bytes)
    let pos = ChangePosition::new(ctx.content_len());

    Ok(Insertion {
        predecessors: vec![name_pos],
        successors: vec![],
        flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
        start: pos,
        end: pos, // Empty span
        inode: name_pos,
    })
}

/// Create a Insertion for file content.
///
/// This creates a span containing actual file content, with proper
/// up and down context for positioning in the graph.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `inode` - The file's inode
/// * `inode_pos` - The graph position of the file's inode
/// * `predecessors` - Positions that should come before this content
/// * `successors` - Positions that should come after this content
/// * `content` - The content bytes
///
/// # Returns
///
/// A `Insertion` structure for the content.
///
/// # Example
///
/// ```rust,ignore
/// let content_vertex = create_content_vertex(
///     &mut ctx,
///     inode,
///     inode_pos,
///     vec![up_pos],
///     vec![down_pos],
///     b"Hello, world!",
/// )?;
/// ```
pub fn create_content_vertex<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    _inode: Inode,
    inode_pos: Position<NodeId>,
    predecessors: Vec<Position<NodeId>>,
    successors: Vec<Position<NodeId>>,
    content: &[u8],
) -> GlobalizeResult<Insertion<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    // Track dependencies on context vertices
    for pos in &predecessors {
        ctx.add_dependency_by_id(pos.change)?;
    }
    for pos in &successors {
        ctx.add_dependency_by_id(pos.change)?;
    }

    // Append content to buffer
    let (start, end) = ctx.append_content(content);

    // Convert contexts to Option<Hash> positions, resolving external change hashes.
    // For predecessors and successors, we need to use the actual hash of the
    // change that introduced those vertices, not None (which means self-reference).
    // We pass None for current_change_id since we're creating a new change and
    // don't have its NodeId yet - any position not matching will be resolved.
    let up_ctx: Vec<Position<Option<Hash>>> = predecessors
        .into_iter()
        .map(|pos| position_to_option_hash_resolved(ctx.txn(), pos, None))
        .collect();
    let down_ctx: Vec<Position<Option<Hash>>> = successors
        .into_iter()
        .map(|pos| position_to_option_hash_resolved(ctx.txn(), pos, None))
        .collect();

    Ok(Insertion {
        predecessors: up_ctx,
        successors: down_ctx,
        flag: EdgeFlags::BLOCK,
        start,
        end,
        inode: position_to_option_hash(inode_pos),
    })
}

// ============================================================================
// EDGE CREATION (DELETIONS)
// ============================================================================

/// Create an EdgeUpdate for deleting content.
///
/// When content is deleted, we don't actually remove it from the graph.
/// Instead, we mark the edges leading to that content with the DELETED flag.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `inode` - The file's inode
/// * `inode_pos` - The graph position of the file's inode
/// * `deleted_vertices` - The vertices to mark as deleted
///
/// # Returns
///
/// An `EdgeUpdate` structure that marks the specified content as deleted.
///
/// # Example
///
/// ```rust,ignore
/// let deletion_edges = create_deletion_edges(
///     &mut ctx,
///     inode,
///     inode_pos,
///     deleted_vertices,
/// )?;
/// ```
pub fn create_deletion_edges<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    _inode: Inode,
    inode_pos: Position<NodeId>,
    deleted_vertices: Vec<GraphNode<NodeId>>,
) -> GlobalizeResult<EdgeUpdate<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    let mut edges = Vec::new();

    for deleted_node in deleted_vertices {
        // Track dependency on the change that introduced this span
        ctx.add_dependency_by_id(deleted_node.change)?;

        // Create a deletion edge
        // The edge goes from the start of the span to the span itself
        let edge = NewEdge {
            previous: EdgeFlags::BLOCK,
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            from: position_to_option_hash(deleted_node.start_pos()),
            to: vertex_to_option_hash(deleted_node),
            introduced_by: node_id_to_option_hash(deleted_node.change),
        };
        edges.push(edge);
    }

    Ok(EdgeUpdate {
        edges,
        inode: position_to_option_hash(inode_pos),
    })
}

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Convert a Position<NodeId> to Position<Option<Hash>>.
///
/// This converts from internal representation to the format used in
/// serializable hunks.
///
/// # Hash Semantics
///
/// - `None` means "this change" (self-reference) - the actual hash will be
///   filled in during serialization
/// - `Some(Hash::NONE)` means the ROOT position - the special virtual root
///   span that all top-level files reference
/// - `Some(hash)` means a reference to a specific existing change
///
/// # Arguments
///
/// * `pos` - The position with internal NodeId
///
/// # Returns
///
/// A position with Option<Hash>.
#[inline]
fn position_to_option_hash(pos: Position<NodeId>) -> Position<Option<Hash>> {
    Position {
        change: if pos.change.is_root() {
            // ROOT is the special virtual root span - use Hash::NONE
            Some(Hash::NONE)
        } else {
            // Non-root positions that we're creating are self-references
            // The actual hash will be filled in during serialization
            None
        },
        pos: pos.pos,
    }
}

/// Convert a Position<NodeId> to Position<Option<Hash>>, resolving external change hashes.
///
/// Unlike `position_to_option_hash`, this function looks up the actual hash for
/// external change references using the transaction. This is necessary when
/// creating predecessors or successors references that point to vertices in
/// previously applied changes.
///
/// # Hash Semantics
///
/// - `None` means "this change" (self-reference) - used for positions within the current change
/// - `Some(Hash::NONE)` means the ROOT span
/// - `Some(hash)` means a specific existing change
///
/// # Arguments
///
/// * `txn` - Transaction for looking up external hashes
/// * `pos` - The position with internal NodeId
/// * `current_change_id` - The NodeId of the change being created (if known), or None
///
/// # Returns
///
/// A position with Option<Hash> where external changes have their hashes resolved.
fn position_to_option_hash_resolved<T: GraphTxnT>(
    txn: &T,
    pos: Position<NodeId>,
    current_change_id: Option<NodeId>,
) -> Position<Option<Hash>> {
    Position {
        change: if pos.change.is_root() {
            // ROOT is the special virtual root span - use Hash::NONE
            Some(Hash::NONE)
        } else if current_change_id == Some(pos.change) {
            // Self-reference to the change being created
            None
        } else {
            // External change - look up its hash
            match txn.get_external(pos.change) {
                Ok(Some(hash)) => Some(hash),
                _ => {
                    // If we can't find the hash, treat as self-reference
                    // This shouldn't happen in normal operation
                    None
                }
            }
        },
        pos: pos.pos,
    }
}

/// Convert a GraphNode<NodeId> to GraphNode<Option<Hash>>.
///
/// Similar to position_to_option_hash, but for vertices.
///
/// # Hash Semantics
///
/// - `None` means "this change" (self-reference)
/// - `Some(Hash::NONE)` means the ROOT span
/// - `Some(hash)` means a specific existing change
#[inline]
fn vertex_to_option_hash(node: GraphNode<NodeId>) -> GraphNode<Option<Hash>> {
    GraphNode {
        change: if node.change.is_root() {
            // ROOT span - use Hash::NONE
            Some(Hash::NONE)
        } else {
            // Self-reference - hash filled in during serialization
            None
        },
        start: node.start,
        end: node.end,
    }
}

/// Convert a NodeId to Option<Hash>.
///
/// # Hash Semantics
///
/// - Returns `Some(Hash::NONE)` for the ROOT node
/// - Returns `None` for non-root nodes (self-references, hash filled in during serialization)
#[inline]
fn node_id_to_option_hash(node_id: NodeId) -> Option<Hash> {
    if node_id.is_root() {
        // ROOT node - use Hash::NONE
        Some(Hash::NONE)
    } else {
        // Self-reference - hash will be filled in during serialization
        None
    }
}

/// Extract the filename from a path.
///
/// # Arguments
///
/// * `path` - The full file path
///
/// # Returns
///
/// The filename portion of the path, or the full path if no separator found.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::globalize::extract_filename;
///
/// assert_eq!(extract_filename("src/lib/mod.rs"), "mod.rs");
/// assert_eq!(extract_filename("Cargo.toml"), "Cargo.toml");
/// ```
#[must_use]
pub fn extract_filename(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
}

/// Extract the parent directory from a path.
///
/// # Arguments
///
/// * `path` - The full file path
///
/// # Returns
///
/// The parent directory, or empty string for root-level files.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::globalize::extract_parent;
///
/// assert_eq!(extract_parent("src/lib/mod.rs"), "src/lib");
/// assert_eq!(extract_parent("Cargo.toml"), "");
/// ```
#[must_use]
pub fn extract_parent(path: &str) -> &str {
    std::path::Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("")
}

// ============================================================================
// GLOBALIZATION RESULT
// ============================================================================

/// Result of globalizing a single file.
///
/// Contains the generated hunks and metadata about the globalization process.
#[derive(Debug, Clone)]
pub struct GlobalizedFile {
    /// The file path.
    path: String,

    /// The generated hunks.
    hunks: Vec<GraphOp<Option<Hash>>>,

    /// Number of content bytes added.
    bytes_added: u64,

    /// Number of dependencies tracked.
    dependency_count: usize,

    /// Enriched CRDT file operations with graph positions.
    ///
    /// After globalization, this contains the FileOps with `content_range`
    /// fields populated, linking CRDT branches to graph vertex positions.
    file_ops: Option<crate::change::FileOps>,
}

impl GlobalizedFile {
    /// Create a new globalized file result.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            hunks: Vec::new(),
            bytes_added: 0,
            dependency_count: 0,
            file_ops: None,
        }
    }

    /// Add a graph_op to the result.
    pub fn add_hunk(&mut self, graph_op: GraphOp<Option<Hash>>) {
        self.hunks.push(graph_op);
    }

    /// Set bytes added.
    pub fn set_bytes_added(&mut self, bytes: u64) {
        self.bytes_added = bytes;
    }

    /// Set dependency count.
    pub fn set_dependency_count(&mut self, count: usize) {
        self.dependency_count = count;
    }

    /// Get the file path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get the hunks.
    #[must_use]
    pub fn hunks(&self) -> &[GraphOp<Option<Hash>>] {
        &self.hunks
    }

    /// Get bytes added.
    #[must_use]
    pub fn bytes_added(&self) -> u64 {
        self.bytes_added
    }

    /// Get dependency count.
    #[must_use]
    pub fn dependency_count(&self) -> usize {
        self.dependency_count
    }

    /// Check if empty (no hunks).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// Get number of hunks.
    #[must_use]
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Take ownership of the hunks.
    #[must_use]
    pub fn into_hunks(self) -> Vec<GraphOp<Option<Hash>>> {
        self.hunks
    }

    /// Set the enriched CRDT file operations.
    ///
    /// This is called during globalization after the FileOps have been
    /// enriched with graph position information.
    pub fn set_file_ops(&mut self, file_ops: crate::change::FileOps) {
        self.file_ops = Some(file_ops);
    }

    /// Get the enriched CRDT file operations.
    #[must_use]
    pub fn file_ops(&self) -> Option<&crate::change::FileOps> {
        self.file_ops.as_ref()
    }

    /// Take ownership of the enriched CRDT file operations.
    #[must_use]
    pub fn into_file_ops(self) -> Option<crate::change::FileOps> {
        self.file_ops
    }

    /// Take ownership of both hunks and file_ops.
    #[must_use]
    pub fn into_parts(self) -> (Vec<GraphOp<Option<Hash>>>, Option<crate::change::FileOps>) {
        (self.hunks, self.file_ops)
    }

    /// Check if this file has enriched CRDT operations.
    #[must_use]
    pub fn has_file_ops(&self) -> bool {
        self.file_ops.is_some()
    }
}

// ============================================================================
// MAIN GLOBALIZATION FUNCTIONS
// ============================================================================

/// Globalize a single built graph_op into a graph graph_op.
///
/// This is the core function that converts a local working copy change
/// (represented as a `BuiltHunk`) into a graph-compatible `GraphOp<Option<Hash>>`.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `built` - The built graph_op from the recording phase
/// * `inode` - The file's inode
/// * `inode_pos` - The graph position of the file's inode
/// * `content` - The content slice for this graph_op
/// * `full_content` - The full file content (needed for NeedsReplace case)
/// * `old_line_count` - Number of lines in the old content (for precise insert detection)
///
/// # Returns
///
/// A graph-compatible graph_op, or an error if globalization fails.
///
/// # Example
///
/// ```rust,ignore
/// let graph_op = globalize_hunk(&mut ctx, &built_hunk, inode, inode_pos, content)?;
/// change.add_hunk(graph_op);
/// ```
pub fn globalize_hunk<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    built: &BuiltHunk,
    inode: Inode,
    inode_pos: Position<NodeId>,
    content: &[u8],
    full_content: &[u8],
    old_line_count: Option<usize>,
) -> GlobalizeResult<GraphOp<Option<Hash>>>
where
    T: GraphTxnT + TreeTxnT,
{
    let local = built.local.clone();
    let encoding = built.encoding;

    // For modifications to existing files, we need to find the correct context
    // positions from the content graph. The predecessors should be the END of the
    // span that comes before, and successors should be the START of the
    // span that comes after.
    //
    // We use the line number information (old_start) to determine insertion position:
    // - old_start == 0: Prepend (insert at beginning)
    // - Otherwise: Check if we can insert in middle, or need to do a Replace
    //
    // The challenge is that without byte-to-span mapping (like original Atomic has),
    // we can't reliably insert in the middle of a single span. So for middle insertions,
    // we convert to Replace (delete all old content, insert all new content).

    match built.kind {
        BuiltHunkKind::Insert => {
            // Pure insertion - create a Insertion
            // Determine predecessors and successors based on insertion position
            // old_start tells us which line in the old content the insertion comes AFTER
            let insert_result =
                find_insert_context(ctx.txn(), inode_pos, built.old_start, old_line_count)?;

            match insert_result {
                InsertContext::Prepend { successors } => {
                    // Prepend: connect to inode, with successors to first content
                    let content_node = create_content_vertex(
                        ctx,
                        inode,
                        inode_pos,
                        vec![inode_pos],
                        successors,
                        content,
                    )?;

                    Ok(GraphOp::Edit {
                        change: Atom::Insertion(content_node),
                        local,
                        encoding,
                    })
                }
                InsertContext::Append { predecessors } => {
                    // Append: connect after existing content
                    let content_node = create_content_vertex(
                        ctx,
                        inode,
                        inode_pos,
                        predecessors,
                        vec![],
                        content,
                    )?;

                    Ok(GraphOp::Edit {
                        change: Atom::Insertion(content_node),
                        local,
                        encoding,
                    })
                }
                InsertContext::NeedsReplace => {
                    // Middle insertion into a single span - we can't split it without
                    // byte-to-span mapping. Convert this to a Replace operation:
                    // 1. Delete all existing content
                    // 2. Insert new content connected to the inode
                    //
                    // This is semantically correct: we're replacing the file content.
                    let content_vertices = find_content_vertices(ctx.txn(), inode_pos)?;
                    let deletion_edges =
                        create_deletion_edges_for_vertices(ctx, &content_vertices)?;

                    let deletion = EdgeUpdate {
                        edges: deletion_edges,
                        inode: position_to_option_hash(inode_pos),
                    };

                    // Insert the FULL file content connected to the inode (after deletion)
                    // We use full_content because this is a complete file replacement
                    let insertion = create_content_vertex(
                        ctx,
                        inode,
                        inode_pos,
                        vec![inode_pos], // predecessors: the inode span itself
                        vec![],          // successors: nothing after
                        full_content,
                    )?;

                    Ok(GraphOp::Replacement {
                        change: deletion,
                        replacement: insertion,
                        local,
                        encoding,
                    })
                }
            }
        }

        BuiltHunkKind::Delete => {
            // Pure deletion - create an EdgeUpdate
            // Find all content vertices for this file and mark them as deleted
            let content_vertices = find_content_vertices(ctx.txn(), inode_pos)?;
            let deletion_edges = create_deletion_edges_for_vertices(ctx, &content_vertices)?;

            let edge_update = EdgeUpdate {
                edges: deletion_edges,
                inode: position_to_option_hash(inode_pos),
            };

            Ok(GraphOp::Edit {
                change: Atom::EdgeUpdate(edge_update),
                local,
                encoding,
            })
        }

        BuiltHunkKind::Replace => {
            // Replacement - delete old content, insert new
            // For a replacement, we need to:
            // 1. Find and delete the old content vertices
            // 2. Insert the FULL new file content connected to the inode span
            //
            // IMPORTANT: We must use `full_content` (the entire new file), not `content`
            // (just the replacement portion). This is because we're deleting ALL old
            // content vertices, so we need to replace with ALL new content.
            //
            // Bug fix: Previously this used `content` which only contained the changed
            // lines, causing data loss of unchanged lines.
            let content_vertices = find_content_vertices(ctx.txn(), inode_pos)?;
            let deletion_edges = create_deletion_edges_for_vertices(ctx, &content_vertices)?;

            let deletion = EdgeUpdate {
                edges: deletion_edges,
                inode: position_to_option_hash(inode_pos),
            };

            // For replacement, insert the FULL new file content connected to the inode
            // (not just the graph_op content, since we're deleting ALL old content)
            let insertion = create_content_vertex(
                ctx,
                inode,
                inode_pos,
                vec![inode_pos], // predecessors: the inode span itself
                vec![],          // successors: nothing after
                full_content,    // Use full file content, not just the graph_op portion
            )?;

            Ok(GraphOp::Replacement {
                change: deletion,
                replacement: insertion,
                local,
                encoding,
            })
        }
    }
}

/// Find the end position of the last content span in a file.
///
/// This traverses the file's content graph to find the span that represents
/// the end of the current content. This position is used as predecessors when
/// appending new content.
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `inode_pos` - The graph position of the file's inode
///
/// # Returns
///
/// The end position of the last content span, or the inode position if
/// the file has no content.

/// Result of finding insert context - determines how to handle the insertion.
#[derive(Debug)]
enum InsertContext {
    /// Prepend: insert at the very beginning of the file.
    /// predecessors should be the inode, successors is the start of first content.
    Prepend { successors: Vec<Position<NodeId>> },
    /// Append: insert at the end of the file.
    /// predecessors is the end of last content, successors is empty.
    Append { predecessors: Vec<Position<NodeId>> },
    /// Middle insertion that requires a Replace operation.
    /// This happens when we need to insert within a single span but don't have
    /// byte-to-span mapping to find the exact position.
    NeedsReplace,
}

/// Find the appropriate context for an insertion based on old_start line number.
///
/// This determines where new content should be inserted:
/// - old_start == 0: Prepend (insert before all existing content)
/// - old_start >= total_lines: Append (insert after all existing content)
/// - Otherwise: Middle insertion (needs Replace because we can't split vertices)
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `inode_pos` - The graph position of the file's inode
/// * `old_start` - The line number in old content AFTER which to insert (0 = prepend)
///
/// # Returns
///
/// An `InsertContext` indicating how to handle the insertion.
fn find_insert_context<T>(
    txn: &T,
    inode_pos: Position<NodeId>,
    old_start: usize,
    old_line_count: Option<usize>,
) -> GlobalizeResult<InsertContext>
where
    T: GraphTxnT,
{
    // Retrieve the file's content graph
    let options = RetrieveOptions::default();
    let result = match retrieve_graph(txn, inode_pos, options) {
        Ok(r) => r,
        Err(_) => {
            // Empty file - treat as append (will connect to inode)
            return Ok(InsertContext::Append {
                predecessors: vec![inode_pos],
            });
        }
    };

    // Collect content vertices with their positions
    let mut content_vertices: Vec<(GraphNode<NodeId>, Position<NodeId>, Position<NodeId>)> =
        Vec::new();

    for vertex_id in 0..result.graph.len_vertices() {
        if let Some(alive_vertex) = result.graph.try_get_vertex(vertex_id.into()) {
            let alive_node = alive_vertex.node;

            // Skip DUMMY and empty vertices
            if alive_node.change.is_root() || alive_node.start == alive_node.end {
                continue;
            }

            let start_pos = Position::new(alive_node.change, alive_node.start);
            let end_pos = Position::new(alive_node.change, alive_node.end);
            content_vertices.push((alive_node, start_pos, end_pos));
        }
    }

    // If no content vertices, this is an empty file - append
    if content_vertices.is_empty() {
        return Ok(InsertContext::Append {
            predecessors: vec![inode_pos],
        });
    }

    // Sort by start position to get proper ordering
    content_vertices.sort_by(|a, b| a.1.pos.cmp(&b.1.pos));

    // old_start == 0 means prepend (insert BEFORE line 0, i.e., at the very beginning)
    if old_start == 0 {
        let first_start = content_vertices[0].1;
        return Ok(InsertContext::Prepend {
            successors: vec![first_start],
        });
    }

    // Determine if this is an append or middle insertion using the actual old line count.
    //
    // old_start indicates which line of OLD content the insertion comes AFTER.
    // If old_start >= total_old_lines, it's an append (insert at end).
    // If old_start < total_old_lines and we have a single span, we need Replace
    // because we can't split a span without byte-to-span mapping.

    // Use the actual old line count if available, otherwise fall back to span count
    let total_old_lines = old_line_count.unwrap_or(content_vertices.len());

    // If old_start >= total lines, it's an append
    if old_start >= total_old_lines {
        let last_end = content_vertices.last().unwrap().2;
        return Ok(InsertContext::Append {
            predecessors: vec![last_end],
        });
    }

    // For single span or middle insertion into multiple vertices,
    // we need byte-level mapping to split correctly.
    // Without it, signal that a Replace is needed.
    Ok(InsertContext::NeedsReplace)
}

#[allow(dead_code)]
fn find_content_end_position<T>(
    txn: &T,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Position<NodeId>>
where
    T: GraphTxnT,
{
    // Retrieve the file's content graph starting from the inode position
    let options = RetrieveOptions::default();
    let result = match retrieve_graph(txn, inode_pos, options) {
        Ok(r) => r,
        Err(_) => {
            // If we can't retrieve the graph, fall back to inode position
            // This can happen for empty files or files with no content yet
            return Ok(inode_pos);
        }
    };

    // Find the span with the highest end position
    // This is the "last" content in the file
    //
    // We track content vertices separately from the inode because inode positions
    // may be in a reserved high range (for CRDT compatibility) that would make
    // simple comparisons fail. We want the content span with the highest end
    // position in the normal content range.
    let mut max_content_end: Option<Position<NodeId>> = None;

    for vertex_id in 0..result.graph.len_vertices() {
        if let Some(alive_vertex) = result.graph.try_get_vertex(vertex_id.into()) {
            let alive_node = &alive_vertex.node;

            // Skip the DUMMY span (NodeId(0) / ROOT)
            if alive_node.change.is_root() {
                continue;
            }

            // Skip empty vertices (like inode markers)
            // Content vertices always have start < end
            if alive_node.start == alive_node.end {
                continue;
            }

            // This is a content span - track the one with the highest end position
            let end_pos = Position::new(alive_node.change, alive_node.end);

            match &max_content_end {
                None => {
                    // First content span found
                    max_content_end = Some(end_pos);
                }
                Some(current_max) => {
                    // Compare by position value - we want the highest end position
                    if end_pos.pos > current_max.pos {
                        max_content_end = Some(end_pos);
                    }
                }
            }
        }
    }

    // Return the highest content end position, or fall back to inode position
    // if no content was found (empty file)
    Ok(max_content_end.unwrap_or(inode_pos))
}

/// Find all content vertices for a file.
///
/// This retrieves the file's graph and returns all non-inode content vertices.
/// Used for deletion operations where we need to mark existing content as deleted.
///
/// # Arguments
///
/// * `txn` - Transaction for graph lookups
/// * `inode_pos` - The graph position of the file's inode
///
/// # Returns
///
/// A vector of content vertices (excluding the inode span and DUMMY).
fn find_content_vertices<T>(
    txn: &T,
    inode_pos: Position<NodeId>,
) -> GlobalizeResult<Vec<GraphNode<NodeId>>>
where
    T: GraphTxnT,
{
    use crate::output::alive::{retrieve_graph, RetrieveOptions};
    

    let options = RetrieveOptions::default();
    let result = match retrieve_graph(txn, inode_pos, options) {
        Ok(r) => r,
        Err(_) => {
            // No graph content - return empty
            return Ok(Vec::new());
        }
    };

    let mut vertices = Vec::new();

    for vertex_id in 0..result.graph.len_vertices() {
        if let Some(alive_vertex) = result.graph.try_get_vertex(vertex_id.into()) {
            let alive_node = alive_vertex.node;

            // Skip DUMMY/ROOT span
            if alive_node.change.is_root() {
                continue;
            }

            // Skip the inode span (empty span at inode position)
            if alive_node.start == alive_node.end && alive_node.start == inode_pos.pos {
                continue;
            }

            // This is a content span
            vertices.push(alive_node);
        }
    }

    Ok(vertices)
}

/// Create deletion edges for a list of content vertices.
///
/// For each span, creates a NewEdge that marks the edge TO that span as deleted.
/// The edge goes from the predecessor's end position to the span being deleted.
///
/// # Arguments
///
/// * `ctx` - The globalization context (for tracking dependencies)
/// * `inode_pos` - The inode position (used to find predecessor edges)
/// * `vertices` - The vertices to mark as deleted
///
/// # Returns
///
/// A vector of NewEdge structures for the deletion.
fn create_deletion_edges_for_vertices<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    vertices: &[GraphNode<NodeId>],
) -> GlobalizeResult<Vec<NewEdge<Option<Hash>>>>
where
    T: GraphTxnT + TreeTxnT,
{
    use crate::change::NewEdge;
    use crate::types::EdgeFlags;

    let mut edges = Vec::new();

    for v in vertices {
        // Track dependency on the change that introduced this span
        ctx.add_dependency_by_id(v.change)?;

        // Find the predecessor of this span by looking for PARENT edges
        // The deletion edge should go from the predecessor's end to this span
        //
        // For a content span that's a child of the inode, the predecessor
        // is the inode span itself. We look up the parent edge to find
        // the source position.
        let from_pos = find_predecessor_end_position(ctx.txn(), *v)?;

        // Create a deletion edge
        // This marks the edge FROM the predecessor TO this span as deleted
        let edge = NewEdge {
            // The previous edge type (what we expect the existing edge to have)
            previous: EdgeFlags::BLOCK,
            // The new edge type (add DELETED flag)
            flag: EdgeFlags::BLOCK | EdgeFlags::DELETED,
            // From: the end position of the predecessor span
            from: position_to_option_hash_resolved(ctx.txn(), from_pos, None),
            // To: the span being deleted
            to: vertex_to_option_hash_resolved(ctx.txn(), *v, None),
            // Introduced by: the change that originally created the edge
            // We look this up from the graph
            introduced_by: find_edge_introduced_by(ctx.txn(), from_pos, *v),
        };

        edges.push(edge);
    }

    Ok(edges)
}

/// Find the end position of the predecessor of a span.
///
/// This looks up the PARENT edges of the span to find which span
/// comes before it, then returns the end position of that predecessor.
fn find_predecessor_end_position<T: GraphTxnT>(
    txn: &T,
    node: GraphNode<NodeId>,
) -> GlobalizeResult<Position<NodeId>> {
    use crate::types::EdgeFlags;

    // Look for BLOCK|PARENT edges - these tell us where the edge came from
    let min_flag = EdgeFlags::BLOCK | EdgeFlags::PARENT;
    let max_flag = EdgeFlags::BLOCK | EdgeFlags::PARENT | EdgeFlags::FOLDER;

    let adj = txn
        .iter_adjacent(node, min_flag, max_flag)
        .map_err(GlobalizeError::Pristine)?;

    for edge_result in adj {
        let edge = edge_result.map_err(GlobalizeError::Pristine)?;

        // The edge dest() points to where the forward edge came FROM
        // (remember, this is a reverse/PARENT edge)
        return Ok(edge.dest());
    }

    // If no parent found, this shouldn't happen for content vertices
    // Use NodeNotFound as the closest matching error type
    Err(GlobalizeError::NodeNotFound {
        position: node.start_pos(),
    })
}

/// Find the change that introduced an edge between two positions.
fn find_edge_introduced_by<T: GraphTxnT>(
    txn: &T,
    from_pos: Position<NodeId>,
    to_vertex: GraphNode<NodeId>,
) -> Option<Hash> {
    use crate::types::EdgeFlags;

    // Find the span at the from position
    let from_vertex = match txn.find_block_end(from_pos) {
        Ok(v) => v,
        Err(_) => return None,
    };

    // Look for the edge from from_vertex to to_vertex
    let min_flag = EdgeFlags::BLOCK;
    let max_flag = EdgeFlags::BLOCK | EdgeFlags::FOLDER;

    let adj = match txn.iter_adjacent(from_vertex, min_flag, max_flag) {
        Ok(a) => a,
        Err(_) => return None,
    };

    for edge_result in adj {
        let edge = match edge_result {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Check if this edge points to our target span
        if edge.dest() == to_vertex.start_pos() {
            // Get the hash for the introduced_by NodeId
            let introduced_by_id = edge.introduced_by();
            return txn.get_external(introduced_by_id).ok().flatten();
        }
    }

    None
}

/// Convert a GraphNode<NodeId> to GraphNode<Option<Hash>>, resolving external change hashes.
fn vertex_to_option_hash_resolved<T: GraphTxnT>(
    txn: &T,
    node: GraphNode<NodeId>,
    current_change_id: Option<NodeId>,
) -> GraphNode<Option<Hash>> {
    let change = if node.change.is_root() {
        Some(Hash::NONE)
    } else if current_change_id == Some(node.change) {
        None
    } else {
        match txn.get_external(node.change) {
            Ok(Some(hash)) => Some(hash),
            _ => None,
        }
    };

    GraphNode {
        change,
        start: node.start,
        end: node.end,
    }
}

/// Globalize all hunks in a recorded file.
///
/// This processes all hunks in a `RecordedFile` and converts them to
/// graph-compatible hunks.
///
/// # Arguments
///
/// * `ctx` - The globalization context
/// * `recorded` - The recorded file with built hunks
/// * `options` - Globalization options
///
/// # Returns
///
/// A `GlobalizedFile` containing all the converted hunks.
///
/// # Example
///
/// ```rust,ignore
/// let globalized = globalize_recorded_file(&mut ctx, &recorded_file, &options)?;
/// for graph_op in globalized.hunks() {
///     change.add_hunk(graph_op.clone());
/// }
/// ```
pub fn globalize_recorded_file<T>(
    ctx: &mut GlobalizeContext<'_, T>,
    recorded: &RecordedFile,
    options: &GlobalizeOptions,
) -> GlobalizeResult<GlobalizedFile>
where
    T: GraphTxnT + TreeTxnT,
{
    use crate::change::{GraphOp, Insertion};
    use crate::types::{ChangePosition, EdgeFlags, Position};

    let path = recorded.path();
    let mut result = GlobalizedFile::new(path);

    // Handle directory additions (DirAdd)
    if recorded.is_directory() {
        // Create a DirAdd graph_op for an explicitly tracked directory
        // The directory has no content, just name and inode vertices

        let parent_context_pos: Position<Option<Hash>> = {
            let parent_path = extract_parent(path);
            if parent_path.is_empty() {
                // Top-level directory - parent is ROOT
                Position {
                    change: Some(Hash::NONE),
                    pos: ChangePosition::ROOT,
                }
            } else {
                // Nested directory - for now use ROOT
                Position {
                    change: Some(Hash::NONE),
                    pos: ChangePosition::ROOT,
                }
            }
        };

        // Add the directory name to the content buffer
        let dirname = extract_filename(path);
        let dirname_bytes = dirname.as_bytes();
        let (name_start, name_end) = ctx.append_content(dirname_bytes);

        // The inode span is empty (marks the directory's root)
        let inode_start = name_end;
        let inode_end = inode_start;

        // Create name span with FOLDER flag
        let add_name = Insertion {
            predecessors: vec![parent_context_pos],
            successors: vec![],
            flag: EdgeFlags::FOLDER, // FOLDER flag for directory entry
            start: name_start,
            end: name_end,
            inode: Position {
                change: None, // Self-reference (current change)
                pos: inode_start,
            },
        };

        // Create inode span (empty)
        let add_inode = Insertion {
            predecessors: vec![Position {
                change: None,
                pos: name_end,
            }],
            successors: vec![],
            flag: EdgeFlags::FOLDER,
            start: inode_start,
            end: inode_end,
            inode: Position {
                change: None,
                pos: inode_start,
            },
        };

        let graph_op: GraphOp<Option<Hash>> = GraphOp::DirAdd {
            add_name,
            add_inode,
            path: path.to_string(),
        };

        result.add_hunk(graph_op);
        result.set_bytes_added(dirname_bytes.len() as u64);
        return Ok(result);
    }

    // Handle directory deletions (DirDel)
    if recorded.is_deleted_directory() {
        // For directory deletion, we need to create an EdgeUpdate to mark the
        // directory's edges as deleted. This requires the directory's inode
        // and position in the graph.
        //
        // If we have position info, create a proper DirDel graph_op.
        // Otherwise, the directory is already untracked from the TREE table
        // during the record process, so we can skip the graph_op.

        if let (Some(_inode), Some(position)) = (recorded.inode(), recorded.position()) {
            // Convert NodeId to Option<Hash> for the graph_op
            // We need to look up the external hash for this change
            let change_hash: Option<Hash> = ctx.get_external(position.change);

            // Create EdgeUpdate to mark directory edges as deleted
            let del = EdgeUpdate {
                edges: vec![NewEdge {
                    previous: EdgeFlags::FOLDER,
                    flag: EdgeFlags::FOLDER | EdgeFlags::DELETED,
                    from: Position {
                        change: change_hash,
                        pos: position.pos,
                    },
                    to: GraphNode {
                        change: change_hash,
                        start: position.pos,
                        end: position.pos, // Empty span for directory inode
                    },
                    introduced_by: change_hash,
                }],
                inode: Position {
                    change: change_hash,
                    pos: position.pos,
                },
            };

            let graph_op: GraphOp<Option<Hash>> = GraphOp::DirDel {
                del,
                path: path.to_string(),
            };

            result.add_hunk(graph_op);
            // Note: edges_added tracking not implemented in GlobalizedFile
            // The graph_op count serves as a proxy for tracking edge modifications
        }
        // If no position info, the directory was never recorded to the graph,
        // so there's nothing to delete. The tracking removal is sufficient.

        return Ok(result);
    }

    // Check for empty file
    if recorded.is_empty() && !options.get_include_empty_files() {
        return Ok(result);
    }

    let content = recorded.content();
    let initial_deps = ctx.dependencies().len();
    let initial_content_len = ctx.content_len();

    // Check if this is a newly added file (FileAdd) or a modification
    if let Some(inode) = recorded.inode() {
        // Existing file - needs position for modification
        let inode_pos = recorded
            .position()
            .ok_or_else(|| GlobalizeError::MissingField {
                path: path.to_string(),
                field: "position",
            })?;

        // Track content positions for each hunk to enrich FileOps later
        let mut hunk_content_ranges: Vec<HunkContentRange> = Vec::new();

        // Process each graph_op for modification
        for built in recorded.hunks() {
            // Get the content slice for this graph_op
            let hunk_content =
                if let (Some(start), Some(end)) = (built.content_start, built.content_end) {
                    let start = start as usize;
                    let end = end as usize;
                    if end <= content.len() {
                        &content[start..end]
                    } else {
                        &[]
                    }
                } else {
                    &[]
                };

            // Track content position before globalization
            let content_pos_before = ctx.content_len();

            let graph_op = globalize_hunk(
                ctx,
                built,
                inode,
                inode_pos,
                hunk_content,
                content,
                recorded.old_line_count(),
            )?;

            // Track content position after globalization
            let content_pos_after = ctx.content_len();

            // Record the content range for this hunk
            if content_pos_after > content_pos_before {
                hunk_content_ranges.push(HunkContentRange {
                    kind: built.kind,
                    new_start: built.new_start,
                    new_len: built.new_len,
                    content_start: ChangePosition::new(content_pos_before as u64),
                    content_end: ChangePosition::new(content_pos_after as u64),
                    // For Replace hunks, we use full_content, so track that
                    uses_full_content: matches!(
                        built.kind,
                        super::graph_op::BuiltHunkKind::Replace
                    ) || matches!(
                        built.kind,
                        super::graph_op::BuiltHunkKind::Insert
                    ),
                });
            }

            result.add_hunk(graph_op);
        }

        // Enrich FileOps with content ranges for Edit hunks
        if let Some(mut file_ops) = recorded.crdt_ops().cloned() {
            enrich_file_ops_for_edit(&mut file_ops, content, &hunk_content_ranges);
            result.set_file_ops(file_ops);
        }
    } else {
        // Newly added file (FileAdd) - no existing inode/position
        // We need to create a FileAdd graph_op that:
        // 1. Creates the file entry in the parent directory (or root)
        // 2. Contains the file content
        //
        // The FileAdd graph_op structure:
        // - add_name: Span for the filename, connected to parent directory
        // - add_inode: Span for the file's inode (root of file content graph)
        // - contents: Span containing the actual file content

        // Determine the parent context position.
        // For top-level files (no directory prefix), we use ROOT.
        // For nested files, we would resolve the parent directory's position.
        //
        // The ROOT position is represented as:
        // Position { change: Some(Hash::NONE), pos: ChangePosition::ROOT }
        //
        // This is the virtual root span that all top-level files reference.
        let parent_context_pos: Position<Option<Hash>> = {
            let parent_path = extract_parent(path);
            if parent_path.is_empty() {
                // Top-level file - parent is ROOT
                Position {
                    change: Some(Hash::NONE), // Hash::NONE indicates ROOT
                    pos: ChangePosition::ROOT,
                }
            } else {
                // Nested file - try to resolve parent directory position
                // For now, use ROOT as we don't have nested directory support yet
                // In a full implementation, we would resolve the parent directory's
                // inode and get its graph position
                Position {
                    change: Some(Hash::NONE),
                    pos: ChangePosition::ROOT,
                }
            }
        };

        // Add the filename to the content buffer
        let filename = extract_filename(path);
        let filename_bytes = filename.as_bytes();
        let (name_start, name_end) = ctx.append_content(filename_bytes);

        // The inode span is empty (marks the file's root in the graph)
        let inode_start = name_end;
        let inode_end = name_end;

        // Position referencing the END of the name span we're creating (self-reference).
        // Up-context positions must reference the END of the predecessor vertex so that
        // find_block_end() correctly resolves to this name vertex V[name_start:name_end].
        // Using name_start here would cause find_block_end(name_start) to find whatever
        // vertex ENDS at that position (e.g., the previous file's content vertex),
        // creating cross-file edges that contaminate graph traversal.
        // None means "this change" - the actual hash is filled in during serialization.
        let name_pos: Position<Option<Hash>> = Position {
            change: None, // Self-reference to this change
            pos: name_end,
        };

        // Position referencing the inode span we're creating (self-reference)
        let inode_pos: Position<Option<Hash>> = Position {
            change: None, // Self-reference to this change
            pos: inode_start,
        };

        if !content.is_empty() {
            // Add file content to the context buffer
            let (content_start, content_end) = ctx.append_content(content);

            let encoding = recorded.encoding();

            // Enrich FileOps with the content range if available
            // This links the CRDT branches to their graph vertex positions
            if let Some(mut file_ops) = recorded.crdt_ops().cloned() {
                // For a FileAdd, all line content is in the single content span
                // We need to compute per-line ranges within the content
                enrich_file_ops_for_add(&mut file_ops, content, content_start);
                result.set_file_ops(file_ops);
            }

            let graph_op = GraphOp::FileAdd {
                add_name: Insertion {
                    // Parent context - ROOT for top-level files
                    predecessors: vec![parent_context_pos.clone()],
                    successors: vec![],
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                    start: name_start,
                    end: name_end,
                    // The inode field for add_name points to the parent's position
                    inode: parent_context_pos,
                },
                add_inode: Insertion {
                    // The inode span's parent is the name span
                    predecessors: vec![name_pos],
                    successors: vec![],
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                    start: inode_start,
                    end: inode_end,
                    // The inode field points to itself (this is the file's root)
                    inode: inode_pos.clone(),
                },
                contents: Some(Insertion {
                    // Content's parent is the inode span
                    predecessors: vec![inode_pos.clone()],
                    successors: vec![],
                    flag: EdgeFlags::BLOCK,
                    start: content_start,
                    end: content_end,
                    // Content belongs to this file (referenced by inode)
                    inode: inode_pos,
                }),
                path: path.to_string(),
                encoding,
            };

            result.add_hunk(graph_op);
        } else if options.get_include_empty_files() {
            // Empty file - still create the FileAdd but with no content span
            let graph_op = GraphOp::FileAdd {
                add_name: Insertion {
                    predecessors: vec![parent_context_pos.clone()],
                    successors: vec![],
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                    start: name_start,
                    end: name_end,
                    inode: parent_context_pos,
                },
                add_inode: Insertion {
                    predecessors: vec![name_pos],
                    successors: vec![],
                    flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                    start: inode_start,
                    end: inode_end,
                    inode: inode_pos.clone(),
                },
                contents: None,
                path: path.to_string(),
                encoding: recorded.encoding(),
            };

            result.add_hunk(graph_op);
        }
    }

    // Note: FileOps enrichment for modifications is now handled above
    // in the inode branch after processing all hunks

    // Update statistics
    result.set_bytes_added(ctx.content_len() - initial_content_len);
    result.set_dependency_count(ctx.dependencies().len() - initial_deps);

    Ok(result)
}

/// Enrich FileOps with content ranges for a FileAdd operation.
///
/// For new files, the content is laid out sequentially in the change buffer.
/// This function computes the byte range for each line within the content
/// and stores it in the LineOps for later use in populating BRANCH_VERTEX.
fn enrich_file_ops_for_add(
    file_ops: &mut crate::change::FileOps,
    content: &[u8],
    content_start: ChangePosition,
) {
    use crate::types::ChangePosition;

    // Split content into lines to compute per-line ranges
    let mut line_start = 0usize;
    let mut line_idx = 0usize;

    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' {
            // End of line (including the newline)
            let line_end = i + 1;

            // Find the corresponding LineOps entry by line number
            if let Some(line_ops) = file_ops
                .line_ops_mut()
                .iter_mut()
                .find(|ops| ops.new_line_num() == Some(line_idx + 1))
            {
                // Compute the absolute positions in the change content buffer
                let abs_start = ChangePosition::new(content_start.get() + line_start as u64);
                let abs_end = ChangePosition::new(content_start.get() + line_end as u64);
                line_ops.set_content_range(abs_start, abs_end);
            }

            line_start = line_end;
            line_idx += 1;
        }
    }

    // Handle last line if it doesn't end with newline
    if line_start < content.len() {
        if let Some(line_ops) = file_ops
            .line_ops_mut()
            .iter_mut()
            .find(|ops| ops.new_line_num() == Some(line_idx + 1))
        {
            let abs_start = ChangePosition::new(content_start.get() + line_start as u64);
            let abs_end = ChangePosition::new(content_start.get() + content.len() as u64);
            line_ops.set_content_range(abs_start, abs_end);
        }
    }
}

/// Tracks content range information for a globalized hunk.
///
/// Used to correlate hunks with LineOps during Edit enrichment.
#[allow(dead_code)]
#[derive(Debug)]
struct HunkContentRange {
    /// The kind of hunk (Insert, Delete, Replace).
    kind: super::graph_op::BuiltHunkKind,
    /// Starting line number in new content (0-indexed).
    new_start: usize,
    /// Number of lines in new content.
    new_len: usize,
    /// Start position in the change content buffer.
    content_start: ChangePosition,
    /// End position in the change content buffer.
    content_end: ChangePosition,
    /// Whether this hunk uses the full file content (Replace/Insert with NeedsReplace).
    uses_full_content: bool,
}

/// Enrich FileOps with content ranges for Edit (modification) operations.
///
/// For file modifications, hunks may be Insert, Delete, or Replace operations.
/// This function correlates the hunks with LineOps based on line numbers and
/// computes the byte ranges for inserted content.
///
/// # Arguments
///
/// * `file_ops` - The FileOps to enrich
/// * `content` - The full new file content
/// * `hunk_ranges` - Content range information from globalized hunks
fn enrich_file_ops_for_edit(
    file_ops: &mut crate::change::FileOps,
    content: &[u8],
    hunk_ranges: &[HunkContentRange],
) {
    // For modifications, we have two cases:
    // 1. Simple Insert hunks: Content is the inserted lines only
    // 2. Replace hunks (including NeedsReplace): Content is the full file
    //
    // We need to compute per-line byte ranges within the content that was
    // actually written to the change buffer.

    // Check if any hunk uses full content (Replace/NeedsReplace)
    let uses_full_content = hunk_ranges
        .iter()
        .any(|h| h.uses_full_content && h.new_len > 0);

    if uses_full_content {
        // For Replace hunks, the full file content was written
        // Find the hunk that contains the full content
        if let Some(range) = hunk_ranges
            .iter()
            .find(|h| h.uses_full_content && h.new_len > 0)
        {
            // The content buffer contains the full new file
            // Compute per-line ranges similar to FileAdd
            enrich_lines_from_full_content(file_ops, content, range.content_start);
        }
    } else {
        // For simple Insert hunks, each hunk contains only its inserted lines
        // We need to correlate each hunk's line range with LineOps
        for range in hunk_ranges {
            if range.new_len == 0 {
                continue; // Delete-only hunk, no content
            }

            // This hunk inserts lines [new_start, new_start + new_len)
            // The content for these lines is at [content_start, content_end)
            enrich_lines_from_hunk_content(
                file_ops,
                content,
                range.new_start,
                range.new_len,
                range.content_start,
                range.content_end,
            );
        }
    }
}

/// Enrich LineOps when the full file content was written (Replace scenario).
fn enrich_lines_from_full_content(
    file_ops: &mut crate::change::FileOps,
    content: &[u8],
    content_start: ChangePosition,
) {
    // This is the same logic as enrich_file_ops_for_add
    let mut line_start = 0usize;
    let mut line_idx = 0usize;

    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' {
            let line_end = i + 1;

            // Find the corresponding LineOps entry by line number (1-indexed)
            if let Some(line_ops) = file_ops
                .line_ops_mut()
                .iter_mut()
                .find(|ops| ops.new_line_num() == Some(line_idx + 1))
            {
                let abs_start = ChangePosition::new(content_start.get() + line_start as u64);
                let abs_end = ChangePosition::new(content_start.get() + line_end as u64);
                line_ops.set_content_range(abs_start, abs_end);
            }

            line_start = line_end;
            line_idx += 1;
        }
    }

    // Handle last line if it doesn't end with newline
    if line_start < content.len() {
        if let Some(line_ops) = file_ops
            .line_ops_mut()
            .iter_mut()
            .find(|ops| ops.new_line_num() == Some(line_idx + 1))
        {
            let abs_start = ChangePosition::new(content_start.get() + line_start as u64);
            let abs_end = ChangePosition::new(content_start.get() + content.len() as u64);
            line_ops.set_content_range(abs_start, abs_end);
        }
    }
}

/// Enrich LineOps for a specific hunk's inserted content.
///
/// This handles the case where a hunk only contains its inserted lines,
/// not the full file content.
fn enrich_lines_from_hunk_content(
    file_ops: &mut crate::change::FileOps,
    full_content: &[u8],
    hunk_new_start: usize,
    hunk_new_len: usize,
    content_start: ChangePosition,
    content_end: ChangePosition,
) {
    // Extract the slice of content that corresponds to this hunk
    // We need to find the byte range in full_content for lines [hunk_new_start, hunk_new_start + hunk_new_len)

    // First, find the byte offset in full_content for hunk_new_start
    let mut byte_offset = 0usize;
    let mut current_line = 0usize;

    for (i, &byte) in full_content.iter().enumerate() {
        if current_line == hunk_new_start {
            byte_offset = i;
            break;
        }
        if byte == b'\n' {
            current_line += 1;
        }
    }

    // Now process lines within the hunk's range
    let mut line_start_in_hunk = 0usize; // Relative to the hunk's content in the buffer
    let mut lines_processed = 0usize;

    // Iterate through full_content starting from the hunk's starting line
    let hunk_content_len = (content_end.get() - content_start.get()) as usize;
    let hunk_slice_start = byte_offset;
    let hunk_slice_end = (byte_offset + hunk_content_len).min(full_content.len());

    if hunk_slice_start >= full_content.len() {
        return;
    }

    let hunk_slice = &full_content[hunk_slice_start..hunk_slice_end];

    for (i, &byte) in hunk_slice.iter().enumerate() {
        if byte == b'\n' {
            let line_end_in_hunk = i + 1;
            let actual_line_num = hunk_new_start + lines_processed;

            // Find the corresponding LineOps entry (1-indexed)
            if let Some(line_ops) = file_ops
                .line_ops_mut()
                .iter_mut()
                .find(|ops| ops.new_line_num() == Some(actual_line_num + 1))
            {
                let abs_start =
                    ChangePosition::new(content_start.get() + line_start_in_hunk as u64);
                let abs_end = ChangePosition::new(content_start.get() + line_end_in_hunk as u64);
                line_ops.set_content_range(abs_start, abs_end);
            }

            line_start_in_hunk = line_end_in_hunk;
            lines_processed += 1;

            if lines_processed >= hunk_new_len {
                break;
            }
        }
    }

    // Handle last line if it doesn't end with newline
    if lines_processed < hunk_new_len && line_start_in_hunk < hunk_slice.len() {
        let actual_line_num = hunk_new_start + lines_processed;

        if let Some(line_ops) = file_ops
            .line_ops_mut()
            .iter_mut()
            .find(|ops| ops.new_line_num() == Some(actual_line_num + 1))
        {
            let abs_start = ChangePosition::new(content_start.get() + line_start_in_hunk as u64);
            let abs_end = content_end;
            line_ops.set_content_range(abs_start, abs_end);
        }
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // GlobalizeOptions Tests
    // ========================================================================

    #[test]
    fn test_options_new_returns_defaults() {
        let opts = GlobalizeOptions::new();
        assert!(!opts.get_include_empty_files());
        assert!(opts.get_validate_positions());
        assert_eq!(opts.get_max_hunk_size(), 0);
        assert_eq!(opts.get_default_encoding(), Encoding::Utf8);
    }

    #[test]
    fn test_options_default() {
        let opts = GlobalizeOptions::default();
        assert!(!opts.get_include_empty_files());
        assert!(opts.get_validate_positions());
    }

    #[test]
    fn test_options_include_empty_files() {
        let opts = GlobalizeOptions::new().include_empty_files(true);
        assert!(opts.get_include_empty_files());
    }

    #[test]
    fn test_options_validate_positions() {
        let opts = GlobalizeOptions::new().validate_positions(false);
        assert!(!opts.get_validate_positions());
    }

    #[test]
    fn test_options_max_hunk_size() {
        let opts = GlobalizeOptions::new().max_hunk_size(1024);
        assert_eq!(opts.get_max_hunk_size(), 1024);
    }

    #[test]
    fn test_options_default_encoding() {
        let opts = GlobalizeOptions::new().default_encoding(Encoding::Binary);
        assert_eq!(opts.get_default_encoding(), Encoding::Binary);
    }

    #[test]
    fn test_options_builder_chain() {
        let opts = GlobalizeOptions::new()
            .include_empty_files(true)
            .validate_positions(false)
            .max_hunk_size(2048)
            .default_encoding(Encoding::Latin1);

        assert!(opts.get_include_empty_files());
        assert!(!opts.get_validate_positions());
        assert_eq!(opts.get_max_hunk_size(), 2048);
        assert_eq!(opts.get_default_encoding(), Encoding::Latin1);
    }

    #[test]
    fn test_options_clone() {
        let opts1 = GlobalizeOptions::new().include_empty_files(true);
        let opts2 = opts1.clone();
        assert!(opts2.get_include_empty_files());
    }

    #[test]
    fn test_options_debug() {
        let opts = GlobalizeOptions::new();
        let debug = format!("{:?}", opts);
        assert!(debug.contains("GlobalizeOptions"));
    }

    // ========================================================================
    // GlobalizeError Tests
    // ========================================================================

    #[test]
    fn test_error_path_not_found() {
        let err = GlobalizeError::PathNotFound {
            path: "test.rs".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
        assert!(msg.contains("not found"));
    }

    #[test]
    fn test_error_inode_not_found() {
        let err = GlobalizeError::InodeNotFound {
            inode: Inode::new(42),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("42"));
    }

    #[test]
    fn test_error_parent_not_found() {
        let err = GlobalizeError::ParentNotFound {
            path: "src/test.rs".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("src/test.rs"));
    }

    #[test]
    fn test_error_missing_context() {
        let err = GlobalizeError::MissingContext {
            path: "test.rs".to_string(),
            line: 42,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
        assert!(msg.contains("42"));
    }

    #[test]
    fn test_error_missing_field() {
        let err = GlobalizeError::MissingField {
            path: "test.rs".to_string(),
            field: "inode",
        };
        let msg = format!("{}", err);
        assert!(msg.contains("test.rs"));
        assert!(msg.contains("inode"));
    }

    #[test]
    fn test_error_invalid_line() {
        let err = GlobalizeError::InvalidLine {
            path: "test.rs".to_string(),
            line: 100,
            max_line: 50,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("100"));
        assert!(msg.contains("50"));
    }

    // ========================================================================
    // CacheStats Tests
    // ========================================================================

    #[test]
    fn test_cache_stats_display() {
        let stats = CacheStats {
            inode_cache_size: 10,
            position_cache_size: 20,
        };
        let display = format!("{}", stats);
        assert!(display.contains("10"));
        assert!(display.contains("20"));
    }

    #[test]
    fn test_cache_stats_equality() {
        let stats1 = CacheStats {
            inode_cache_size: 5,
            position_cache_size: 10,
        };
        let stats2 = CacheStats {
            inode_cache_size: 5,
            position_cache_size: 10,
        };
        let stats3 = CacheStats {
            inode_cache_size: 5,
            position_cache_size: 15,
        };
        assert_eq!(stats1, stats2);
        assert_ne!(stats1, stats3);
    }

    // ========================================================================
    // Helper Function Tests
    // ========================================================================

    #[test]
    fn test_extract_filename_with_path() {
        assert_eq!(extract_filename("src/lib/mod.rs"), "mod.rs");
    }

    #[test]
    fn test_extract_filename_root_level() {
        assert_eq!(extract_filename("Cargo.toml"), "Cargo.toml");
    }

    #[test]
    fn test_extract_filename_deep_path() {
        assert_eq!(extract_filename("a/b/c/d/e.txt"), "e.txt");
    }

    #[test]
    fn test_extract_filename_empty() {
        assert_eq!(extract_filename(""), "");
    }

    #[test]
    fn test_extract_parent_with_path() {
        assert_eq!(extract_parent("src/lib/mod.rs"), "src/lib");
    }

    #[test]
    fn test_extract_parent_root_level() {
        assert_eq!(extract_parent("Cargo.toml"), "");
    }

    #[test]
    fn test_extract_parent_deep_path() {
        assert_eq!(extract_parent("a/b/c/d/e.txt"), "a/b/c/d");
    }

    #[test]
    fn test_extract_parent_empty() {
        assert_eq!(extract_parent(""), "");
    }

    // ========================================================================
    // Position Conversion Tests
    // ========================================================================

    #[test]
    fn test_position_to_option_hash_root() {
        let pos = Position::new(NodeId::ROOT, ChangePosition::new(0));
        let converted = position_to_option_hash(pos);
        // ROOT positions use Some(Hash::NONE) to indicate the virtual root span
        assert!(converted.change.is_some());
        assert_eq!(converted.change.unwrap(), Hash::NONE);
        assert_eq!(converted.pos, ChangePosition::new(0));
    }

    #[test]
    fn test_position_to_option_hash_non_root() {
        let pos = Position::new(NodeId::new(42), ChangePosition::new(100));
        let converted = position_to_option_hash(pos);
        // Currently returns None for self-reference
        assert!(converted.change.is_none());
        assert_eq!(converted.pos, ChangePosition::new(100));
    }

    #[test]
    fn test_vertex_to_option_hash() {
        let node = GraphNode::new(
            NodeId::new(1),
            ChangePosition::new(0),
            ChangePosition::new(10),
        );
        let converted = vertex_to_option_hash(node);
        assert!(converted.change.is_none());
        assert_eq!(converted.start, ChangePosition::new(0));
        assert_eq!(converted.end, ChangePosition::new(10));
    }

    #[test]
    fn test_node_id_to_option_hash_root() {
        let result = node_id_to_option_hash(NodeId::ROOT);
        // ROOT node uses Some(Hash::NONE) to indicate the virtual root
        assert!(result.is_some());
        assert_eq!(result.unwrap(), Hash::NONE);
    }

    #[test]
    fn test_node_id_to_option_hash_non_root() {
        let result = node_id_to_option_hash(NodeId::new(42));
        // Currently returns None for self-reference
        assert!(result.is_none());
    }

    // ========================================================================
    // GlobalizedFile Tests
    // ========================================================================

    #[test]
    fn test_globalized_file_new() {
        let gf = GlobalizedFile::new("test.rs");
        assert_eq!(gf.path(), "test.rs");
        assert!(gf.is_empty());
        assert_eq!(gf.hunk_count(), 0);
    }

    #[test]
    fn test_globalized_file_set_bytes() {
        let mut gf = GlobalizedFile::new("test.rs");
        gf.set_bytes_added(100);
        assert_eq!(gf.bytes_added(), 100);
    }

    #[test]
    fn test_globalized_file_set_deps() {
        let mut gf = GlobalizedFile::new("test.rs");
        gf.set_dependency_count(5);
        assert_eq!(gf.dependency_count(), 5);
    }

    #[test]
    fn test_globalized_file_into_hunks() {
        let gf = GlobalizedFile::new("test.rs");
        let hunks = gf.into_hunks();
        assert!(hunks.is_empty());
    }
}

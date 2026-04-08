//! Context structures for change detection and recording.
//!
//! This module provides context types that bundle together the various
//! components needed for detecting changes and recording them. These contexts
//! encapsulate the pristine transaction, working copy, and change store,
//! providing a clean interface for the detection and recording workflows.
//!
//! # Overview
//!
//! Change detection and recording require access to multiple components:
//!
//! - **Pristine Transaction**: Read access to the repository graph and tree
//! - **Working Copy**: File system operations for reading/writing files
//! - **Change Store**: Access to change contents for comparison
//!
//! Rather than passing these separately to every function, we bundle them
//! into context structures that can be passed around.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                       Context Architecture                               │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  DetectContext<T, W, C>                                                 │
//! │  ┌───────────────────────────────────────────────────────────────────┐ │
//! │  │                                                                   │ │
//! │  │  ┌─────────────┐  ┌─────────────────┐  ┌─────────────────────┐   │ │
//! │  │  │  Pristine   │  │  Working Copy   │  │   Change Store      │   │ │
//! │  │  │  (read)     │  │  (read)         │  │   (content)         │   │ │
//! │  │  └─────────────┘  └─────────────────┘  └─────────────────────┘   │ │
//! │  │         │                  │                     │               │ │
//! │  │         └──────────────────┼─────────────────────┘               │ │
//! │  │                            │                                     │ │
//! │  │                            ▼                                     │ │
//! │  │                   ┌─────────────────┐                            │ │
//! │  │                   │ detect_changes()│                            │ │
//! │  │                   └─────────────────┘                            │ │
//! │  └───────────────────────────────────────────────────────────────────┘ │
//! │                                                                         │
//! │  RecordContext<T, W, C>                                                 │
//! │  ┌───────────────────────────────────────────────────────────────────┐ │
//! │  │                                                                   │ │
//! │  │  DetectContext + RecordBuilder + View Reference                   │ │
//! │  │                                                                   │ │
//! │  │         ┌─────────────────────────────────────────┐              │ │
//! │  │         │  record() / record_files() / record_path()  │          │ │
//! │  │         └─────────────────────────────────────────┘              │ │
//! │  └───────────────────────────────────────────────────────────────────┘ │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::record::{DetectContext, RecordContext, DetectOptions};
//!
//! // Create a detection context
//! let detect_ctx = DetectContext::new(&txn, &working_copy, &changes);
//!
//! // Detect changes with options
//! let result = detect_ctx.detect_changes(DetectOptions::new())?;
//!
//! // Or create a full recording context
//! let record_ctx = RecordContext::new(&txn, &view, &working_copy, &changes);
//!
//! // Record all changes
//! let recorded = record_ctx.record("")?;
//! ```
//!
//! # Thread Safety
//!
//! Context structures hold references to their components and are designed
//! for single-threaded use. For parallel recording, create separate contexts
//! for each thread with appropriate synchronization on shared state.

use crate::change::ChangeStore;
use crate::diff::Algorithm;
use crate::output::WorkingCopyRead;
use crate::pristine::{GraphTxnT, TreeTxnT, ViewState, ViewTxnT};

use super::builder::RecordBuilder;
use super::detect::DetectOptions;

// DETECT CONTEXT

/// Context for change detection operations.
///
/// This struct bundles together all the components needed to detect changes
/// between the working copy and the pristine state. It provides a clean
/// interface for the detection workflow.
///
/// # Type Parameters
///
/// * `T` - Transaction type implementing `GraphTxnT + TreeTxnT + ViewTxnT`
/// * `W` - Working copy type implementing `WorkingCopyRead`
/// * `C` - Change store type implementing `ChangeStore`
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::{DetectContext, DetectOptions};
///
/// let ctx = DetectContext::new(&txn, &working_copy, &changes);
///
/// // Detect all changes
/// let result = ctx.detect_changes(DetectOptions::new())?;
///
/// // Detect changes under a prefix
/// let result = ctx.detect_changes(
///     DetectOptions::new().with_prefix("src/")
/// )?;
/// ```
#[derive(Debug)]
pub struct DetectContext<'a, T, W, C>
where
    T: GraphTxnT + TreeTxnT + ViewTxnT,
    W: WorkingCopyRead,
    C: ChangeStore,
{
    /// Read-only access to the pristine database.
    txn: &'a T,

    /// Reference to the working copy for file operations.
    working_copy: &'a W,

    /// Reference to the change store for content retrieval.
    change_store: &'a C,

    /// The view to compare against.
    ///
    /// If `None`, uses the repository's current view.
    view: Option<&'a ViewState>,

    /// Detection options.
    options: DetectOptions,
}

impl<'a, T, W, C> DetectContext<'a, T, W, C>
where
    T: GraphTxnT + TreeTxnT + ViewTxnT,
    W: WorkingCopyRead,
    C: ChangeStore,
{
    /// Create a new detection context.
    ///
    /// # Arguments
    ///
    /// * `txn` - Read-only pristine transaction
    /// * `working_copy` - Working copy for file access
    /// * `change_store` - Change store for content retrieval
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let ctx = DetectContext::new(&txn, &working_copy, &changes);
    /// ```
    pub fn new(txn: &'a T, working_copy: &'a W, change_store: &'a C) -> Self {
        Self {
            txn,
            working_copy,
            change_store,
            view: None,
            options: DetectOptions::default(),
        }
    }

    /// Set the view to compare against.
    ///
    /// By default, detection compares against the repository's current view.
    /// Use this method to compare against a specific view.
    ///
    /// # Arguments
    ///
    /// * `view` - The view state to compare against
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let ctx = DetectContext::new(&txn, &working_copy, &changes)
    ///     .with_view(&feature_view);
    /// ```
    pub fn with_view(mut self, view: &'a ViewState) -> Self {
        self.view = Some(view);
        self
    }

    /// Set detection options.
    ///
    /// # Arguments
    ///
    /// * `options` - Detection options to use
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::diff::Algorithm;
    ///
    /// let ctx = DetectContext::new(&txn, &working_copy, &changes)
    ///     .with_options(
    ///         DetectOptions::new()
    ///             .with_algorithm(Algorithm::Patience)
    ///             .with_check_mtime(false)
    ///     );
    /// ```
    pub fn with_options(mut self, options: DetectOptions) -> Self {
        self.options = options;
        self
    }

    /// Get the pristine transaction reference.
    ///
    /// # Returns
    ///
    /// Reference to the pristine transaction.
    pub fn txn(&self) -> &T {
        self.txn
    }

    /// Get the working copy reference.
    ///
    /// # Returns
    ///
    /// Reference to the working copy.
    pub fn working_copy(&self) -> &W {
        self.working_copy
    }

    /// Get the change store reference.
    ///
    /// # Returns
    ///
    /// Reference to the change store.
    pub fn change_store(&self) -> &C {
        self.change_store
    }

    /// Get the view reference, if set.
    ///
    /// # Returns
    ///
    /// Optional reference to the view being compared against.
    pub fn view(&self) -> Option<&ViewState> {
        self.view
    }

    /// Get the detection options.
    ///
    /// # Returns
    ///
    /// Reference to the current detection options.
    pub fn options(&self) -> &DetectOptions {
        &self.options
    }

    /// Get the diff algorithm from options.
    ///
    /// # Returns
    ///
    /// The diff algorithm to use for content comparison.
    pub fn algorithm(&self) -> Algorithm {
        self.options.algorithm
    }

    /// Check if mtime optimization is enabled.
    ///
    /// # Returns
    ///
    /// `true` if mtime checking is enabled.
    pub fn check_mtime(&self) -> bool {
        self.options.check_mtime
    }

    /// Get the prefix filter from options.
    ///
    /// # Returns
    ///
    /// The path prefix to filter detection, or empty string for all files.
    pub fn prefix(&self) -> &str {
        &self.options.prefix
    }
}

impl<'a, T, W, C> Clone for DetectContext<'a, T, W, C>
where
    T: GraphTxnT + TreeTxnT + ViewTxnT,
    W: WorkingCopyRead,
    C: ChangeStore,
{
    fn clone(&self) -> Self {
        Self {
            txn: self.txn,
            working_copy: self.working_copy,
            change_store: self.change_store,
            view: self.view,
            options: self.options.clone(),
        }
    }
}

// RECORD CONTEXT

/// Context for recording changes.
///
/// This struct extends [`DetectContext`] with a [`RecordBuilder`] and view
/// reference, providing everything needed for the full recording workflow.
///
/// # Type Parameters
///
/// * `T` - Transaction type implementing `GraphTxnT + TreeTxnT + ViewTxnT`
/// * `W` - Working copy type implementing `WorkingCopyRead`
/// * `C` - Change store type implementing `ChangeStore`
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::{RecordContext, DetectOptions};
///
/// let mut ctx = RecordContext::new(&txn, &view, &working_copy, &changes);
///
/// // Record all changes
/// let recorded = ctx.record("")?;
///
/// // Record specific files
/// let recorded = ctx.record_files(&["src/main.rs", "src/lib.rs"])?;
/// ```
#[derive(Debug)]
pub struct RecordContext<'a, T, W, C>
where
    T: GraphTxnT + TreeTxnT + ViewTxnT,
    W: WorkingCopyRead,
    C: ChangeStore,
{
    /// The detection context (holds txn, working_copy, change_store).
    detect: DetectContext<'a, T, W, C>,

    /// The record builder for accumulating changes.
    builder: RecordBuilder,

    /// The view we're recording changes to.
    view: &'a ViewState,
}

impl<'a, T, W, C> RecordContext<'a, T, W, C>
where
    T: GraphTxnT + TreeTxnT + ViewTxnT,
    W: WorkingCopyRead,
    C: ChangeStore,
{
    /// Create a new recording context.
    ///
    /// # Arguments
    ///
    /// * `txn` - Read-only pristine transaction
    /// * `view` - The view to record changes to
    /// * `working_copy` - Working copy for file access
    /// * `change_store` - Change store for content retrieval
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let ctx = RecordContext::new(&txn, &view, &working_copy, &changes);
    /// ```
    pub fn new(txn: &'a T, view: &'a ViewState, working_copy: &'a W, change_store: &'a C) -> Self {
        Self {
            detect: DetectContext::new(txn, working_copy, change_store).with_view(view),
            builder: RecordBuilder::new(),
            view,
        }
    }

    /// Create a recording context with a pre-configured builder.
    ///
    /// Use this when you need to customize the builder before recording.
    ///
    /// # Arguments
    ///
    /// * `txn` - Read-only pristine transaction
    /// * `view` - The view to record changes to
    /// * `working_copy` - Working copy for file access
    /// * `change_store` - Change store for content retrieval
    /// * `builder` - Pre-configured record builder
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut builder = RecordBuilder::with_capacity(100, 1024 * 1024);
    /// builder.force_rediff = true;
    ///
    /// let ctx = RecordContext::with_builder(
    ///     &txn, &view, &working_copy, &changes, builder
    /// );
    /// ```
    pub fn with_builder(
        txn: &'a T,
        view: &'a ViewState,
        working_copy: &'a W,
        change_store: &'a C,
        builder: RecordBuilder,
    ) -> Self {
        Self {
            detect: DetectContext::new(txn, working_copy, change_store).with_view(view),
            builder,
            view,
        }
    }

    /// Set detection options.
    ///
    /// # Arguments
    ///
    /// * `options` - Detection options to use
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let ctx = RecordContext::new(&txn, &view, &working_copy, &changes)
    ///     .with_options(DetectOptions::new().with_check_mtime(false));
    /// ```
    pub fn with_options(mut self, options: DetectOptions) -> Self {
        self.detect = self.detect.with_options(options);
        self
    }

    /// Get a reference to the detection context.
    ///
    /// # Returns
    ///
    /// Reference to the underlying detection context.
    pub fn detect_context(&self) -> &DetectContext<'a, T, W, C> {
        &self.detect
    }

    /// Get a reference to the record builder.
    ///
    /// # Returns
    ///
    /// Reference to the record builder.
    pub fn builder(&self) -> &RecordBuilder {
        &self.builder
    }

    /// Get a mutable reference to the record builder.
    ///
    /// # Returns
    ///
    /// Mutable reference to the record builder.
    pub fn builder_mut(&mut self) -> &mut RecordBuilder {
        &mut self.builder
    }

    /// Get the view reference.
    ///
    /// # Returns
    ///
    /// Reference to the view being recorded to.
    pub fn view(&self) -> &ViewState {
        self.view
    }

    /// Get the pristine transaction reference.
    ///
    /// # Returns
    ///
    /// Reference to the pristine transaction.
    pub fn txn(&self) -> &T {
        self.detect.txn()
    }

    /// Get the working copy reference.
    ///
    /// # Returns
    ///
    /// Reference to the working copy.
    pub fn working_copy(&self) -> &W {
        self.detect.working_copy()
    }

    /// Get the change store reference.
    ///
    /// # Returns
    ///
    /// Reference to the change store.
    pub fn change_store(&self) -> &C {
        self.detect.change_store()
    }

    /// Get the detection options.
    ///
    /// # Returns
    ///
    /// Reference to the current detection options.
    pub fn options(&self) -> &DetectOptions {
        self.detect.options()
    }

    /// Check if the builder is empty (no changes recorded).
    ///
    /// # Returns
    ///
    /// `true` if no hunks have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.builder.is_empty()
    }

    /// Get the number of hunks recorded so far.
    ///
    /// # Returns
    ///
    /// The count of recorded hunks.
    pub fn hunk_count(&self) -> usize {
        self.builder.hunk_count()
    }

    /// Consume the context and return the builder.
    ///
    /// Use this after recording to get the accumulated changes.
    ///
    /// # Returns
    ///
    /// The record builder with all accumulated changes.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut ctx = RecordContext::new(&txn, &view, &working_copy, &changes);
    /// // ... record changes ...
    /// let builder = ctx.into_builder();
    /// let recorded = builder.finish();
    /// ```
    pub fn into_builder(self) -> RecordBuilder {
        self.builder
    }

    /// Take the builder, replacing it with a new empty builder.
    ///
    /// This allows continuing to use the context after extracting recorded changes.
    ///
    /// # Returns
    ///
    /// The record builder with accumulated changes.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut ctx = RecordContext::new(&txn, &view, &working_copy, &changes);
    /// // ... record first batch ...
    /// let first_batch = ctx.take_builder();
    /// // ... record second batch ...
    /// let second_batch = ctx.take_builder();
    /// ```
    pub fn take_builder(&mut self) -> RecordBuilder {
        std::mem::take(&mut self.builder)
    }
}

// RECORD ITEM - Internal tracking structure

/// Internal item for tracking files during recording.
///
/// This structure holds information about a file or directory being processed
/// during the recording workflow. It tracks the path, inode, and parent
/// relationships needed to build change hunks.
#[derive(Debug, Clone)]
pub struct RecordItem {
    /// Full path relative to repository root.
    pub full_path: String,

    /// Base name (file/directory name without path).
    pub basename: String,

    /// The inode of this file/directory.
    pub inode: crate::types::Inode,

    /// The parent's inode.
    pub parent_inode: crate::types::Inode,

    /// The parent's position in the graph (if known).
    ///
    /// This is `None` for the root or when the parent hasn't been
    /// recorded yet.
    pub parent_position: Option<crate::types::Position<Option<crate::types::NodeId>>>,

    /// File metadata (permissions, type).
    pub metadata: crate::output::FileMetadata,

    /// Whether this is a directory.
    pub is_directory: bool,
}

impl RecordItem {
    /// Create a new record item.
    ///
    /// # Arguments
    ///
    /// * `full_path` - Full path relative to repository root
    /// * `inode` - The file's inode
    /// * `parent_inode` - The parent directory's inode
    /// * `metadata` - File metadata
    ///
    /// # Returns
    ///
    /// A new `RecordItem`.
    pub fn new(
        full_path: impl Into<String>,
        inode: crate::types::Inode,
        parent_inode: crate::types::Inode,
        metadata: crate::output::FileMetadata,
    ) -> Self {
        let full_path = full_path.into();
        let basename = std::path::Path::new(&full_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let is_directory = metadata.is_dir;

        Self {
            full_path,
            basename,
            inode,
            parent_inode,
            parent_position: None,
            metadata,
            is_directory,
        }
    }

    /// Create a record item for the repository root.
    ///
    /// # Returns
    ///
    /// A `RecordItem` representing the repository root.
    pub fn root() -> Self {
        Self {
            full_path: String::new(),
            basename: String::new(),
            inode: crate::types::Inode::ROOT,
            parent_inode: crate::types::Inode::ROOT,
            parent_position: Some(crate::types::Position::ROOT.to_option()),
            metadata: crate::output::FileMetadata::directory(),
            is_directory: true,
        }
    }

    /// Check if this item is the repository root.
    ///
    /// # Returns
    ///
    /// `true` if this is the root item.
    pub fn is_root(&self) -> bool {
        self.inode == crate::types::Inode::ROOT && self.full_path.is_empty()
    }

    /// Set the parent position.
    ///
    /// # Arguments
    ///
    /// * `position` - The parent's graph position
    pub fn with_parent_position(
        mut self,
        position: crate::types::Position<Option<crate::types::NodeId>>,
    ) -> Self {
        self.parent_position = Some(position);
        self
    }
}

impl Default for RecordItem {
    fn default() -> Self {
        Self::root()
    }
}

// FILE STATE - Pristine file state for comparison

/// Represents the pristine state of a file for comparison.
///
/// This structure captures what we know about a file from the pristine
/// database, allowing efficient comparison with the working copy.
#[derive(Debug, Clone)]
pub struct PristineFileState {
    /// The file's inode.
    pub inode: crate::types::Inode,

    /// Position in the graph.
    pub position: crate::types::Position<crate::types::NodeId>,

    /// File path.
    pub path: String,

    /// Recorded content hash (if available).
    pub content_hash: Option<crate::types::Hash>,

    /// Recorded file size (if known).
    pub size: Option<u64>,

    /// Recorded modification time (if known).
    pub mtime: Option<std::time::SystemTime>,

    /// Whether this is a directory.
    pub is_directory: bool,
}

impl PristineFileState {
    /// Create a new pristine file state.
    ///
    /// # Arguments
    ///
    /// * `inode` - The file's inode
    /// * `position` - Position in the graph
    /// * `path` - File path
    ///
    /// # Returns
    ///
    /// A new `PristineFileState`.
    pub fn new(
        inode: crate::types::Inode,
        position: crate::types::Position<crate::types::NodeId>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            inode,
            position,
            path: path.into(),
            content_hash: None,
            size: None,
            mtime: None,
            is_directory: false,
        }
    }

    /// Mark this as a directory.
    pub fn as_directory(mut self) -> Self {
        self.is_directory = true;
        self
    }

    /// Set the content hash.
    pub fn with_content_hash(mut self, hash: crate::types::Hash) -> Self {
        self.content_hash = Some(hash);
        self
    }

    /// Set the file size.
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// Set the modification time.
    pub fn with_mtime(mut self, mtime: std::time::SystemTime) -> Self {
        self.mtime = Some(mtime);
        self
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::MemoryChangeStore;
    use crate::diff::Algorithm;
    use crate::output::Memory;
    use crate::pristine::PristineError;
    use crate::types::{
        ChangePosition, EdgeFlags, GraphNode, Hash, Inode, Merkle, NodeId, Position,
        SerializedGraphEdge,
    };

    // Mock Transaction for Testing

    /// Mock transaction for testing contexts.
    #[derive(Debug, Default)]
    struct MockTxn;

    /// Mock adjacency iterator that wraps results.
    struct MockAdjIter(std::vec::IntoIter<SerializedGraphEdge>);

    impl Iterator for MockAdjIter {
        type Item = Result<SerializedGraphEdge, PristineError>;

        fn next(&mut self) -> Option<Self::Item> {
            self.0.next().map(Ok)
        }
    }

    impl GraphTxnT for MockTxn {
        type Adj = MockAdjIter;

        fn get_external(&self, _id: NodeId) -> Result<Option<Hash>, PristineError> {
            Ok(None)
        }

        fn get_internal(&self, _hash: &Hash) -> Result<Option<NodeId>, PristineError> {
            Ok(None)
        }

        fn iter_adjacent(
            &self,
            _vertex: GraphNode<NodeId>,
            _min_flag: EdgeFlags,
            _max_flag: EdgeFlags,
        ) -> Result<Self::Adj, PristineError> {
            Ok(MockAdjIter(Vec::new().into_iter()))
        }

        fn find_block(&self, _pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
            Err(PristineError::BlockNotFound { change: 0, pos: 0 })
        }

        fn find_block_end(
            &self,
            _pos: Position<NodeId>,
        ) -> Result<GraphNode<NodeId>, PristineError> {
            Err(PristineError::BlockNotFound { change: 0, pos: 0 })
        }

        fn has_vertex(&self, _vertex: GraphNode<NodeId>) -> Result<bool, PristineError> {
            Ok(false)
        }

        fn get_node_type(&self, _node_id: NodeId) -> Result<Option<u8>, PristineError> {
            Ok(None)
        }

        fn get_rev_deps(&self, _dep_id: NodeId) -> Result<Vec<NodeId>, PristineError> {
            Ok(Vec::new())
        }

        fn has_change_in_graph(&self, _change_id: NodeId) -> Result<bool, PristineError> {
            Ok(false)
        }
    }

    impl TreeTxnT for MockTxn {
        fn get_inode(&self, _path: &str) -> Result<Option<Inode>, PristineError> {
            Ok(None)
        }

        fn get_directory_flags(&self, _inode: Inode) -> Result<Option<u8>, PristineError> {
            Ok(None)
        }

        fn get_path(&self, _inode: Inode) -> Result<Option<String>, PristineError> {
            Ok(None)
        }

        fn inode_position(&self, _inode: Inode) -> Result<Option<Position<NodeId>>, PristineError> {
            Ok(None)
        }

        fn position_inode(&self, _pos: Position<NodeId>) -> Result<Option<Inode>, PristineError> {
            Ok(None)
        }

        fn iter_tree(
            &self,
        ) -> Result<
            Box<dyn Iterator<Item = Result<(String, Inode), PristineError>> + '_>,
            PristineError,
        > {
            Ok(Box::new(std::iter::empty()))
        }

        fn iter_inode_vertices(
            &self,
            _inode: Inode,
        ) -> Result<
            Box<
                dyn Iterator<Item = Result<(GraphNode<NodeId>, SerializedGraphEdge), PristineError>>
                    + '_,
            >,
            PristineError,
        > {
            Ok(Box::new(std::iter::empty()))
        }

        fn get_file_mtime(&self, _path: &str) -> Result<Option<(i64, u32, u64)>, PristineError> {
            Ok(None)
        }
    }

    impl ViewTxnT for MockTxn {
        fn get_view_by_id(&self, _id: u64) -> Result<Option<ViewState>, PristineError> {
            Ok(None)
        }

        fn get_view(&self, _name: &str) -> Result<Option<ViewState>, PristineError> {
            Ok(None)
        }

        fn list_views(&self) -> Result<Vec<String>, PristineError> {
            Ok(Vec::new())
        }

        fn get_change_seq(
            &self,
            _view: &ViewState,
            _change_id: NodeId,
        ) -> Result<Option<u64>, PristineError> {
            Ok(None)
        }

        fn get_change_at_seq(
            &self,
            _view: &ViewState,
            _seq: u64,
        ) -> Result<Option<NodeId>, PristineError> {
            Ok(None)
        }

        fn iter_changes(
            &self,
            _view: &ViewState,
            _from_seq: u64,
        ) -> Result<
            Box<dyn Iterator<Item = Result<(u64, NodeId, Merkle), PristineError>> + '_>,
            PristineError,
        > {
            Ok(Box::new(std::iter::empty()))
        }
    }

    // DetectContext Tests

    #[test]
    fn test_detect_context_new() {
        let txn = MockTxn;
        let working_copy = Memory::new();
        let change_store = MemoryChangeStore::new();

        let ctx = DetectContext::new(&txn, &working_copy, &change_store);

        assert!(ctx.view().is_none());
        assert_eq!(ctx.algorithm(), Algorithm::Myers);
        assert!(ctx.check_mtime());
        assert!(ctx.prefix().is_empty());
    }

    #[test]
    fn test_detect_context_with_view() {
        let txn = MockTxn;
        let working_copy = Memory::new();
        let change_store = MemoryChangeStore::new();
        let view = ViewState::default();

        let ctx = DetectContext::new(&txn, &working_copy, &change_store).with_view(&view);

        assert!(ctx.view().is_some());
        assert_eq!(ctx.view().unwrap().name, view.name);
    }

    #[test]
    fn test_detect_context_with_options() {
        let txn = MockTxn;
        let working_copy = Memory::new();
        let change_store = MemoryChangeStore::new();

        let options = DetectOptions::new()
            .with_algorithm(Algorithm::Patience)
            .with_check_mtime(false)
            .with_prefix("src/");

        let ctx = DetectContext::new(&txn, &working_copy, &change_store).with_options(options);

        assert_eq!(ctx.algorithm(), Algorithm::Patience);
        assert!(!ctx.check_mtime());
        assert_eq!(ctx.prefix(), "src/");
    }

    #[test]
    fn test_detect_context_clone() {
        let txn = MockTxn;
        let working_copy = Memory::new();
        let change_store = MemoryChangeStore::new();

        let ctx = DetectContext::new(&txn, &working_copy, &change_store)
            .with_options(DetectOptions::new().with_prefix("test/"));

        let cloned = ctx.clone();

        assert_eq!(cloned.prefix(), "test/");
        assert_eq!(cloned.algorithm(), ctx.algorithm());
    }

    // RecordContext Tests

    #[test]
    fn test_record_context_new() {
        let txn = MockTxn;
        let working_copy = Memory::new();
        let change_store = MemoryChangeStore::new();
        let view = ViewState::default();

        let ctx = RecordContext::new(&txn, &view, &working_copy, &change_store);

        assert!(ctx.is_empty());
        assert_eq!(ctx.hunk_count(), 0);
        assert_eq!(ctx.view().name, view.name);
    }

    #[test]
    fn test_record_context_with_builder() {
        let txn = MockTxn;
        let working_copy = Memory::new();
        let change_store = MemoryChangeStore::new();
        let view = ViewState::default();

        let mut builder = RecordBuilder::with_capacity(10, 1024);
        builder.force_rediff = true;

        let ctx = RecordContext::with_builder(&txn, &view, &working_copy, &change_store, builder);

        assert!(ctx.builder().force_rediff);
    }

    #[test]
    fn test_record_context_with_options() {
        let txn = MockTxn;
        let working_copy = Memory::new();
        let change_store = MemoryChangeStore::new();
        let view = ViewState::default();

        let ctx = RecordContext::new(&txn, &view, &working_copy, &change_store)
            .with_options(DetectOptions::new().with_algorithm(Algorithm::Patience));

        assert_eq!(ctx.options().algorithm, Algorithm::Patience);
    }

    #[test]
    fn test_record_context_into_builder() {
        let txn = MockTxn;
        let working_copy = Memory::new();
        let change_store = MemoryChangeStore::new();
        let view = ViewState::default();

        let ctx = RecordContext::new(&txn, &view, &working_copy, &change_store);
        let builder = ctx.into_builder();

        assert!(builder.is_empty());
    }

    #[test]
    fn test_record_context_take_builder() {
        let txn = MockTxn;
        let working_copy = Memory::new();
        let change_store = MemoryChangeStore::new();
        let view = ViewState::default();

        let mut ctx = RecordContext::new(&txn, &view, &working_copy, &change_store);
        let first_builder = ctx.take_builder();
        let second_builder = ctx.take_builder();

        // Both should be empty since we're not adding any changes
        assert!(first_builder.is_empty());
        assert!(second_builder.is_empty());
    }

    #[test]
    fn test_record_context_builder_mut() {
        let txn = MockTxn;
        let working_copy = Memory::new();
        let change_store = MemoryChangeStore::new();
        let view = ViewState::default();

        let mut ctx = RecordContext::new(&txn, &view, &working_copy, &change_store);
        ctx.builder_mut().force_rediff = true;

        assert!(ctx.builder().force_rediff);
    }

    // RecordItem Tests

    #[test]
    fn test_record_item_new() {
        let item = RecordItem::new(
            "src/main.rs",
            Inode::new(42),
            Inode::new(1),
            crate::output::FileMetadata::file(),
        );

        assert_eq!(item.full_path, "src/main.rs");
        assert_eq!(item.basename, "main.rs");
        assert_eq!(item.inode, Inode::new(42));
        assert_eq!(item.parent_inode, Inode::new(1));
        assert!(!item.is_directory);
        assert!(item.parent_position.is_none());
    }

    #[test]
    fn test_record_item_root() {
        let item = RecordItem::root();

        assert!(item.is_root());
        assert!(item.full_path.is_empty());
        assert!(item.basename.is_empty());
        assert_eq!(item.inode, Inode::ROOT);
        assert!(item.is_directory);
        assert!(item.parent_position.is_some());
    }

    #[test]
    fn test_record_item_with_parent_position() {
        let pos = Position::ROOT.to_option();
        let item = RecordItem::new(
            "file.txt",
            Inode::new(10),
            Inode::ROOT,
            crate::output::FileMetadata::file(),
        )
        .with_parent_position(pos);

        assert!(item.parent_position.is_some());
    }

    #[test]
    fn test_record_item_directory() {
        let item = RecordItem::new(
            "src/",
            Inode::new(5),
            Inode::ROOT,
            crate::output::FileMetadata::directory(),
        );

        assert!(item.is_directory);
        assert_eq!(item.basename, "src");
    }

    #[test]
    fn test_record_item_default() {
        let item = RecordItem::default();

        assert!(item.is_root());
    }

    #[test]
    fn test_record_item_nested_path() {
        let item = RecordItem::new(
            "deeply/nested/path/file.rs",
            Inode::new(100),
            Inode::new(99),
            crate::output::FileMetadata::file(),
        );

        assert_eq!(item.basename, "file.rs");
        assert_eq!(item.full_path, "deeply/nested/path/file.rs");
    }

    #[test]
    fn test_record_item_clone() {
        let item = RecordItem::new(
            "test.txt",
            Inode::new(1),
            Inode::ROOT,
            crate::output::FileMetadata::file(),
        );

        let cloned = item.clone();

        assert_eq!(cloned.full_path, item.full_path);
        assert_eq!(cloned.inode, item.inode);
    }

    #[test]
    fn test_record_item_debug() {
        let item = RecordItem::root();
        let debug = format!("{:?}", item);

        assert!(debug.contains("RecordItem"));
    }

    // PristineFileState Tests

    #[test]
    fn test_pristine_file_state_new() {
        let state = PristineFileState::new(
            Inode::new(42),
            Position::new(NodeId::new(1), ChangePosition::new(0)),
            "src/main.rs",
        );

        assert_eq!(state.inode, Inode::new(42));
        assert_eq!(state.path, "src/main.rs");
        assert!(state.content_hash.is_none());
        assert!(state.size.is_none());
        assert!(state.mtime.is_none());
        assert!(!state.is_directory);
    }

    #[test]
    fn test_pristine_file_state_as_directory() {
        let state = PristineFileState::new(
            Inode::new(1),
            Position::new(NodeId::new(1), ChangePosition::new(0)),
            "src/",
        )
        .as_directory();

        assert!(state.is_directory);
    }

    #[test]
    fn test_pristine_file_state_with_content_hash() {
        let hash = Hash::of(b"test content");
        let state = PristineFileState::new(
            Inode::new(1),
            Position::new(NodeId::new(1), ChangePosition::new(0)),
            "file.txt",
        )
        .with_content_hash(hash);

        assert_eq!(state.content_hash, Some(hash));
    }

    #[test]
    fn test_pristine_file_state_with_size() {
        let state = PristineFileState::new(
            Inode::new(1),
            Position::new(NodeId::new(1), ChangePosition::new(0)),
            "file.txt",
        )
        .with_size(1024);

        assert_eq!(state.size, Some(1024));
    }

    #[test]
    fn test_pristine_file_state_with_mtime() {
        let now = std::time::SystemTime::now();
        let state = PristineFileState::new(
            Inode::new(1),
            Position::new(NodeId::new(1), ChangePosition::new(0)),
            "file.txt",
        )
        .with_mtime(now);

        assert_eq!(state.mtime, Some(now));
    }

    #[test]
    fn test_pristine_file_state_clone() {
        let state = PristineFileState::new(
            Inode::new(42),
            Position::new(NodeId::new(5), ChangePosition::new(100)),
            "path/to/file.rs",
        )
        .with_size(2048)
        .as_directory();

        let cloned = state.clone();

        assert_eq!(cloned.inode, state.inode);
        assert_eq!(cloned.path, state.path);
        assert_eq!(cloned.size, state.size);
        assert_eq!(cloned.is_directory, state.is_directory);
    }

    #[test]
    fn test_pristine_file_state_debug() {
        let state = PristineFileState::new(
            Inode::new(1),
            Position::new(NodeId::new(1), ChangePosition::new(0)),
            "test.txt",
        );
        let debug = format!("{:?}", state);

        assert!(debug.contains("PristineFileState"));
        assert!(debug.contains("test.txt"));
    }

    #[test]
    fn test_pristine_file_state_builder_chain() {
        let now = std::time::SystemTime::now();
        let hash = Hash::of(b"content");

        let state = PristineFileState::new(
            Inode::new(10),
            Position::new(NodeId::new(2), ChangePosition::new(50)),
            "chained.txt",
        )
        .with_content_hash(hash)
        .with_size(512)
        .with_mtime(now);

        assert_eq!(state.content_hash, Some(hash));
        assert_eq!(state.size, Some(512));
        assert_eq!(state.mtime, Some(now));
    }
}

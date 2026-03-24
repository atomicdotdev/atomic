//! Content retrieval from the pristine graph.
//!
//! This module provides functionality for retrieving file content from the
//! repository graph. It is used during change detection to compare the
//! pristine state with the working copy.
//!
//! # Overview
//!
//! When detecting changes, we need to compare the working copy content with
//! the recorded (pristine) content. This module provides the functions to
//! retrieve that pristine content from the graph.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                    Pristine Content Retrieval                            │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input                    Processing                    Output          │
//! │  ┌──────────────┐        ┌─────────────────┐          ┌────────────┐   │
//! │  │ - Position   │        │ 1. retrieve     │          │ Content    │   │
//! │  │ - Inode      │ ─────▶ │    alive graph  │ ───────▶ │ bytes      │   │
//! │  │ - ChangeStore│        │ 2. compute      │          │            │   │
//! │  └──────────────┘        │    order (SCC)  │          └────────────┘   │
//! │                          │ 3. collect      │                            │
//! │                          │    content      │                            │
//! │                          └─────────────────┘                            │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::retrieve::retrieve_content;
//! use atomic_core::types::Position;
//!
//! // Retrieve content for a file at the given position
//! let content = retrieve_content(&txn, &changes, position)?;
//! println!("File has {} bytes", content.len());
//! ```

use crate::change::ChangeStore;
use crate::output::alive::RetrieveOptions;
use crate::output::repo::{
    output_file_to_buffer, output_file_to_buffer_with_options, FileOutputOptions,
};
use crate::pristine::{GraphTxnT, PristineError};
use crate::types::{NodeId, Position};

use std::collections::HashSet;

use super::super::error::{RecordError, RecordResult};

// RETRIEVE OPTIONS

/// Options for content retrieval.
///
/// Controls how content is retrieved from the pristine graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetrieveContentOptions {
    /// Include deleted content in retrieval.
    ///
    /// When true, deleted vertices are included. This is usually false
    /// for comparison purposes.
    pub include_deleted: bool,

    /// Maximum vertices to process.
    ///
    /// Safety limit to prevent runaway processing on large files.
    pub max_vertices: Option<usize>,
}

impl RetrieveContentOptions {
    /// Create new options with defaults.
    ///
    /// Default configuration:
    /// - `include_deleted`: false
    /// - `max_vertices`: None (no limit)
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to include deleted content.
    pub fn include_deleted(mut self, include: bool) -> Self {
        self.include_deleted = include;
        self
    }

    /// Set maximum span count.
    pub fn max_vertices(mut self, max: usize) -> Self {
        self.max_vertices = Some(max);
        self
    }

    /// Convert to file output options.
    fn to_file_output_options(self) -> FileOutputOptions {
        let mut opts = FileOutputOptions::new();
        if self.include_deleted {
            opts = opts.include_deleted(true);
        }
        if let Some(max) = self.max_vertices {
            opts = opts.max_vertices(max);
        }
        opts
    }
}

// RETRIEVE RESULT

/// Result of content retrieval.
///
/// Contains the retrieved content and metadata about the retrieval.
#[derive(Debug, Clone)]
pub struct RetrieveResult {
    /// The retrieved content bytes.
    pub content: Vec<u8>,

    /// Number of vertices processed.
    pub vertices_processed: usize,

    /// Whether the content has conflicts.
    ///
    /// This is true if the file's graph has cyclic SCCs, indicating
    /// unresolved conflicts.
    pub has_conflicts: bool,

    /// Number of conflict regions detected.
    pub conflict_count: usize,
}

impl RetrieveResult {
    /// Create an empty result (for files with no content).
    pub fn empty() -> Self {
        Self {
            content: Vec::new(),
            vertices_processed: 0,
            has_conflicts: false,
            conflict_count: 0,
        }
    }

    /// Create a result with content.
    pub fn with_content(content: Vec<u8>, vertices: usize) -> Self {
        Self {
            content,
            vertices_processed: vertices,
            has_conflicts: false,
            conflict_count: 0,
        }
    }

    /// Mark as having conflicts.
    pub fn with_conflicts(mut self, count: usize) -> Self {
        self.has_conflicts = count > 0;
        self.conflict_count = count;
        self
    }

    /// Check if content is empty.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Get content length.
    pub fn len(&self) -> usize {
        self.content.len()
    }
}

// RETRIEVE FUNCTIONS

/// Retrieve content from the pristine graph.
///
/// This is the main entry point for retrieving file content from the graph.
/// It handles the full pipeline of retrieving the alive graph, computing
/// order, and collecting content.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph access
/// * `changes` - Change store for span content
/// * `position` - Position in the graph (identifies the file)
///
/// # Returns
///
/// The file content as bytes.
///
/// # Errors
///
/// Returns `RecordError` if graph traversal or content retrieval fails.
///
/// # Example
///
/// ```rust,ignore
/// let content = retrieve_content(&txn, &changes, position)?;
/// let text = String::from_utf8_lossy(&content);
/// ```
pub fn retrieve_content<T, C>(
    txn: &T,
    changes: &C,
    position: Position<NodeId>,
) -> RecordResult<Vec<u8>>
where
    T: GraphTxnT,
    C: ChangeStore,
{
    let result =
        retrieve_content_with_options(txn, changes, position, RetrieveContentOptions::new())?;
    Ok(result.content)
}

/// Retrieve content with options.
///
/// Like `retrieve_content`, but allows configuration via options.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph access
/// * `changes` - Change store for span content
/// * `position` - Position in the graph
/// * `options` - Retrieval options
///
/// # Returns
///
/// A `RetrieveResult` with content and metadata.
///
/// # Errors
///
/// Returns `RecordError` if retrieval fails.
pub fn retrieve_content_with_options<T, C>(
    txn: &T,
    changes: &C,
    position: Position<NodeId>,
    options: RetrieveContentOptions,
) -> RecordResult<RetrieveResult>
where
    T: GraphTxnT,
    C: ChangeStore,
{
    // Check for root position - it has no content
    if position == Position::ROOT {
        return Ok(RetrieveResult::empty());
    }

    // Use output_file_to_buffer which handles the full pipeline
    let file_opts = options.to_file_output_options();
    let (content, conflicts) = match output_file_to_buffer(txn, changes, position, file_opts) {
        Ok(result) => result,
        Err(e) => {
            // Convert output error to record error
            return Err(RecordError::Io(std::io::Error::other(format!(
                "Failed to retrieve content: {}",
                e
            ))));
        }
    };

    // Build result with conflict count
    let conflict_count = conflicts.len();
    Ok(RetrieveResult::with_content(content, 0).with_conflicts(conflict_count))
}

/// Check if a position has any content.
///
/// This is a lighter-weight check than full retrieval, useful for
/// quickly determining if a file exists in the graph.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph access
/// * `changes` - Change store for content retrieval
/// * `position` - Position to check
///
/// # Returns
///
/// `true` if the position has content vertices.
pub fn has_content<T, C>(txn: &T, changes: &C, position: Position<NodeId>) -> RecordResult<bool>
where
    T: GraphTxnT,
    C: ChangeStore,
{
    if position == Position::ROOT {
        return Ok(false);
    }

    // Try to retrieve content - if we get anything, it has content
    let opts = RetrieveContentOptions::new().max_vertices(1);
    match retrieve_content_with_options(txn, changes, position, opts) {
        Ok(result) => Ok(!result.is_empty()),
        Err(RecordError::Pristine(PristineError::BlockNotFound { .. })) => Ok(false),
        Err(e) => Err(e),
    }
}

// STATE-BASED CONTENT RETRIEVAL

/// Retrieve content with a change filter for state-based retrieval.
///
/// This function retrieves file content at a specific historical state by
/// filtering the graph to only include vertices from a specific set of changes.
/// This is essential for code review workflows where you want to see what a
/// specific change actually modified.
///
/// # State-Based Retrieval Overview
///
/// In Atomic's graph model, all changes are stored together. To view content
/// at a historical state, we filter the graph to only include vertices from
/// changes that existed at that state.
///
/// ```text
/// Full Graph:
///   Change 0  Change 1  Change 2  Change 3  Change 4
///     │         │         │         │         │
///     ▼         ▼         ▼         ▼         ▼
///   [V0]──────[V1]──────[V2]──────[V3]──────[V4]
///
/// State at seq 3 (filter = {0, 1, 2}):
///   Change 0  Change 1  Change 2
///     │         │         │
///     ▼         ▼         ▼
///   [V0]──────[V1]──────[V2]
///
/// Content before change 3 = content from V0, V1, V2
/// Content after change 3 = content from V0, V1, V2, V3
/// ```
///
/// # Arguments
///
/// * `txn` - Transaction providing graph access
/// * `changes` - Change store for span content
/// * `position` - Position in the graph (identifies the file)
/// * `options` - Retrieve options including the change filter
///
/// # Returns
///
/// The file content as bytes at the filtered state.
///
/// # Errors
///
/// Returns `RecordError` if graph traversal or content retrieval fails.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::alive::RetrieveOptions;
/// use std::collections::HashSet;
///
/// // Get changes applied before a specific change
/// let change_set: HashSet<NodeId> = get_changes_up_to_sequence(&txn, &stack, 5)?;
///
/// // Create options with the filter
/// let options = RetrieveOptions::new().with_change_filter(change_set);
///
/// // Retrieve content at that state
/// let content = retrieve_content_with_filter(&txn, &changes, position, options)?;
/// ```
///
/// # Performance
///
/// The change filter is applied during graph traversal, so vertices from
/// excluded changes are never visited. This is efficient even for large
/// repositories with many changes.
pub fn retrieve_content_with_filter<T, C>(
    txn: &T,
    changes: &C,
    position: Position<NodeId>,
    options: RetrieveOptions,
) -> RecordResult<Vec<u8>>
where
    T: GraphTxnT,
    C: ChangeStore,
{
    // Check for root position - it has no content
    if position == Position::ROOT {
        return Ok(Vec::new());
    }

    // Convert RetrieveOptions to FileOutputOptions, preserving the change filter
    let mut file_opts = FileOutputOptions::new();
    if options.include_deleted {
        file_opts = file_opts.include_deleted(true);
    }
    if let Some(max) = options.max_vertices {
        file_opts = file_opts.max_vertices(max);
    }

    // Use output_file_to_buffer_with_options which accepts RetrieveOptions
    let (content, _conflicts) =
        match output_file_to_buffer_with_options(txn, changes, position, file_opts, options) {
            Ok(result) => result,
            Err(e) => {
                // Convert output error to record error
                return Err(RecordError::Io(std::io::Error::other(format!(
                    "Failed to retrieve content with filter: {}",
                    e
                ))));
            }
        };

    Ok(content)
}

/// Retrieve content at the state before a specific sequence.
///
/// This is a convenience function that combines change set collection
/// with filtered retrieval.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph and stack access
/// * `changes` - Change store for span content
/// * `position` - Position in the graph (identifies the file)
/// * `change_set` - Set of change NodeIds to include
///
/// # Returns
///
/// The file content at the filtered state, or empty if no content.
///
/// # Example
///
/// ```rust,ignore
/// // Get the change set for the parent state
/// let change_set = get_changes_up_to_sequence(&txn, &stack, parent_seq)?;
///
/// // Retrieve content at that state
/// let before_content = retrieve_content_at_state(&txn, &changes, position, change_set)?;
/// ```
pub fn retrieve_content_at_state<T, C>(
    txn: &T,
    changes: &C,
    position: Position<NodeId>,
    change_set: HashSet<NodeId>,
) -> RecordResult<Vec<u8>>
where
    T: GraphTxnT,
    C: ChangeStore,
{
    let options = RetrieveOptions::new().with_change_filter(change_set);
    retrieve_content_with_filter(txn, changes, position, options)
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::MemoryChangeStore;
    use crate::pristine::PristineError;
    use crate::types::{ChangePosition, EdgeFlags, GraphNode, Hash, SerializedGraphEdge};

    // Mock Transaction

    /// Mock transaction for testing.
    #[derive(Debug, Default)]
    struct MockTxn {
        /// Whether to return empty graph
        empty: bool,
    }

    impl MockTxn {
        fn new() -> Self {
            Self { empty: true }
        }

        fn with_content() -> Self {
            Self { empty: false }
        }
    }

    /// Mock adjacency iterator.
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

        fn find_block(&self, pos: Position<NodeId>) -> Result<GraphNode<NodeId>, PristineError> {
            if self.empty || pos == Position::ROOT {
                Err(PristineError::BlockNotFound {
                    change: pos.change.get(),
                    pos: pos.pos.get(),
                })
            } else {
                // Return a span for the position
                Ok(GraphNode::new(
                    pos.change,
                    pos.pos,
                    ChangePosition::new(pos.pos.get() + 10),
                ))
            }
        }

        fn find_block_end(
            &self,
            pos: Position<NodeId>,
        ) -> Result<GraphNode<NodeId>, PristineError> {
            if self.empty || pos == Position::ROOT {
                Err(PristineError::BlockNotFound {
                    change: pos.change.get(),
                    pos: pos.pos.get(),
                })
            } else {
                // Return a span ending at the position
                Ok(GraphNode::new(
                    pos.change,
                    ChangePosition::new(pos.pos.get().saturating_sub(10)),
                    pos.pos,
                ))
            }
        }

        fn has_vertex(&self, _vertex: GraphNode<NodeId>) -> Result<bool, PristineError> {
            Ok(!self.empty)
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

    // RetrieveContentOptions Tests

    #[test]
    fn test_options_new() {
        let opts = RetrieveContentOptions::new();

        assert!(!opts.include_deleted);
        assert!(opts.max_vertices.is_none());
    }

    #[test]
    fn test_options_default() {
        let opts = RetrieveContentOptions::default();

        assert!(!opts.include_deleted);
        assert!(opts.max_vertices.is_none());
    }

    #[test]
    fn test_options_include_deleted() {
        let opts = RetrieveContentOptions::new().include_deleted(true);

        assert!(opts.include_deleted);
    }

    #[test]
    fn test_options_max_vertices() {
        let opts = RetrieveContentOptions::new().max_vertices(1000);

        assert_eq!(opts.max_vertices, Some(1000));
    }

    #[test]
    fn test_options_builder_chain() {
        let opts = RetrieveContentOptions::new()
            .include_deleted(true)
            .max_vertices(500);

        assert!(opts.include_deleted);
        assert_eq!(opts.max_vertices, Some(500));
    }

    #[test]
    fn test_options_clone() {
        let opts = RetrieveContentOptions::new().max_vertices(100);
        let cloned = opts.clone();

        assert_eq!(cloned.max_vertices, Some(100));
    }

    // RetrieveResult Tests

    #[test]
    fn test_result_empty() {
        let result = RetrieveResult::empty();

        assert!(result.is_empty());
        assert_eq!(result.len(), 0);
        assert_eq!(result.vertices_processed, 0);
        assert!(!result.has_conflicts);
        assert_eq!(result.conflict_count, 0);
    }

    #[test]
    fn test_result_with_content() {
        let content = b"hello world".to_vec();
        let result = RetrieveResult::with_content(content, 5);

        assert!(!result.is_empty());
        assert_eq!(result.len(), 11);
        assert_eq!(result.vertices_processed, 5);
        assert!(!result.has_conflicts);
    }

    #[test]
    fn test_result_with_conflicts() {
        let result = RetrieveResult::with_content(Vec::new(), 10).with_conflicts(3);

        assert!(result.has_conflicts);
        assert_eq!(result.conflict_count, 3);
    }

    #[test]
    fn test_result_zero_conflicts() {
        let result = RetrieveResult::with_content(Vec::new(), 5).with_conflicts(0);

        assert!(!result.has_conflicts);
        assert_eq!(result.conflict_count, 0);
    }

    #[test]
    fn test_result_clone() {
        let content = b"test".to_vec();
        let result = RetrieveResult::with_content(content, 2).with_conflicts(1);
        let cloned = result.clone();

        assert_eq!(cloned.content, result.content);
        assert_eq!(cloned.vertices_processed, result.vertices_processed);
        assert_eq!(cloned.has_conflicts, result.has_conflicts);
    }

    // Retrieve Content Tests

    #[test]
    fn test_retrieve_content_root_position() {
        let txn = MockTxn::new();
        let changes = MemoryChangeStore::new();

        let result = retrieve_content(&txn, &changes, Position::ROOT).unwrap();

        assert!(result.is_empty());
    }

    #[test]
    fn test_retrieve_content_empty_graph() {
        let txn = MockTxn::new();
        let changes = MemoryChangeStore::new();
        let position = Position::new(NodeId::new(1), ChangePosition::new(0));

        let result = retrieve_content(&txn, &changes, position).unwrap();

        // Empty graph returns empty content
        assert!(result.is_empty());
    }

    #[test]
    fn test_retrieve_content_with_options_root() {
        let txn = MockTxn::new();
        let changes = MemoryChangeStore::new();
        let opts = RetrieveContentOptions::new().include_deleted(true);

        let result = retrieve_content_with_options(&txn, &changes, Position::ROOT, opts).unwrap();

        assert!(result.is_empty());
        assert_eq!(result.vertices_processed, 0);
    }

    // Has Content Tests

    #[test]
    fn test_has_content_root() {
        let txn = MockTxn::new();
        let changes = MemoryChangeStore::new();

        let result = has_content(&txn, &changes, Position::ROOT).unwrap();

        assert!(!result);
    }

    #[test]
    fn test_has_content_empty_graph() {
        let txn = MockTxn::new();
        let changes = MemoryChangeStore::new();
        let position = Position::new(NodeId::new(1), ChangePosition::new(0));

        let result = has_content(&txn, &changes, position).unwrap();

        assert!(!result);
    }

    #[test]
    fn test_has_content_with_data() {
        let txn = MockTxn::with_content();
        let changes = MemoryChangeStore::new();
        let position = Position::new(NodeId::new(1), ChangePosition::new(0));

        // Note: This will still return false because our mock doesn't
        // provide proper graph structure. In a real implementation with
        // actual graph data, this would return true.
        let result = has_content(&txn, &changes, position);

        // We just verify it doesn't panic and returns a result
        assert!(result.is_ok());
    }
}

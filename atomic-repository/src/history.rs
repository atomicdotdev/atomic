//! History operations for Atomic VCS
//!
//! This module provides functionality for querying and traversing the history
//! of changes applied to a stack. History in Atomic is fundamentally different
//! from Git: it's not a linked list of commits but an ordered log of changes
//! applied to a view (stack) of the graph.
//!
//! # Overview
//!
//! Each stack maintains an ordered log of changes that have been applied to it.
//! This log is indexed by sequence number and includes Merkle state hashes at
//! each point, enabling efficient synchronization and state verification.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                          Stack History Log                              │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │   Seq   │   Change Hash        │   Merkle State                        │
//! │  ──────┼─────────────────────┼────────────────────────────────────    │
//! │    0   │ ABC123...            │ state_0 = Hash(empty)                  │
//! │    1   │ DEF456...            │ state_1 = Hash(state_0 || DEF456)      │
//! │    2   │ GHI789...            │ state_2 = Hash(state_1 || GHI789)      │
//! │   ...  │ ...                  │ ...                                    │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Concepts
//!
//! - **Sequence Number**: A 0-indexed position in the change log
//! - **Merkle State**: Cumulative hash representing all changes up to a point
//! - **Change Hash**: Content-addressed identifier for a specific change
//!
//! # Usage
//!
//! ```rust,ignore
//! use atomic_repository::{Repository, HistoryOptions};
//!
//! let repo = Repository::open(".")?;
//!
//! // Get forward history
//! let history = repo.log(HistoryOptions::default())?;
//! for entry in history {
//!     println!("#{}: {} (state: {})",
//!         entry.sequence,
//!         entry.hash.to_base32(),
//!         entry.state.to_base32()
//!     );
//! }
//!
//! // Get reverse history (most recent first)
//! let history = repo.reverse_log(HistoryOptions::default())?;
//!
//! // Get changes affecting a specific path
//! let path_history = repo.log_for_path("src/main.rs", HistoryOptions::default())?;
//! ```
//!
//! # Performance
//!
//! History queries are efficient O(k) operations where k is the number of
//! entries requested. The underlying B-tree structure allows cursor-based
//! iteration without loading the entire history into memory.

use atomic_core::change::{Change, ChangeHeader};
use atomic_core::pristine::{StackState, StackTxnT};
use atomic_core::types::{Base32, Hash, Inode, Merkle, NodeId};
use std::fmt;
use thiserror::Error;

// Error Types

/// Result type for history operations.
pub type HistoryResult<T> = Result<T, HistoryError>;

/// Errors that can occur during history operations.
#[derive(Debug, Error)]
pub enum HistoryError {
    /// The specified stack was not found.
    #[error("Stack not found: {name}")]
    StackNotFound {
        /// Name of the missing stack.
        name: String,
    },

    /// The specified sequence number is out of range.
    #[error("Sequence {sequence} out of range (max: {max})")]
    SequenceOutOfRange {
        /// Requested sequence number.
        sequence: u64,
        /// Maximum valid sequence number.
        max: u64,
    },

    /// The specified change was not found.
    #[error("Change not found: {hash}")]
    ChangeNotFound {
        /// Hash of the missing change.
        hash: String,
    },

    /// The specified path was not found.
    #[error("Path not found: {path}")]
    PathNotFound {
        /// Path that was not found.
        path: String,
    },

    /// Database error.
    #[error("Database error: {0}")]
    Database(String),

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// History Entry

/// A single entry in the history log.
///
/// Each entry represents a change that was applied to the stack at a specific
/// point in time. The entry includes:
///
/// - The sequence number (position in the log)
/// - The change's content hash
/// - The Merkle state after applying this change
/// - Optional metadata loaded from the change file
///
/// # Example
///
/// ```rust,ignore
/// let entry = HistoryEntry::new(42, hash, merkle);
/// println!("Change #{}: {}", entry.sequence, entry.hash.to_base32());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// The sequence number of this change in the stack (0-indexed).
    pub sequence: u64,

    /// The content-addressed hash of the change.
    pub hash: Hash,

    /// The Merkle state of the stack after this change was applied.
    pub state: Merkle,

    /// The internal node ID (repository-local identifier).
    pub node_id: NodeId,

    /// Optional change header metadata (loaded on demand).
    pub header: Option<ChangeHeader>,

    /// Whether this change has been tagged.
    pub is_tagged: bool,
}

impl HistoryEntry {
    /// Create a new history entry with minimal information.
    ///
    /// # Arguments
    ///
    /// * `sequence` - The sequence number in the stack log
    /// * `node_id` - The internal node ID
    /// * `hash` - The content hash of the change
    /// * `state` - The Merkle state after this change
    ///
    /// # Returns
    ///
    /// A new `HistoryEntry` with no header loaded.
    pub fn new(sequence: u64, node_id: NodeId, hash: Hash, state: Merkle) -> Self {
        Self {
            sequence,
            node_id,
            hash,
            state,
            header: None,
            is_tagged: false,
        }
    }

    /// Create a history entry with full metadata.
    ///
    /// # Arguments
    ///
    /// * `sequence` - The sequence number in the stack log
    /// * `node_id` - The internal node ID
    /// * `hash` - The content hash of the change
    /// * `state` - The Merkle state after this change
    /// * `header` - The change header with metadata
    /// * `is_tagged` - Whether this change is tagged
    ///
    /// # Returns
    ///
    /// A new `HistoryEntry` with full metadata.
    pub fn with_header(
        sequence: u64,
        node_id: NodeId,
        hash: Hash,
        state: Merkle,
        header: ChangeHeader,
        is_tagged: bool,
    ) -> Self {
        Self {
            sequence,
            node_id,
            hash,
            state,
            header: Some(header),
            is_tagged,
        }
    }

    /// Mark this entry as tagged.
    pub fn with_tagged(mut self, is_tagged: bool) -> Self {
        self.is_tagged = is_tagged;
        self
    }

    /// Attach a header to this entry.
    pub fn with_change_header(mut self, header: ChangeHeader) -> Self {
        self.header = Some(header);
        self
    }

    /// Get the commit message if a header is loaded.
    pub fn message(&self) -> Option<&str> {
        self.header.as_ref().map(|h| h.message.as_str())
    }

    /// Get the description if a header is loaded.
    pub fn description(&self) -> Option<&str> {
        self.header.as_ref().and_then(|h| h.description.as_deref())
    }

    /// Get the timestamp if a header is loaded.
    pub fn timestamp(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.header.as_ref().map(|h| h.timestamp)
    }

    /// Get the authors if a header is loaded.
    pub fn authors(&self) -> Option<&[atomic_core::change::Author]> {
        self.header.as_ref().map(|h| h.authors.as_slice())
    }
}

impl fmt::Display for HistoryEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "#{} {} (state: {}{})",
            self.sequence,
            self.hash.to_base32(),
            &self.state.to_base32()[..8],
            if self.is_tagged { " [tagged]" } else { "" }
        )
    }
}

// History Options

/// Options for controlling history queries.
///
/// These options allow you to customize how history is retrieved,
/// including pagination, filtering, and metadata loading.
///
/// # Example
///
/// ```rust,ignore
/// let options = HistoryOptions::default()
///     .from_sequence(10)
///     .limit(50)
///     .load_headers(true);
/// ```
#[derive(Debug, Clone)]
pub struct HistoryOptions {
    /// Starting sequence number (inclusive).
    pub from_sequence: u64,

    /// Maximum number of entries to return (None = unlimited).
    pub limit: Option<usize>,

    /// Whether to load change headers (slower but more info).
    pub load_headers: bool,

    /// Specific stack to query (None = current stack).
    pub stack: Option<String>,

    /// Only include tagged changes.
    pub tagged_only: bool,
}

impl Default for HistoryOptions {
    fn default() -> Self {
        Self {
            from_sequence: 0,
            limit: None,
            load_headers: false,
            stack: None,
            tagged_only: false,
        }
    }
}

impl HistoryOptions {
    /// Create new history options with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the starting sequence number.
    ///
    /// # Arguments
    ///
    /// * `seq` - The sequence number to start from (inclusive)
    pub fn from_sequence(mut self, seq: u64) -> Self {
        self.from_sequence = seq;
        self
    }

    /// Set the maximum number of entries to return.
    ///
    /// # Arguments
    ///
    /// * `n` - Maximum number of entries
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    /// Enable loading of change headers.
    ///
    /// This is slower but provides access to message, authors, etc.
    pub fn load_headers(mut self, load: bool) -> Self {
        self.load_headers = load;
        self
    }

    /// Set the stack to query.
    ///
    /// # Arguments
    ///
    /// * `name` - Stack name (None = current stack)
    pub fn stack(mut self, name: impl Into<String>) -> Self {
        self.stack = Some(name.into());
        self
    }

    /// Only include tagged changes.
    pub fn tagged_only(mut self, tagged: bool) -> Self {
        self.tagged_only = tagged;
        self
    }

    /// Create options for getting the last N changes.
    ///
    /// # Arguments
    ///
    /// * `n` - Number of recent changes to retrieve
    pub fn last(n: usize) -> Self {
        Self::default().limit(n)
    }

    /// Create options with headers loaded.
    pub fn with_headers() -> Self {
        Self::default().load_headers(true)
    }
}

// History Summary

/// Summary statistics about a stack's history.
///
/// Provides quick access to aggregate information without
/// iterating through all entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySummary {
    /// Total number of changes in the stack.
    pub change_count: u64,

    /// Current Merkle state of the stack.
    pub current_state: Merkle,

    /// Hash of the first change (if any).
    pub first_change: Option<Hash>,

    /// Hash of the most recent change (if any).
    pub last_change: Option<Hash>,

    /// Number of tagged changes.
    pub tagged_count: u64,

    /// Stack name.
    pub stack_name: String,
}

impl HistorySummary {
    /// Create a new history summary.
    pub fn new(stack_name: impl Into<String>, stack_state: &StackState) -> Self {
        Self {
            change_count: stack_state.change_count,
            current_state: stack_state.state,
            first_change: None,
            last_change: None,
            tagged_count: 0,
            stack_name: stack_name.into(),
        }
    }

    /// Check if the stack has any changes.
    pub fn is_empty(&self) -> bool {
        self.change_count == 0
    }

    /// Set the first and last change hashes.
    pub fn with_bounds(mut self, first: Option<Hash>, last: Option<Hash>) -> Self {
        self.first_change = first;
        self.last_change = last;
        self
    }

    /// Set the tagged count.
    pub fn with_tagged_count(mut self, count: u64) -> Self {
        self.tagged_count = count;
        self
    }
}

impl fmt::Display for HistorySummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Stack '{}': {} changes (state: {}, {} tagged)",
            self.stack_name,
            self.change_count,
            &self.current_state.to_base32()[..8],
            self.tagged_count
        )
    }
}

// Path History Entry

/// A history entry for a specific path.
///
/// Similar to `HistoryEntry` but includes information about how
/// the change affected the specified path.
#[derive(Debug, Clone)]
pub struct PathHistoryEntry {
    /// The base history entry.
    pub entry: HistoryEntry,

    /// The path this entry relates to.
    pub path: String,

    /// The inode of the file at this point.
    pub inode: Option<Inode>,

    /// Type of modification to the path.
    pub modification_type: PathModificationType,
}

impl PathHistoryEntry {
    /// Create a new path history entry.
    pub fn new(
        entry: HistoryEntry,
        path: impl Into<String>,
        modification_type: PathModificationType,
    ) -> Self {
        Self {
            entry,
            path: path.into(),
            inode: None,
            modification_type,
        }
    }

    /// Set the inode for this entry.
    pub fn with_inode(mut self, inode: Inode) -> Self {
        self.inode = Some(inode);
        self
    }

    /// Get the sequence number.
    pub fn sequence(&self) -> u64 {
        self.entry.sequence
    }

    /// Get the change hash.
    pub fn hash(&self) -> &Hash {
        &self.entry.hash
    }
}

/// The type of modification a change made to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathModificationType {
    /// The file was created.
    Created,

    /// The file was modified.
    Modified,

    /// The file was deleted.
    Deleted,

    /// The file was moved/renamed.
    Moved,

    /// The modification type is unknown.
    Unknown,
}

impl fmt::Display for PathModificationType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Modified => write!(f, "modified"),
            Self::Deleted => write!(f, "deleted"),
            Self::Moved => write!(f, "moved"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

// History Iterator

/// An iterator over history entries.
///
/// This iterator is lazy - it fetches entries as needed from the
/// underlying database cursor.
#[allow(dead_code)]
pub struct HistoryIter<'a, T: StackTxnT> {
    txn: &'a T,
    stack: StackState,
    inner: Box<
        dyn Iterator<Item = Result<(u64, NodeId, Merkle), atomic_core::pristine::PristineError>>
            + 'a,
    >,
    limit: Option<usize>,
    count: usize,
    load_headers: bool,
}

impl<'a, T: StackTxnT> HistoryIter<'a, T> {
    /// Create a new history iterator.
    pub(crate) fn new(
        txn: &'a T,
        stack: StackState,
        inner: Box<
            dyn Iterator<Item = Result<(u64, NodeId, Merkle), atomic_core::pristine::PristineError>>
                + 'a,
        >,
        options: &HistoryOptions,
    ) -> Self {
        Self {
            txn,
            stack,
            inner,
            limit: options.limit,
            count: 0,
            load_headers: options.load_headers,
        }
    }

    /// Get the stack being iterated.
    pub fn stack(&self) -> &StackState {
        &self.stack
    }

    /// Get the number of entries yielded so far.
    pub fn count(&self) -> usize {
        self.count
    }
}

impl<'a, T: StackTxnT> Iterator for HistoryIter<'a, T> {
    type Item = HistoryResult<HistoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        // Check limit
        if let Some(limit) = self.limit {
            if self.count >= limit {
                return None;
            }
        }

        // Get next from inner iterator
        let result = self.inner.next()?;

        self.count += 1;

        match result {
            Ok((seq, node_id, merkle)) => {
                // Get the external hash
                let hash = match self.txn.get_external(node_id) {
                    Ok(Some(h)) => h,
                    Ok(None) => {
                        return Some(Err(HistoryError::ChangeNotFound {
                            hash: format!("{:?}", node_id),
                        }));
                    }
                    Err(e) => {
                        return Some(Err(HistoryError::Database(e.to_string())));
                    }
                };

                let entry = HistoryEntry::new(seq, node_id, hash, merkle);

                // Note: Header loading would be done by the Repository layer
                // since it requires access to the ChangeStore

                Some(Ok(entry))
            }
            Err(e) => Some(Err(HistoryError::Database(e.to_string()))),
        }
    }
}

// Log Functions

/// Get forward history log from a stack.
///
/// Returns an iterator over history entries starting from the given
/// sequence number and proceeding forward (oldest to newest).
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to query
/// * `options` - Query options
///
/// # Returns
///
/// An iterator yielding `HistoryEntry` items.
///
/// # Example
///
/// ```rust,ignore
/// let iter = log(&txn, &stack, &HistoryOptions::default())?;
/// for entry in iter {
///     let entry = entry?;
///     println!("{}", entry);
/// }
/// ```
pub fn log<'a, T: StackTxnT>(
    txn: &'a T,
    stack: &StackState,
    options: &HistoryOptions,
) -> HistoryResult<HistoryIter<'a, T>> {
    let inner = txn
        .iter_changes(stack, options.from_sequence)
        .map_err(|e| HistoryError::Database(e.to_string()))?;

    Ok(HistoryIter::new(txn, stack.clone(), inner, options))
}

/// Get reverse history log from a stack.
///
/// Returns entries in reverse order (newest to oldest), starting from
/// either the most recent change or a specified sequence number.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to query
/// * `options` - Query options (from_sequence is the upper bound)
///
/// # Returns
///
/// A vector of history entries in reverse order.
///
/// # Note
///
/// This function collects entries into a vector and reverses them,
/// as the underlying iterator only supports forward iteration.
/// For large histories, consider using `log()` with pagination.
pub fn reverse_log<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    options: &HistoryOptions,
) -> HistoryResult<Vec<HistoryEntry>> {
    // Determine the range to query
    let end_seq = if options.from_sequence > 0 {
        options.from_sequence
    } else {
        stack.change_count
    };

    let start_seq = if let Some(limit) = options.limit {
        end_seq.saturating_sub(limit as u64)
    } else {
        0
    };

    // Collect forward and reverse
    let forward_options = HistoryOptions {
        from_sequence: start_seq,
        limit: options.limit,
        load_headers: options.load_headers,
        stack: options.stack.clone(),
        tagged_only: options.tagged_only,
    };

    let iter = log(txn, stack, &forward_options)?;
    let mut entries: Vec<HistoryEntry> = iter.filter_map(|r| r.ok()).collect();
    entries.reverse();

    Ok(entries)
}

/// Get a summary of the stack's history.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to summarize
///
/// # Returns
///
/// A `HistorySummary` with aggregate statistics.
pub fn history_summary<T: StackTxnT>(txn: &T, stack: &StackState) -> HistoryResult<HistorySummary> {
    let mut summary = HistorySummary::new(&stack.name, stack);

    // Get first change
    if stack.change_count > 0 {
        if let Ok(Some(first_id)) = txn.get_change_at_seq(stack, 0) {
            if let Ok(Some(hash)) = txn.get_external(first_id) {
                summary.first_change = Some(hash);
            }
        }

        // Get last change
        if let Ok(Some(last_id)) = txn.get_change_at_seq(stack, stack.change_count - 1) {
            if let Ok(Some(hash)) = txn.get_external(last_id) {
                summary.last_change = Some(hash);
            }
        }
    }

    // Note: Tagged count would require iterating through the history
    // or maintaining a separate counter. For now, we leave it at 0.

    Ok(summary)
}

/// Get a specific change by sequence number.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to query
/// * `sequence` - Sequence number to retrieve
///
/// # Returns
///
/// The history entry at the given sequence, or an error if out of range.
pub fn get_change_at_sequence<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    sequence: u64,
) -> HistoryResult<HistoryEntry> {
    if sequence >= stack.change_count {
        return Err(HistoryError::SequenceOutOfRange {
            sequence,
            max: stack.change_count.saturating_sub(1),
        });
    }

    let node_id = txn
        .get_change_at_seq(stack, sequence)
        .map_err(|e| HistoryError::Database(e.to_string()))?
        .ok_or_else(|| HistoryError::SequenceOutOfRange {
            sequence,
            max: stack.change_count.saturating_sub(1),
        })?;

    let hash = txn
        .get_external(node_id)
        .map_err(|e| HistoryError::Database(e.to_string()))?
        .ok_or_else(|| HistoryError::ChangeNotFound {
            hash: format!("{:?}", node_id),
        })?;

    // Get the Merkle state - we need to iterate to find it
    // This is a bit inefficient but maintains correctness
    let iter = txn
        .iter_changes(stack, sequence)
        .map_err(|e| HistoryError::Database(e.to_string()))?;

    for result in iter {
        match result {
            Ok((seq, id, merkle)) if seq == sequence && id == node_id => {
                return Ok(HistoryEntry::new(sequence, node_id, hash, merkle));
            }
            Ok((seq, _, _)) if seq > sequence => break,
            Err(e) => return Err(HistoryError::Database(e.to_string())),
            _ => continue,
        }
    }

    // Fallback with zero merkle if we couldn't find it
    Ok(HistoryEntry::new(sequence, node_id, hash, Merkle::ZERO))
}

/// Find the sequence number for a change by its hash.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to search
/// * `hash` - Hash of the change to find
///
/// # Returns
///
/// The sequence number if found, or None if the change is not in the stack.
pub fn find_change_sequence<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    hash: &Hash,
) -> HistoryResult<Option<u64>> {
    // First get the internal ID
    let node_id = match txn.get_internal(hash) {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(None),
        Err(e) => return Err(HistoryError::Database(e.to_string())),
    };

    // Then look up the sequence
    match txn.get_change_seq(stack, node_id) {
        Ok(seq) => Ok(seq),
        Err(e) => Err(HistoryError::Database(e.to_string())),
    }
}

/// Check if a change is in the stack's history.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - Stack to search
/// * `hash` - Hash of the change to check
///
/// # Returns
///
/// `true` if the change is in the stack's history.
pub fn is_change_in_history<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    hash: &Hash,
) -> HistoryResult<bool> {
    find_change_sequence(txn, stack, hash).map(|seq| seq.is_some())
}

// STATE-BASED CONTENT RETRIEVAL

/// Information about the state before a change was applied.
///
/// This struct contains all the information needed to retrieve file content
/// at the state immediately before a specific change was applied to the stack.
///
/// # Use Case
///
/// This is primarily used for code review workflows where you want to see
/// what a specific change actually changed:
///
/// ```text
/// Before State (sequence N-1)          After State (sequence N)
/// ┌─────────────────────────┐         ┌─────────────────────────┐
/// │ File content as it was  │  ───▶   │ File content after the  │
/// │ before the change       │ Change  │ change was applied      │
/// └─────────────────────────┘         └─────────────────────────┘
/// ```
///
/// By comparing content at these two states, we can show exactly what
/// the change modified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateBeforeChange {
    /// The sequence number of the parent state (N-1 for a change at sequence N).
    /// This is `None` if the change is at sequence 0 (the first change).
    pub parent_sequence: Option<u64>,

    /// The Merkle state hash before the change was applied.
    /// This is `Merkle::ZERO` if the change is at sequence 0.
    pub parent_state: Merkle,

    /// The sequence number of the change itself.
    pub change_sequence: u64,

    /// The Merkle state hash after the change was applied.
    pub change_state: Merkle,
}

impl StateBeforeChange {
    /// Create a new StateBeforeChange.
    ///
    /// # Arguments
    ///
    /// * `parent_sequence` - Sequence of parent state, or None if first change
    /// * `parent_state` - Merkle hash of parent state
    /// * `change_sequence` - Sequence of the change
    /// * `change_state` - Merkle hash after the change
    pub fn new(
        parent_sequence: Option<u64>,
        parent_state: Merkle,
        change_sequence: u64,
        change_state: Merkle,
    ) -> Self {
        Self {
            parent_sequence,
            parent_state,
            change_sequence,
            change_state,
        }
    }

    /// Check if this is the first change in the stack.
    ///
    /// If true, the parent state is empty (no content existed before).
    pub fn is_first_change(&self) -> bool {
        self.parent_sequence.is_none()
    }

    /// Get the maximum sequence number to include when retrieving parent state content.
    ///
    /// This returns the exclusive upper bound for change sequences that should
    /// be included when retrieving file content at the parent state.
    ///
    /// # Returns
    ///
    /// - `0` if this is the first change (no changes should be included)
    /// - `parent_sequence + 1` otherwise (includes all changes up to and including parent)
    pub fn parent_max_sequence_exclusive(&self) -> u64 {
        match self.parent_sequence {
            None => 0,
            Some(seq) => seq + 1,
        }
    }
}

impl std::fmt::Display for StateBeforeChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.parent_sequence {
            None => write!(
                f,
                "First change (seq {}) → state {}",
                self.change_sequence,
                &self.change_state.to_base32()[..8]
            ),
            Some(parent_seq) => write!(
                f,
                "State {} (seq {}) → state {} (seq {})",
                &self.parent_state.to_base32()[..8],
                parent_seq,
                &self.change_state.to_base32()[..8],
                self.change_sequence
            ),
        }
    }
}

/// Get the state immediately before a change was applied.
///
/// This function finds the Merkle state of the stack as it was just before
/// the specified change was applied. This is essential for showing what
/// a specific change actually modified.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - The stack containing the change
/// * `change_hash` - Hash of the change to find the parent state for
///
/// # Returns
///
/// * `Ok(Some(state))` - The state information before the change
/// * `Ok(None)` - The change is not in this stack's history
/// * `Err(_)` - Database error
///
/// # Example
///
/// ```rust,ignore
/// use atomic_repository::history::{get_state_before_change, StateBeforeChange};
///
/// let state_info = get_state_before_change(&txn, &stack, &change_hash)?;
/// if let Some(info) = state_info {
///     println!("Change at sequence {}", info.change_sequence);
///     if info.is_first_change() {
///         println!("This is the first change - no parent content");
///     } else {
///         println!("Parent state: {}", info.parent_state.to_base32());
///     }
/// }
/// ```
///
/// # Performance
///
/// This function performs:
/// - One hash-to-internal-ID lookup
/// - One sequence lookup
/// - One or two iterations over the change log (to find parent and current states)
///
/// Total: O(log n) where n is the number of changes in the stack.
pub fn get_state_before_change<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    change_hash: &Hash,
) -> HistoryResult<Option<StateBeforeChange>> {
    // First, find the sequence number for this change
    let change_sequence = match find_change_sequence(txn, stack, change_hash)? {
        Some(seq) => seq,
        None => return Ok(None), // Change not in this stack
    };

    // Get the change's own state (state after it was applied)
    let change_entry = get_change_at_sequence(txn, stack, change_sequence)?;
    let change_state = change_entry.state;

    // If this is the first change (sequence 0), there's no parent state
    if change_sequence == 0 {
        return Ok(Some(StateBeforeChange::new(
            None,
            Merkle::ZERO,
            change_sequence,
            change_state,
        )));
    }

    // Get the parent state (state at sequence - 1)
    let parent_sequence = change_sequence - 1;
    let parent_entry = get_change_at_sequence(txn, stack, parent_sequence)?;
    let parent_state = parent_entry.state;

    Ok(Some(StateBeforeChange::new(
        Some(parent_sequence),
        parent_state,
        change_sequence,
        change_state,
    )))
}

/// Get all change NodeIds applied up to (but not including) a given sequence.
///
/// This function returns a set of internal NodeIds for all changes that were
/// applied to the stack before the specified sequence number. This set can
/// be used to filter graph retrieval to only include content from those changes.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - The stack to query
/// * `max_sequence` - Exclusive upper bound (changes with seq < max_sequence are included)
///
/// # Returns
///
/// A `HashSet<NodeId>` containing all changes applied before `max_sequence`.
/// Returns an empty set if `max_sequence` is 0.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_repository::history::get_changes_up_to_sequence;
/// use std::collections::HashSet;
///
/// // Get all changes applied before sequence 5
/// let change_set = get_changes_up_to_sequence(&txn, &stack, 5)?;
/// println!("Found {} changes before sequence 5", change_set.len());
///
/// // Use this set to filter graph retrieval
/// let options = RetrieveOptions::new().with_change_filter(change_set);
/// let graph = retrieve_graph(&txn, position, options)?;
/// ```
///
/// # Performance
///
/// This function iterates over all changes from sequence 0 to `max_sequence - 1`.
/// Time complexity: O(max_sequence).
///
/// For large repositories, consider caching the result if you need to retrieve
/// content for multiple files at the same state.
pub fn get_changes_up_to_sequence<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    max_sequence: u64,
) -> HistoryResult<std::collections::HashSet<NodeId>> {
    use std::collections::HashSet;

    let mut change_set = HashSet::new();

    // Early return for sequence 0 - no changes to include
    if max_sequence == 0 {
        return Ok(change_set);
    }

    // Iterate from sequence 0 up to (but not including) max_sequence
    let iter = txn
        .iter_changes(stack, 0)
        .map_err(|e| HistoryError::Database(e.to_string()))?;

    for result in iter {
        let (seq, node_id, _merkle) = result.map_err(|e| HistoryError::Database(e.to_string()))?;

        // Stop if we've reached or passed max_sequence
        if seq >= max_sequence {
            break;
        }

        change_set.insert(node_id);
    }

    Ok(change_set)
}

/// Get all change NodeIds applied up to and including a specific change.
///
/// This is a convenience wrapper around [`get_changes_up_to_sequence`] that
/// includes the specified change in the result set.
///
/// # Arguments
///
/// * `txn` - Read transaction
/// * `stack` - The stack to query
/// * `change_hash` - Hash of the change to include (and all before it)
///
/// # Returns
///
/// * `Ok(Some(set))` - Set of changes including the specified change and all before it
/// * `Ok(None)` - The change is not in this stack's history
/// * `Err(_)` - Database error
///
/// # Example
///
/// ```rust,ignore
/// // Get all changes up to and including a specific change
/// if let Some(change_set) = get_changes_up_to_change(&txn, &stack, &change_hash)? {
///     println!("State includes {} changes", change_set.len());
/// }
/// ```
pub fn get_changes_up_to_change<T: StackTxnT>(
    txn: &T,
    stack: &StackState,
    change_hash: &Hash,
) -> HistoryResult<Option<std::collections::HashSet<NodeId>>> {
    // Find the sequence number for this change
    let sequence = match find_change_sequence(txn, stack, change_hash)? {
        Some(seq) => seq,
        None => return Ok(None),
    };

    // Get all changes up to and including this sequence
    let change_set = get_changes_up_to_sequence(txn, stack, sequence + 1)?;

    Ok(Some(change_set))
}

/// Get the files modified by a specific change.
///
/// This function returns the paths of all files that were added, modified,
/// or deleted by a specific change. It examines the change's hunks to
/// determine which files were affected.
///
/// # Arguments
///
/// * `change` - The change to examine
///
/// # Returns
///
/// A vector of file paths that were modified by the change.
///
/// # Example
///
/// ```rust,ignore
/// let change = repo.load_change(&hash)?;
/// let modified_files = get_files_in_change(&change);
/// for path in modified_files {
///     println!("Modified: {}", path);
/// }
/// ```
pub fn get_files_in_change(change: &Change) -> Vec<String> {
    use std::collections::HashSet;

    let mut files: HashSet<String> = HashSet::new();

    for graph_op in change.hunks() {
        if let Some(path) = graph_op.path() {
            files.insert(path.to_string());
        }
    }

    files.into_iter().collect()
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // StateBeforeChange Tests

    #[test]
    fn test_state_before_change_new() {
        let parent_state = Merkle::of(b"parent");
        let change_state = Merkle::of(b"change");

        let state_info = StateBeforeChange::new(Some(5), parent_state, 6, change_state);

        assert_eq!(state_info.parent_sequence, Some(5));
        assert_eq!(state_info.parent_state, parent_state);
        assert_eq!(state_info.change_sequence, 6);
        assert_eq!(state_info.change_state, change_state);
    }

    #[test]
    fn test_state_before_change_first_change() {
        let change_state = Merkle::of(b"first");

        let state_info = StateBeforeChange::new(None, Merkle::ZERO, 0, change_state);

        assert!(state_info.is_first_change());
        assert_eq!(state_info.parent_sequence, None);
        assert_eq!(state_info.parent_state, Merkle::ZERO);
    }

    #[test]
    fn test_state_before_change_not_first() {
        let parent_state = Merkle::of(b"parent");
        let change_state = Merkle::of(b"change");

        let state_info = StateBeforeChange::new(Some(10), parent_state, 11, change_state);

        assert!(!state_info.is_first_change());
    }

    #[test]
    fn test_state_before_change_parent_max_sequence_first() {
        let state_info = StateBeforeChange::new(None, Merkle::ZERO, 0, Merkle::of(b"first"));

        // First change has no parent, so max sequence should be 0
        assert_eq!(state_info.parent_max_sequence_exclusive(), 0);
    }

    #[test]
    fn test_state_before_change_parent_max_sequence_later() {
        let state_info =
            StateBeforeChange::new(Some(5), Merkle::of(b"parent"), 6, Merkle::of(b"change"));

        // Parent at sequence 5, so max exclusive is 6 (includes 0-5)
        assert_eq!(state_info.parent_max_sequence_exclusive(), 6);
    }

    #[test]
    fn test_state_before_change_display_first() {
        let state_info = StateBeforeChange::new(None, Merkle::ZERO, 0, Merkle::of(b"first"));

        let display = format!("{}", state_info);
        assert!(display.contains("First change"));
        assert!(display.contains("seq 0"));
    }

    #[test]
    fn test_state_before_change_display_later() {
        let parent_state = Merkle::of(b"parent");
        let change_state = Merkle::of(b"change");

        let state_info = StateBeforeChange::new(Some(5), parent_state, 6, change_state);

        let display = format!("{}", state_info);
        assert!(display.contains("seq 5"));
        assert!(display.contains("seq 6"));
    }

    #[test]
    fn test_state_before_change_equality() {
        let state1 = StateBeforeChange::new(Some(1), Merkle::of(b"a"), 2, Merkle::of(b"b"));
        let state2 = StateBeforeChange::new(Some(1), Merkle::of(b"a"), 2, Merkle::of(b"b"));
        let state3 = StateBeforeChange::new(Some(2), Merkle::of(b"a"), 3, Merkle::of(b"b"));

        assert_eq!(state1, state2);
        assert_ne!(state1, state3);
    }

    #[test]
    fn test_state_before_change_clone() {
        let original = StateBeforeChange::new(Some(1), Merkle::of(b"a"), 2, Merkle::of(b"b"));
        let cloned = original.clone();

        assert_eq!(original, cloned);
    }

    #[test]
    fn test_state_before_change_debug() {
        let state_info = StateBeforeChange::new(Some(1), Merkle::of(b"a"), 2, Merkle::of(b"b"));
        let debug = format!("{:?}", state_info);

        assert!(debug.contains("StateBeforeChange"));
        assert!(debug.contains("parent_sequence"));
    }

    // Existing Tests (below this line)

    use super::*;

    // HistoryEntry Tests

    #[test]
    fn test_history_entry_new() {
        let hash = Hash::of(b"test change");
        let state = Merkle::of(b"test state");
        let entry = HistoryEntry::new(42, NodeId::new(1), hash, state);

        assert_eq!(entry.sequence, 42);
        assert_eq!(entry.node_id, NodeId::new(1));
        assert_eq!(entry.hash, hash);
        assert_eq!(entry.state, state);
        assert!(entry.header.is_none());
        assert!(!entry.is_tagged);
    }

    #[test]
    fn test_history_entry_with_header() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let header = ChangeHeader::default();
        let entry = HistoryEntry::with_header(1, NodeId::new(2), hash, state, header.clone(), true);

        assert_eq!(entry.sequence, 1);
        assert!(entry.header.is_some());
        assert!(entry.is_tagged);
    }

    #[test]
    fn test_history_entry_builder_pattern() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let header = ChangeHeader::default();

        let entry = HistoryEntry::new(0, NodeId::new(1), hash, state)
            .with_tagged(true)
            .with_change_header(header);

        assert!(entry.is_tagged);
        assert!(entry.header.is_some());
    }

    #[test]
    fn test_history_entry_accessors() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let mut header = ChangeHeader::default();
        header.message = "Test message".to_string();
        header.description = Some("Test description".to_string());

        let entry = HistoryEntry::with_header(0, NodeId::new(1), hash, state, header, false);

        assert_eq!(entry.message(), Some("Test message"));
        assert_eq!(entry.description(), Some("Test description"));
        assert!(entry.timestamp().is_some());
        assert!(entry.authors().is_some());
    }

    #[test]
    fn test_history_entry_no_header_accessors() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let entry = HistoryEntry::new(0, NodeId::new(1), hash, state);

        assert!(entry.message().is_none());
        assert!(entry.description().is_none());
        assert!(entry.timestamp().is_none());
        assert!(entry.authors().is_none());
    }

    #[test]
    fn test_history_entry_display() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let entry = HistoryEntry::new(5, NodeId::new(1), hash, state);

        let display = format!("{}", entry);
        assert!(display.contains("#5"));
        assert!(display.contains("state:"));
    }

    #[test]
    fn test_history_entry_display_tagged() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let entry = HistoryEntry::new(5, NodeId::new(1), hash, state).with_tagged(true);

        let display = format!("{}", entry);
        assert!(display.contains("[tagged]"));
    }

    #[test]
    fn test_history_entry_equality() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let entry1 = HistoryEntry::new(5, NodeId::new(1), hash, state);
        let entry2 = HistoryEntry::new(5, NodeId::new(1), hash, state);

        assert_eq!(entry1, entry2);
    }

    // HistoryOptions Tests

    #[test]
    fn test_history_options_default() {
        let options = HistoryOptions::default();

        assert_eq!(options.from_sequence, 0);
        assert!(options.limit.is_none());
        assert!(!options.load_headers);
        assert!(options.stack.is_none());
        assert!(!options.tagged_only);
    }

    #[test]
    fn test_history_options_builder() {
        let options = HistoryOptions::new()
            .from_sequence(10)
            .limit(50)
            .load_headers(true)
            .stack("feature")
            .tagged_only(true);

        assert_eq!(options.from_sequence, 10);
        assert_eq!(options.limit, Some(50));
        assert!(options.load_headers);
        assert_eq!(options.stack, Some("feature".to_string()));
        assert!(options.tagged_only);
    }

    #[test]
    fn test_history_options_last() {
        let options = HistoryOptions::last(10);

        assert_eq!(options.from_sequence, 0);
        assert_eq!(options.limit, Some(10));
    }

    #[test]
    fn test_history_options_with_headers() {
        let options = HistoryOptions::with_headers();

        assert!(options.load_headers);
    }

    // HistorySummary Tests

    #[test]
    fn test_history_summary_new() {
        let stack_state = StackState::new(1, "main".to_string());
        let summary = HistorySummary::new("main", &stack_state);

        assert_eq!(summary.stack_name, "main");
        assert_eq!(summary.change_count, 0);
        assert!(summary.first_change.is_none());
        assert!(summary.last_change.is_none());
    }

    #[test]
    fn test_history_summary_is_empty() {
        let stack_state = StackState::new(1, "main".to_string());
        let summary = HistorySummary::new("main", &stack_state);

        assert!(summary.is_empty());
    }

    #[test]
    fn test_history_summary_with_bounds() {
        let stack_state = StackState::new(1, "main".to_string());
        let first = Hash::of(b"first");
        let last = Hash::of(b"last");

        let summary =
            HistorySummary::new("main", &stack_state).with_bounds(Some(first), Some(last));

        assert_eq!(summary.first_change, Some(first));
        assert_eq!(summary.last_change, Some(last));
    }

    #[test]
    fn test_history_summary_with_tagged_count() {
        let stack_state = StackState::new(1, "main".to_string());
        let summary = HistorySummary::new("main", &stack_state).with_tagged_count(5);

        assert_eq!(summary.tagged_count, 5);
    }

    #[test]
    fn test_history_summary_display() {
        let stack_state = StackState::new(1, "main".to_string());
        let summary = HistorySummary::new("main", &stack_state).with_tagged_count(3);

        let display = format!("{}", summary);
        assert!(display.contains("main"));
        assert!(display.contains("0 changes"));
        assert!(display.contains("3 tagged"));
    }

    // PathHistoryEntry Tests

    #[test]
    fn test_path_history_entry_new() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let entry = HistoryEntry::new(1, NodeId::new(1), hash, state);
        let path_entry =
            PathHistoryEntry::new(entry, "src/main.rs", PathModificationType::Modified);

        assert_eq!(path_entry.path, "src/main.rs");
        assert_eq!(path_entry.modification_type, PathModificationType::Modified);
        assert!(path_entry.inode.is_none());
    }

    #[test]
    fn test_path_history_entry_with_inode() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let entry = HistoryEntry::new(1, NodeId::new(1), hash, state);
        let path_entry = PathHistoryEntry::new(entry, "src/main.rs", PathModificationType::Created)
            .with_inode(Inode::new(42));

        assert_eq!(path_entry.inode, Some(Inode::new(42)));
    }

    #[test]
    fn test_path_history_entry_accessors() {
        let hash = Hash::of(b"test");
        let state = Merkle::of(b"state");
        let entry = HistoryEntry::new(5, NodeId::new(1), hash, state);
        let path_entry =
            PathHistoryEntry::new(entry, "src/main.rs", PathModificationType::Modified);

        assert_eq!(path_entry.sequence(), 5);
        assert_eq!(*path_entry.hash(), hash);
    }

    // PathModificationType Tests

    #[test]
    fn test_path_modification_type_display() {
        assert_eq!(format!("{}", PathModificationType::Created), "created");
        assert_eq!(format!("{}", PathModificationType::Modified), "modified");
        assert_eq!(format!("{}", PathModificationType::Deleted), "deleted");
        assert_eq!(format!("{}", PathModificationType::Moved), "moved");
        assert_eq!(format!("{}", PathModificationType::Unknown), "unknown");
    }

    #[test]
    fn test_path_modification_type_equality() {
        assert_eq!(PathModificationType::Created, PathModificationType::Created);
        assert_ne!(
            PathModificationType::Created,
            PathModificationType::Modified
        );
    }

    // HistoryError Tests

    #[test]
    fn test_history_error_stack_not_found() {
        let error = HistoryError::StackNotFound {
            name: "missing".to_string(),
        };
        let msg = format!("{}", error);
        assert!(msg.contains("missing"));
    }

    #[test]
    fn test_history_error_sequence_out_of_range() {
        let error = HistoryError::SequenceOutOfRange {
            sequence: 100,
            max: 50,
        };
        let msg = format!("{}", error);
        assert!(msg.contains("100"));
        assert!(msg.contains("50"));
    }

    #[test]
    fn test_history_error_change_not_found() {
        let error = HistoryError::ChangeNotFound {
            hash: "ABC123".to_string(),
        };
        let msg = format!("{}", error);
        assert!(msg.contains("ABC123"));
    }

    #[test]
    fn test_history_error_path_not_found() {
        let error = HistoryError::PathNotFound {
            path: "src/missing.rs".to_string(),
        };
        let msg = format!("{}", error);
        assert!(msg.contains("src/missing.rs"));
    }

    #[test]
    fn test_history_error_database() {
        let error = HistoryError::Database("connection failed".to_string());
        let msg = format!("{}", error);
        assert!(msg.contains("connection failed"));
    }
}

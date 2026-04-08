//! History types and error definitions.
//!
//! This module contains the core data types used by the history subsystem:
//! [`HistoryEntry`], [`HistoryOptions`], [`HistorySummary`], [`PathHistoryEntry`],
//! and the [`HistoryError`] type.

use atomic_core::change::ChangeHeader;
use atomic_core::pristine::ViewState;
use atomic_core::types::{Base32, Hash, Inode, Merkle, NodeId};
use std::fmt;
use thiserror::Error;

// Error Types

/// Result type for history operations.
pub type HistoryResult<T> = Result<T, HistoryError>;

/// Errors that can occur during history operations.
#[derive(Debug, Error)]
pub enum HistoryError {
    /// The specified view was not found.
    #[error("View not found: {name}")]
    ViewNotFound {
        /// Name of the missing view.
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
/// Each entry represents a change that was applied to the view at a specific
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
    /// The sequence number of this change in the view (0-indexed).
    pub sequence: u64,

    /// The content-addressed hash of the change.
    pub hash: Hash,

    /// The Merkle state of the view after this change was applied.
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
    /// * `sequence` - The sequence number in the view log
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
    /// * `sequence` - The sequence number in the view log
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
#[derive(Debug, Clone, Default)]
pub struct HistoryOptions {
    /// Starting sequence number (inclusive).
    pub from_sequence: u64,

    /// Maximum number of entries to return (None = unlimited).
    pub limit: Option<usize>,

    /// Whether to load change headers (slower but more info).
    pub load_headers: bool,

    /// Specific view to query (None = current view).
    pub view: Option<String>,

    /// Only include tagged changes.
    pub tagged_only: bool,
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

    /// Set the view to query.
    ///
    /// # Arguments
    ///
    /// * `name` - View name (None = current view)
    pub fn view(mut self, name: impl Into<String>) -> Self {
        self.view = Some(name.into());
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

/// Summary statistics about a view's history.
///
/// Provides quick access to aggregate information without
/// iterating through all entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySummary {
    /// Total number of changes in the view.
    pub change_count: u64,

    /// Current Merkle state of the view.
    pub current_state: Merkle,

    /// Hash of the first change (if any).
    pub first_change: Option<Hash>,

    /// Hash of the most recent change (if any).
    pub last_change: Option<Hash>,

    /// Number of tagged changes.
    pub tagged_count: u64,

    /// View name.
    pub view_name: String,
}

impl HistorySummary {
    /// Create a new history summary.
    pub fn new(view_name: impl Into<String>, view_state: &ViewState) -> Self {
        Self {
            change_count: view_state.change_count,
            current_state: view_state.state,
            first_change: None,
            last_change: None,
            tagged_count: 0,
            view_name: view_name.into(),
        }
    }

    /// Check if the view has any changes.
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
            "View '{}': {} changes (state: {}, {} tagged)",
            self.view_name,
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

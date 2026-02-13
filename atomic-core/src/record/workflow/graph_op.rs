//! GraphOp building from diff operations.
//!
//! This module provides functionality for converting diff operations into
//! repository hunks. Hunks are the semantic units of change in Atomic -
//! they represent operations like "insert these lines" or "delete this content".
//!
//! # Overview
//!
//! The graph_op builder bridges the gap between raw diff output and repository
//! graph operations:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        GraphOp Building Pipeline                           │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Diff Operations          GraphOp Builder            Repository Hunks      │
//! │  ┌────────────────┐      ┌──────────────┐       ┌─────────────────┐    │
//! │  │ DiffOp::Equal  │      │              │       │ (unchanged)     │    │
//! │  │ DiffOp::Insert │  ──► │ HunkBuilder  │  ──►  │ GraphOp::Edit      │    │
//! │  │ DiffOp::Delete │      │              │       │ (insert/delete) │    │
//! │  │ DiffOp::Replace│      │              │       │ GraphOp::Replacement│   │
//! │  └────────────────┘      └──────────────┘       └─────────────────┘    │
//! │                                                                         │
//! │  Key Responsibilities:                                                  │
//! │  ┌─────────────────────────────────────────────────────────────────┐   │
//! │  │ • Convert diff ops to hunks with proper graph references       │   │
//! │  │ • Track content positions within the change being built        │   │
//! │  │ • Compute up/down context for graph connectivity               │   │
//! │  │ • Handle encoding information for text vs binary content       │   │
//! │  │ • Generate local context for human-readable display            │   │
//! │  └─────────────────────────────────────────────────────────────────┘   │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Architecture
//!
//! ## HunkBuilder
//!
//! The [`HunkBuilder`] accumulates context as it processes diff operations
//! and produces hunks:
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::graph_op::HunkBuilder;
//! use atomic_core::diff::DiffOp;
//!
//! let mut builder = HunkBuilder::new("src/main.rs");
//!
//! // Process diff operations
//! builder.process_equal(0, 5);      // Lines 0-5 unchanged
//! builder.process_delete(5, 7);     // Lines 5-7 deleted
//! builder.process_insert(5, 8);     // New lines 5-8 inserted
//!
//! // Get the resulting hunks
//! let hunks = builder.finish();
//! ```
//!
//! ## Context Lines
//!
//! Hunks include context information for proper graph connectivity:
//!
//! - **Up context**: The span(es) that precede the change
//! - **Down context**: The span(es) that follow the change
//!
//! This context is essential for the CRDT-based merge algorithm to
//! correctly position changes relative to each other.
//!
//! ## Line Mapping
//!
//! The builder maintains mappings between:
//!
//! - Source file line numbers (for the old/pristine content)
//! - Target file line numbers (for the new/working copy content)
//! - Graph positions (vertices in the repository graph)
//!
//! # Example: Building Hunks from a Diff
//!
//! ```rust
//! use atomic_core::record::workflow::graph_op::{HunkBuilder, HunkBuildOptions, BuiltHunk};
//! use atomic_core::diff::DiffOp;
//! use atomic_core::change::Encoding;
//!
//! // Create a builder for editing a file
//! let options = HunkBuildOptions::new()
//!     .encoding(Encoding::Utf8)
//!     .context_lines(3);
//!
//! let mut builder = HunkBuilder::with_options("src/lib.rs", options);
//!
//! // Simulate processing diff operations
//! let diff_ops = vec![
//!     DiffOp::Equal { old_pos: 0, new_pos: 0, len: 5 },
//!     DiffOp::Replace { old_pos: 5, old_len: 2, new_pos: 5, new_len: 3 },
//!     DiffOp::Equal { old_pos: 7, new_pos: 8, len: 10 },
//! ];
//!
//! // Process each operation
//! for op in &diff_ops {
//!     builder.process_diff_op(op);
//! }
//!
//! // Get the built hunks
//! let result = builder.finish();
//! assert_eq!(result.hunk_count(), 1); // One replacement graph_op
//! ```
//!
//! # GraphOp Types Produced
//!
//! The builder produces different graph_op types based on the diff operations:
//!
//! | Diff Operation | Resulting GraphOp |
//! |----------------|----------------|
//! | `Insert` only | `GraphOp::Edit` with `Insertion` |
//! | `Delete` only | `GraphOp::Edit` with `EdgeUpdate` |
//! | `Replace` | `GraphOp::Replacement` (delete + insert) |
//! | `Equal` | No graph_op (context tracking only) |
//!
//! # Performance Considerations
//!
//! - The builder processes operations in a single pass
//! - Context is tracked incrementally to avoid re-scanning
//! - Line content is referenced, not copied, until final graph_op creation
//!
//! # Thread Safety
//!
//! `HunkBuilder` is designed for single-threaded use. For parallel
//! processing of multiple files, create separate builders per file.

use std::fmt;

use crate::change::{Encoding, Local};
use crate::diff::DiffOp;

// ============================================================================
// HUNK BUILD OPTIONS
// ============================================================================

/// Options for building hunks from diff operations.
///
/// Controls how hunks are constructed, including encoding information
/// and context line settings.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
/// use atomic_core::change::Encoding;
///
/// let options = HunkBuildOptions::new()
///     .encoding(Encoding::Utf8)
///     .context_lines(3)
///     .include_function_context(true);
///
/// assert_eq!(options.get_encoding(), Some(Encoding::Utf8));
/// assert_eq!(options.get_context_lines(), 3);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkBuildOptions {
    /// Text encoding for the file being processed.
    ///
    /// `None` indicates binary content.
    encoding: Option<Encoding>,

    /// Number of unchanged lines to include as context.
    ///
    /// Context lines help users understand where changes occur
    /// and are used for display purposes.
    context_lines: usize,

    /// Whether to include function/class context in output.
    ///
    /// When enabled, hunks include information about the enclosing
    /// function or class for better readability.
    include_function_context: bool,

    /// Minimum number of unchanged lines between hunks.
    ///
    /// If fewer than this many unchanged lines separate two changes,
    /// they are combined into a single graph_op.
    combine_threshold: usize,
}

impl HunkBuildOptions {
    /// Default number of context lines to include.
    pub const DEFAULT_CONTEXT_LINES: usize = 3;

    /// Default threshold for combining adjacent hunks.
    pub const DEFAULT_COMBINE_THRESHOLD: usize = 6;

    /// Create new options with default values.
    ///
    /// Default values:
    /// - `encoding`: None (binary)
    /// - `context_lines`: 3
    /// - `include_function_context`: false
    /// - `combine_threshold`: 6
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    ///
    /// let options = HunkBuildOptions::new();
    /// assert_eq!(options.get_context_lines(), 3);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the text encoding.
    ///
    /// # Arguments
    ///
    /// * `encoding` - The encoding to use (UTF-8, Binary, etc.)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    /// use atomic_core::change::Encoding;
    ///
    /// let options = HunkBuildOptions::new().encoding(Encoding::Utf8);
    /// assert_eq!(options.get_encoding(), Some(Encoding::Utf8));
    /// ```
    #[must_use]
    pub fn encoding(mut self, encoding: Encoding) -> Self {
        self.encoding = Some(encoding);
        self
    }

    /// Set the encoding to None (binary content).
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    ///
    /// let options = HunkBuildOptions::new().binary();
    /// assert!(options.get_encoding().is_none());
    /// ```
    #[must_use]
    pub fn binary(mut self) -> Self {
        self.encoding = None;
        self
    }

    /// Set the number of context lines.
    ///
    /// # Arguments
    ///
    /// * `lines` - Number of unchanged lines to include around changes
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    ///
    /// let options = HunkBuildOptions::new().context_lines(5);
    /// assert_eq!(options.get_context_lines(), 5);
    /// ```
    #[must_use]
    pub fn context_lines(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }

    /// Enable or disable function context inclusion.
    ///
    /// # Arguments
    ///
    /// * `include` - Whether to include enclosing function/class names
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    ///
    /// let options = HunkBuildOptions::new().include_function_context(true);
    /// assert!(options.get_include_function_context());
    /// ```
    #[must_use]
    pub fn include_function_context(mut self, include: bool) -> Self {
        self.include_function_context = include;
        self
    }

    /// Set the combine threshold.
    ///
    /// Hunks separated by fewer than this many unchanged lines
    /// will be merged into a single graph_op.
    ///
    /// # Arguments
    ///
    /// * `threshold` - Minimum unchanged lines between separate hunks
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildOptions;
    ///
    /// let options = HunkBuildOptions::new().combine_threshold(10);
    /// assert_eq!(options.get_combine_threshold(), 10);
    /// ```
    #[must_use]
    pub fn combine_threshold(mut self, threshold: usize) -> Self {
        self.combine_threshold = threshold;
        self
    }

    /// Get the encoding setting.
    #[must_use]
    pub fn get_encoding(&self) -> Option<Encoding> {
        self.encoding
    }

    /// Get the context lines setting.
    #[must_use]
    pub fn get_context_lines(&self) -> usize {
        self.context_lines
    }

    /// Get the function context inclusion setting.
    #[must_use]
    pub fn get_include_function_context(&self) -> bool {
        self.include_function_context
    }

    /// Get the combine threshold setting.
    #[must_use]
    pub fn get_combine_threshold(&self) -> usize {
        self.combine_threshold
    }

    /// Check if content should be treated as binary.
    #[must_use]
    pub fn is_binary(&self) -> bool {
        self.encoding.is_none()
    }
}

impl Default for HunkBuildOptions {
    fn default() -> Self {
        Self {
            encoding: None,
            context_lines: Self::DEFAULT_CONTEXT_LINES,
            include_function_context: false,
            combine_threshold: Self::DEFAULT_COMBINE_THRESHOLD,
        }
    }
}

// ============================================================================
// PENDING CHANGE
// ============================================================================

/// Represents a pending change that will become a graph_op.
///
/// This intermediate representation captures the essential information
/// about a change before it's converted into a full `GraphOp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChange {
    /// The type of change.
    pub kind: PendingChangeKind,

    /// Starting line in the old (pristine) content.
    pub old_start: usize,

    /// Number of lines affected in old content.
    pub old_len: usize,

    /// Starting line in the new (working copy) content.
    pub new_start: usize,

    /// Number of lines in new content.
    pub new_len: usize,
}

/// The type of pending change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PendingChangeKind {
    /// Lines were inserted (no deletion).
    Insert,

    /// Lines were deleted (no insertion).
    Delete,

    /// Lines were replaced (delete + insert).
    Replace,
}

impl PendingChange {
    /// Create a new insertion change.
    ///
    /// # Arguments
    ///
    /// * `old_index` - Position in old content (insertion point)
    /// * `new_start` - Starting line in new content
    /// * `new_len` - Number of lines inserted
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::{PendingChange, PendingChangeKind};
    ///
    /// let change = PendingChange::insert(5, 5, 3);
    /// assert_eq!(change.kind, PendingChangeKind::Insert);
    /// assert_eq!(change.old_start, 5);
    /// assert_eq!(change.old_len, 0);
    /// assert_eq!(change.new_start, 5);
    /// assert_eq!(change.new_len, 3);
    /// ```
    #[must_use]
    pub fn insert(old_index: usize, new_start: usize, new_len: usize) -> Self {
        Self {
            kind: PendingChangeKind::Insert,
            old_start: old_index,
            old_len: 0,
            new_start,
            new_len,
        }
    }

    /// Create a new deletion change.
    ///
    /// # Arguments
    ///
    /// * `old_start` - Starting line in old content
    /// * `old_len` - Number of lines deleted
    /// * `new_index` - Position in new content (deletion point)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::{PendingChange, PendingChangeKind};
    ///
    /// let change = PendingChange::delete(5, 3, 5);
    /// assert_eq!(change.kind, PendingChangeKind::Delete);
    /// assert_eq!(change.old_start, 5);
    /// assert_eq!(change.old_len, 3);
    /// assert_eq!(change.new_start, 5);
    /// assert_eq!(change.new_len, 0);
    /// ```
    #[must_use]
    pub fn delete(old_start: usize, old_len: usize, new_index: usize) -> Self {
        Self {
            kind: PendingChangeKind::Delete,
            old_start,
            old_len,
            new_start: new_index,
            new_len: 0,
        }
    }

    /// Create a new replacement change.
    ///
    /// # Arguments
    ///
    /// * `old_start` - Starting line in old content
    /// * `old_len` - Number of lines being replaced
    /// * `new_start` - Starting line in new content
    /// * `new_len` - Number of replacement lines
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::{PendingChange, PendingChangeKind};
    ///
    /// let change = PendingChange::replace(5, 2, 5, 4);
    /// assert_eq!(change.kind, PendingChangeKind::Replace);
    /// assert_eq!(change.old_start, 5);
    /// assert_eq!(change.old_len, 2);
    /// assert_eq!(change.new_start, 5);
    /// assert_eq!(change.new_len, 4);
    /// ```
    #[must_use]
    pub fn replace(old_start: usize, old_len: usize, new_start: usize, new_len: usize) -> Self {
        Self {
            kind: PendingChangeKind::Replace,
            old_start,
            old_len,
            new_start,
            new_len,
        }
    }

    /// Create from a diff replace operation's parameters.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::{PendingChange, PendingChangeKind};
    ///
    /// let change = PendingChange::from_replace(10, 2, 10, 3);
    /// assert_eq!(change.kind, PendingChangeKind::Replace);
    /// ```
    #[must_use]
    pub fn from_replace(old_pos: usize, old_len: usize, new_pos: usize, new_len: usize) -> Self {
        Self::replace(old_pos, old_len, new_pos, new_len)
    }

    /// Check if this is an insertion.
    #[must_use]
    pub fn is_insert(&self) -> bool {
        self.kind == PendingChangeKind::Insert
    }

    /// Check if this is a deletion.
    #[must_use]
    pub fn is_delete(&self) -> bool {
        self.kind == PendingChangeKind::Delete
    }

    /// Check if this is a replacement.
    #[must_use]
    pub fn is_replace(&self) -> bool {
        self.kind == PendingChangeKind::Replace
    }

    /// Get the line number for display (1-indexed).
    ///
    /// Uses the old line number for deletions/replacements,
    /// or new line number for insertions.
    #[must_use]
    pub fn display_line(&self) -> u64 {
        // Convert to 1-indexed for display
        match self.kind {
            PendingChangeKind::Insert => (self.new_start + 1) as u64,
            PendingChangeKind::Delete | PendingChangeKind::Replace => (self.old_start + 1) as u64,
        }
    }

    /// Check if this change can be combined with another.
    ///
    /// Changes can be combined if they are adjacent or overlapping.
    ///
    /// # Arguments
    ///
    /// * `other` - The other change to check
    /// * `gap` - Maximum gap (in lines) to allow between changes
    #[must_use]
    pub fn can_combine_with(&self, other: &Self, gap: usize) -> bool {
        // Check if changes are close enough in the old file
        let self_old_end = self.old_start + self.old_len;
        let other_old_start = other.old_start;

        if other_old_start > self_old_end {
            other_old_start - self_old_end <= gap
        } else {
            // Overlapping or adjacent
            true
        }
    }

    /// Combine this change with another, producing a merged change.
    ///
    /// # Arguments
    ///
    /// * `other` - The change to combine with (must come after self)
    ///
    /// # Panics
    ///
    /// Panics if `other` comes before `self`.
    #[must_use]
    pub fn combine_with(&self, other: &Self) -> Self {
        assert!(
            other.old_start >= self.old_start,
            "Cannot combine with a change that comes before"
        );

        // Calculate combined old range
        let other_old_end = other.old_start + other.old_len;
        let new_old_len = other_old_end.saturating_sub(self.old_start);

        // For new range, we sum the lengths since both insertions should be combined
        let combined_new_len = self.new_len + other.new_len;

        // Determine the combined kind
        let kind = if new_old_len == 0 && combined_new_len > 0 {
            PendingChangeKind::Insert
        } else if combined_new_len == 0 && new_old_len > 0 {
            PendingChangeKind::Delete
        } else if new_old_len > 0 && combined_new_len > 0 {
            PendingChangeKind::Replace
        } else {
            // Both are zero - shouldn't happen but default to delete
            PendingChangeKind::Delete
        };

        Self {
            kind,
            old_start: self.old_start,
            old_len: new_old_len,
            new_start: self.new_start,
            new_len: combined_new_len,
        }
    }
}

impl fmt::Display for PendingChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            PendingChangeKind::Insert => {
                write!(
                    f,
                    "Insert {} line(s) at line {}",
                    self.new_len,
                    self.new_start + 1
                )
            }
            PendingChangeKind::Delete => {
                write!(
                    f,
                    "Delete {} line(s) from line {}",
                    self.old_len,
                    self.old_start + 1
                )
            }
            PendingChangeKind::Replace => {
                write!(
                    f,
                    "Replace {} line(s) with {} line(s) at line {}",
                    self.old_len,
                    self.new_len,
                    self.old_start + 1
                )
            }
        }
    }
}

// ============================================================================
// BUILT HUNK
// ============================================================================

/// A fully constructed graph_op ready for inclusion in a change.
///
/// This represents the final form of a graph_op after processing diff
/// operations. It contains all the information needed to create
/// the corresponding graph operations.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::graph_op::{BuiltHunk, BuiltHunkKind};
/// use atomic_core::change::{Encoding, Local};
///
/// // Example of checking a built graph_op
/// let local = Local::new("src/main.rs", 42);
/// let graph_op = BuiltHunk::new_edit(
///     local,
///     Some(Encoding::Utf8),
///     0,    // content_start
///     100,  // content_end
/// );
///
/// assert!(graph_op.is_edit());
/// assert_eq!(graph_op.content_range(), Some((0, 100)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltHunk {
    /// The kind of graph_op.
    pub kind: BuiltHunkKind,

    /// Local context for display.
    pub local: Local,

    /// Text encoding (None for binary).
    pub encoding: Option<Encoding>,

    /// Start position in content buffer (for insertions).
    pub content_start: Option<u64>,

    /// End position in content buffer (for insertions).
    pub content_end: Option<u64>,

    /// Lines being deleted (line numbers in old content).
    pub deleted_lines: Vec<usize>,

    /// Starting line number in old content (0-indexed).
    /// For Insert hunks, this is the insertion point (insert AFTER this line).
    /// For Delete/Replace hunks, this is where the deletion starts.
    pub old_start: usize,

    /// Starting line number in new content (0-indexed).
    /// Used to calculate content byte positions.
    pub new_start: usize,

    /// Number of lines in new content.
    /// Used to calculate content byte positions.
    pub new_len: usize,
}

/// The kind of built graph_op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltHunkKind {
    /// Pure insertion (no deletion).
    Insert,

    /// Pure deletion (no insertion).
    Delete,

    /// Replacement (deletion + insertion).
    Replace,
}

impl BuiltHunk {
    /// Create a new edit (insert) graph_op.
    ///
    /// # Arguments
    ///
    /// * `local` - Local context for display
    /// * `encoding` - Text encoding
    /// * `content_start` - Start position in content buffer
    /// * `content_end` - End position in content buffer
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::BuiltHunk;
    /// use atomic_core::change::{Encoding, Local};
    ///
    /// let graph_op = BuiltHunk::new_edit(
    ///     Local::new("file.rs", 10),
    ///     Some(Encoding::Utf8),
    ///     0,
    ///     50,
    /// );
    /// assert!(graph_op.is_edit());
    /// ```
    #[must_use]
    pub fn new_edit(
        local: Local,
        encoding: Option<Encoding>,
        content_start: u64,
        content_end: u64,
    ) -> Self {
        Self {
            kind: BuiltHunkKind::Insert,
            local,
            encoding,
            content_start: Some(content_start),
            content_end: Some(content_end),
            deleted_lines: Vec::new(),
            old_start: 0,
            new_start: 0,
            new_len: 0,
        }
    }

    /// Create a new edit (insert) graph_op with line tracking.
    ///
    /// # Arguments
    ///
    /// * `local` - Local context for display
    /// * `encoding` - Text encoding
    /// * `new_start` - Starting line in new content (0-indexed)
    /// * `old_start` - Position in old content (insertion point)
    /// * `new_start` - Starting line in new content
    /// * `new_len` - Number of lines in new content
    #[must_use]
    pub fn new_edit_with_lines(
        local: Local,
        encoding: Option<Encoding>,
        old_start: usize,
        new_start: usize,
        new_len: usize,
    ) -> Self {
        Self {
            kind: BuiltHunkKind::Insert,
            local,
            encoding,
            content_start: None,
            content_end: None,
            deleted_lines: Vec::new(),
            old_start,
            new_start,
            new_len,
        }
    }

    /// Create a new deletion graph_op.
    ///
    /// # Arguments
    ///
    /// * `local` - Local context for display
    /// * `encoding` - Text encoding
    /// * `deleted_lines` - Line numbers being deleted
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::BuiltHunk;
    /// use atomic_core::change::{Encoding, Local};
    ///
    /// let graph_op = BuiltHunk::new_delete(
    ///     Local::new("file.rs", 10),
    ///     Some(Encoding::Utf8),
    ///     vec![10, 11, 12],
    ///     10,  // old_start line number
    /// );
    /// assert!(graph_op.is_delete());
    /// ```
    #[must_use]
    pub fn new_delete(
        local: Local,
        encoding: Option<Encoding>,
        deleted_lines: Vec<usize>,
        old_start: usize,
    ) -> Self {
        Self {
            kind: BuiltHunkKind::Delete,
            local,
            encoding,
            content_start: None,
            content_end: None,
            deleted_lines,
            old_start,
            new_start: 0,
            new_len: 0,
        }
    }

    /// Create a new replacement graph_op.
    ///
    /// # Arguments
    ///
    /// * `local` - Local context for display
    /// * `encoding` - Text encoding
    /// * `content_start` - Start position of new content
    /// * `content_end` - End position of new content
    /// * `deleted_lines` - Line numbers being replaced
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::BuiltHunk;
    /// use atomic_core::change::{Encoding, Local};
    ///
    /// let graph_op = BuiltHunk::new_replace(
    ///     Local::new("file.rs", 10),
    ///     Some(Encoding::Utf8),
    ///     0,
    ///     100,
    ///     vec![10, 11],
    /// );
    /// assert!(graph_op.is_replace());
    /// ```
    #[must_use]
    pub fn new_replace(
        local: Local,
        encoding: Option<Encoding>,
        content_start: u64,
        content_end: u64,
        deleted_lines: Vec<usize>,
    ) -> Self {
        Self {
            kind: BuiltHunkKind::Replace,
            local,
            encoding,
            content_start: Some(content_start),
            content_end: Some(content_end),
            deleted_lines,
            old_start: 0,
            new_start: 0,
            new_len: 0,
        }
    }

    /// Create a new replace graph_op with line tracking.
    ///
    /// # Arguments
    ///
    /// * `local` - Local context for display
    /// * `encoding` - Text encoding
    /// * `deleted_lines` - Line numbers being deleted
    /// * `new_start` - Starting line in new content (0-indexed)
    /// * `old_start` - Starting line in old content
    /// * `new_start` - Starting line in new content
    /// * `new_len` - Number of lines in replacement content
    #[must_use]
    pub fn new_replace_with_lines(
        local: Local,
        encoding: Option<Encoding>,
        deleted_lines: Vec<usize>,
        old_start: usize,
        new_start: usize,
        new_len: usize,
    ) -> Self {
        Self {
            kind: BuiltHunkKind::Replace,
            local,
            encoding,
            content_start: None,
            content_end: None,
            deleted_lines,
            old_start,
            new_start,
            new_len,
        }
    }

    /// Check if this is an edit (insert) graph_op.
    #[must_use]
    pub fn is_edit(&self) -> bool {
        self.kind == BuiltHunkKind::Insert
    }

    /// Check if this is a delete graph_op.
    #[must_use]
    pub fn is_delete(&self) -> bool {
        self.kind == BuiltHunkKind::Delete
    }

    /// Check if this is a replacement graph_op.
    #[must_use]
    pub fn is_replace(&self) -> bool {
        self.kind == BuiltHunkKind::Replace
    }

    /// Get the content range for this graph_op.
    ///
    /// Returns `None` for pure deletion hunks.
    #[must_use]
    pub fn content_range(&self) -> Option<(u64, u64)> {
        match (self.content_start, self.content_end) {
            (Some(start), Some(end)) => Some((start, end)),
            _ => None,
        }
    }

    /// Get the content length.
    ///
    /// Returns 0 for pure deletion hunks.
    #[must_use]
    pub fn content_len(&self) -> u64 {
        match (self.content_start, self.content_end) {
            (Some(start), Some(end)) => end.saturating_sub(start),
            _ => 0,
        }
    }

    /// Get the number of deleted lines.
    #[must_use]
    pub fn deleted_line_count(&self) -> usize {
        self.deleted_lines.len()
    }

    /// Get the path from local context.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.local.path
    }

    /// Get the line number from local context.
    #[must_use]
    pub fn line(&self) -> u64 {
        self.local.line
    }
}

impl fmt::Display for BuiltHunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            BuiltHunkKind::Insert => {
                write!(f, "Insert at {}:{}", self.local.path, self.local.line)
            }
            BuiltHunkKind::Delete => {
                write!(
                    f,
                    "Delete {} line(s) at {}:{}",
                    self.deleted_lines.len(),
                    self.local.path,
                    self.local.line
                )
            }
            BuiltHunkKind::Replace => {
                write!(
                    f,
                    "Replace {} line(s) at {}:{}",
                    self.deleted_lines.len(),
                    self.local.path,
                    self.local.line
                )
            }
        }
    }
}

// ============================================================================
// HUNK BUILD RESULT
// ============================================================================

/// The result of building hunks from diff operations.
///
/// Contains the list of built hunks along with statistics about
/// the building process.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::graph_op::HunkBuildResult;
///
/// let result = HunkBuildResult::new();
/// assert!(result.is_empty());
/// assert_eq!(result.hunk_count(), 0);
/// ```
#[derive(Debug, Clone, Default)]
pub struct HunkBuildResult {
    /// The built hunks.
    hunks: Vec<BuiltHunk>,

    /// Total lines inserted.
    lines_inserted: usize,

    /// Total lines deleted.
    lines_deleted: usize,

    /// Number of hunks that were combined.
    hunks_combined: usize,
}

impl HunkBuildResult {
    /// Create a new empty result.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuildResult;
    ///
    /// let result = HunkBuildResult::new();
    /// assert!(result.is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a built graph_op to the result.
    pub fn add_hunk(&mut self, graph_op: BuiltHunk) {
        // Update statistics
        if let Some((start, end)) = graph_op.content_range() {
            // Rough estimate: count newlines would be better but this is a proxy
            self.lines_inserted += (end - start) as usize;
        }
        self.lines_deleted += graph_op.deleted_line_count();

        self.hunks.push(graph_op);
    }

    /// Record that hunks were combined.
    pub fn record_combination(&mut self) {
        self.hunks_combined += 1;
    }

    /// Check if the result is empty (no hunks).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// Get the number of hunks.
    #[must_use]
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Get the hunks.
    #[must_use]
    pub fn hunks(&self) -> &[BuiltHunk] {
        &self.hunks
    }

    /// Take ownership of the hunks.
    #[must_use]
    pub fn into_hunks(self) -> Vec<BuiltHunk> {
        self.hunks
    }

    /// Get total lines inserted.
    #[must_use]
    pub fn lines_inserted(&self) -> usize {
        self.lines_inserted
    }

    /// Get total lines deleted.
    #[must_use]
    pub fn lines_deleted(&self) -> usize {
        self.lines_deleted
    }

    /// Get number of graph_op combinations.
    #[must_use]
    pub fn hunks_combined(&self) -> usize {
        self.hunks_combined
    }

    /// Iterate over the hunks.
    pub fn iter(&self) -> impl Iterator<Item = &BuiltHunk> {
        self.hunks.iter()
    }
}

impl IntoIterator for HunkBuildResult {
    type Item = BuiltHunk;
    type IntoIter = std::vec::IntoIter<BuiltHunk>;

    fn into_iter(self) -> Self::IntoIter {
        self.hunks.into_iter()
    }
}

impl<'a> IntoIterator for &'a HunkBuildResult {
    type Item = &'a BuiltHunk;
    type IntoIter = std::slice::Iter<'a, BuiltHunk>;

    fn into_iter(self) -> Self::IntoIter {
        self.hunks.iter()
    }
}

// ============================================================================
// HUNK BUILDER
// ============================================================================

/// Builder for converting diff operations into hunks.
///
/// The `HunkBuilder` processes diff operations one at a time, tracking
/// context and accumulating pending changes. When finished, it produces
/// a [`HunkBuildResult`] containing all the built hunks.
///
/// # Usage
///
/// ```rust
/// use atomic_core::record::workflow::graph_op::{HunkBuilder, HunkBuildOptions};
/// use atomic_core::diff::{DiffOp, Replacement};
/// use atomic_core::change::Encoding;
///
/// let options = HunkBuildOptions::new().encoding(Encoding::Utf8);
/// let mut builder = HunkBuilder::with_options("src/main.rs", options);
///
/// // Process diff operations
/// builder.process_diff_op(&DiffOp::Equal { old_pos: 0, new_pos: 0, len: 5 });
/// builder.process_diff_op(&DiffOp::Insert { old_pos: 5, new_pos: 5, len: 2 });
///
/// // Get results
/// let result = builder.finish();
/// ```
///
/// # Architecture
///
/// The builder maintains:
/// - Current position in both old and new content
/// - Pending changes not yet converted to hunks
/// - Running content buffer position tracking
///
/// When `finish()` is called, pending changes are flushed and
/// converted into proper hunks.
#[derive(Debug)]
pub struct HunkBuilder {
    /// Path of the file being processed.
    path: String,

    /// Build options.
    options: HunkBuildOptions,

    /// Pending changes to be converted to hunks.
    pending: Vec<PendingChange>,

    /// Current position in the content buffer.
    content_position: u64,

    /// Current line in old content.
    old_line: usize,

    /// Current line in new content.
    new_line: usize,
}

impl HunkBuilder {
    /// Create a new builder with default options.
    ///
    /// # Arguments
    ///
    /// * `path` - Path of the file being processed
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuilder;
    ///
    /// let builder = HunkBuilder::new("src/main.rs");
    /// ```
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self::with_options(path, HunkBuildOptions::new())
    }

    /// Create a new builder with custom options.
    ///
    /// # Arguments
    ///
    /// * `path` - Path of the file being processed
    /// * `options` - Build options
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::{HunkBuilder, HunkBuildOptions};
    /// use atomic_core::change::Encoding;
    ///
    /// let options = HunkBuildOptions::new().encoding(Encoding::Utf8);
    /// let builder = HunkBuilder::with_options("src/main.rs", options);
    /// ```
    #[must_use]
    pub fn with_options(path: impl Into<String>, options: HunkBuildOptions) -> Self {
        Self {
            path: path.into(),
            options,
            pending: Vec::new(),
            content_position: 0,
            old_line: 0,
            new_line: 0,
        }
    }

    /// Get the path being processed.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get the current options.
    #[must_use]
    pub fn options(&self) -> &HunkBuildOptions {
        &self.options
    }

    /// Get the number of pending changes.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Check if there are any pending changes.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Process a diff operation.
    ///
    /// This is the main entry point for processing diff output.
    /// Call this for each diff operation in sequence.
    ///
    /// # Arguments
    ///
    /// * `op` - The diff operation to process
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuilder;
    /// use atomic_core::diff::DiffOp;
    ///
    /// let mut builder = HunkBuilder::new("test.rs");
    /// builder.process_diff_op(&DiffOp::Equal { old_pos: 0, new_pos: 0, len: 10 });
    /// builder.process_diff_op(&DiffOp::Insert { old_pos: 10, new_pos: 10, len: 3 });
    /// ```
    pub fn process_diff_op(&mut self, op: &DiffOp) {
        match op {
            DiffOp::Equal {
                old_pos,
                new_pos,
                len,
            } => {
                self.process_equal(*old_pos, *new_pos, *len);
            }
            DiffOp::Insert {
                old_pos,
                new_pos,
                len,
            } => {
                self.process_insert(*old_pos, *new_pos, *len);
            }
            DiffOp::Delete {
                old_pos,
                new_pos,
                len,
            } => {
                self.process_delete(*old_pos, *new_pos, *len);
            }
            DiffOp::Replace {
                old_pos,
                old_len,
                new_pos,
                new_len,
            } => {
                self.process_replace_params(*old_pos, *old_len, *new_pos, *new_len);
            }
        }
    }

    /// Process an equal (unchanged) region.
    ///
    /// Equal regions update position tracking but don't create changes.
    /// They may cause pending changes to be flushed if the gap is large enough.
    ///
    /// # Arguments
    ///
    /// * `old_index` - Starting line in old content
    /// * `new_index` - Starting line in new content
    /// * `len` - Number of equal lines
    pub fn process_equal(&mut self, old_index: usize, new_index: usize, len: usize) {
        self.old_line = old_index + len;
        self.new_line = new_index + len;
    }

    /// Process an insertion.
    ///
    /// # Arguments
    ///
    /// * `old_index` - Position in old content (insertion point)
    /// * `new_index` - Starting line in new content
    /// * `new_len` - Number of lines inserted
    pub fn process_insert(&mut self, old_index: usize, new_index: usize, new_len: usize) {
        let change = PendingChange::insert(old_index, new_index, new_len);
        self.add_pending(change);
        self.new_line = new_index + new_len;
    }

    /// Process a deletion.
    ///
    /// # Arguments
    ///
    /// * `old_index` - Starting line in old content
    /// * `new_index` - Position in new content (deletion point)
    /// * `old_len` - Number of lines deleted
    pub fn process_delete(&mut self, old_index: usize, new_index: usize, old_len: usize) {
        let change = PendingChange::delete(old_index, old_len, new_index);
        self.add_pending(change);
        self.old_line = old_index + old_len;
    }

    /// Process a replacement with individual parameters.
    ///
    /// # Arguments
    ///
    /// * `old_pos` - Starting position in old content
    /// * `old_len` - Number of lines being replaced
    /// * `new_pos` - Starting position in new content
    /// * `new_len` - Number of replacement lines
    pub fn process_replace_params(
        &mut self,
        old_pos: usize,
        old_len: usize,
        new_pos: usize,
        new_len: usize,
    ) {
        let change = PendingChange::from_replace(old_pos, old_len, new_pos, new_len);
        self.add_pending(change);
        self.old_line = old_pos + old_len;
        self.new_line = new_pos + new_len;
    }

    /// Add a pending change, potentially combining with existing ones.
    fn add_pending(&mut self, change: PendingChange) {
        // Check if we can combine with the last pending change
        if let Some(last) = self.pending.last() {
            if last.can_combine_with(&change, self.options.combine_threshold) {
                let combined = last.combine_with(&change);
                self.pending.pop();
                self.pending.push(combined);
                return;
            }
        }
        self.pending.push(change);
    }

    /// Finish building and return the result.
    ///
    /// This converts all pending changes into built hunks and returns
    /// the complete result.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::graph_op::HunkBuilder;
    /// use atomic_core::diff::DiffOp;
    ///
    /// let mut builder = HunkBuilder::new("test.rs");
    /// builder.process_diff_op(&DiffOp::Insert { old_pos: 0, new_pos: 0, len: 3 });
    ///
    /// let result = builder.finish();
    /// assert_eq!(result.hunk_count(), 1);
    /// ```
    #[must_use]
    pub fn finish(self) -> HunkBuildResult {
        let mut result = HunkBuildResult::new();

        for pending in self.pending {
            let local = Local::new(&self.path, pending.display_line());
            let encoding = self.options.encoding;

            let graph_op = match pending.kind {
                PendingChangeKind::Insert => {
                    // Use the new_edit_with_lines constructor to track which lines
                    // in the new content this graph_op covers. The caller will use this
                    // to calculate actual byte positions in the content buffer.
                    // old_start is the insertion point (insert AFTER this line in old content)
                    BuiltHunk::new_edit_with_lines(local, encoding, pending.old_start, pending.new_start, pending.new_len)
                }
                PendingChangeKind::Delete => {
                    let deleted: Vec<usize> =
                        (pending.old_start..pending.old_start + pending.old_len).collect();
                    BuiltHunk::new_delete(local, encoding, deleted, pending.old_start)
                }
                PendingChangeKind::Replace => {
                    let deleted: Vec<usize> =
                        (pending.old_start..pending.old_start + pending.old_len).collect();
                    // Use the new_replace_with_lines constructor to track which lines
                    // in the new content this graph_op covers.
                    BuiltHunk::new_replace_with_lines(local, encoding, deleted, pending.old_start, pending.new_start, pending.new_len)
                }
            };

            result.add_hunk(graph_op);
        }

        result
    }

    /// Reset the builder for reuse with a new file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path of the new file
    pub fn reset(&mut self, path: impl Into<String>) {
        self.path = path.into();
        self.pending.clear();
        self.content_position = 0;
        self.old_line = 0;
        self.new_line = 0;
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // HunkBuildOptions tests
    // ========================================================================

    #[test]
    fn test_options_new_returns_defaults() {
        let opts = HunkBuildOptions::new();
        assert!(opts.get_encoding().is_none());
        assert_eq!(opts.get_context_lines(), 3);
        assert!(!opts.get_include_function_context());
        assert_eq!(opts.get_combine_threshold(), 6);
    }

    #[test]
    fn test_options_default() {
        let opts = HunkBuildOptions::default();
        assert!(opts.is_binary());
        assert_eq!(opts.get_context_lines(), HunkBuildOptions::DEFAULT_CONTEXT_LINES);
    }

    #[test]
    fn test_options_encoding() {
        let opts = HunkBuildOptions::new().encoding(Encoding::Utf8);
        assert_eq!(opts.get_encoding(), Some(Encoding::Utf8));
        assert!(!opts.is_binary());
    }

    #[test]
    fn test_options_binary() {
        let opts = HunkBuildOptions::new().encoding(Encoding::Utf8).binary();
        assert!(opts.get_encoding().is_none());
        assert!(opts.is_binary());
    }

    #[test]
    fn test_options_context_lines() {
        let opts = HunkBuildOptions::new().context_lines(5);
        assert_eq!(opts.get_context_lines(), 5);
    }

    #[test]
    fn test_options_include_function_context() {
        let opts = HunkBuildOptions::new().include_function_context(true);
        assert!(opts.get_include_function_context());
    }

    #[test]
    fn test_options_combine_threshold() {
        let opts = HunkBuildOptions::new().combine_threshold(10);
        assert_eq!(opts.get_combine_threshold(), 10);
    }

    #[test]
    fn test_options_builder_chain() {
        let opts = HunkBuildOptions::new()
            .encoding(Encoding::Utf8)
            .context_lines(5)
            .include_function_context(true)
            .combine_threshold(8);

        assert_eq!(opts.get_encoding(), Some(Encoding::Utf8));
        assert_eq!(opts.get_context_lines(), 5);
        assert!(opts.get_include_function_context());
        assert_eq!(opts.get_combine_threshold(), 8);
    }

    #[test]
    fn test_options_clone() {
        let opts = HunkBuildOptions::new().encoding(Encoding::Utf8);
        let cloned = opts.clone();
        assert_eq!(opts, cloned);
    }

    #[test]
    fn test_options_debug() {
        let opts = HunkBuildOptions::new();
        let debug = format!("{:?}", opts);
        assert!(debug.contains("HunkBuildOptions"));
    }

    // ========================================================================
    // PendingChange tests
    // ========================================================================

    #[test]
    fn test_pending_change_insert() {
        let change = PendingChange::insert(5, 5, 3);
        assert!(change.is_insert());
        assert!(!change.is_delete());
        assert!(!change.is_replace());
        assert_eq!(change.old_start, 5);
        assert_eq!(change.old_len, 0);
        assert_eq!(change.new_start, 5);
        assert_eq!(change.new_len, 3);
    }

    #[test]
    fn test_pending_change_delete() {
        let change = PendingChange::delete(5, 3, 5);
        assert!(!change.is_insert());
        assert!(change.is_delete());
        assert!(!change.is_replace());
        assert_eq!(change.old_start, 5);
        assert_eq!(change.old_len, 3);
        assert_eq!(change.new_start, 5);
        assert_eq!(change.new_len, 0);
    }

    #[test]
    fn test_pending_change_replace() {
        let change = PendingChange::replace(5, 2, 5, 4);
        assert!(!change.is_insert());
        assert!(!change.is_delete());
        assert!(change.is_replace());
        assert_eq!(change.old_start, 5);
        assert_eq!(change.old_len, 2);
        assert_eq!(change.new_start, 5);
        assert_eq!(change.new_len, 4);
    }

    #[test]
    fn test_pending_change_from_replace() {
        let change = PendingChange::from_replace(10, 2, 10, 3);
        assert!(change.is_replace());
        assert_eq!(change.old_start, 10);
        assert_eq!(change.old_len, 2);
        assert_eq!(change.new_start, 10);
        assert_eq!(change.new_len, 3);
    }

    #[test]
    fn test_pending_change_display_line_insert() {
        let change = PendingChange::insert(9, 9, 2);
        assert_eq!(change.display_line(), 10); // 1-indexed
    }

    #[test]
    fn test_pending_change_display_line_delete() {
        let change = PendingChange::delete(4, 2, 4);
        assert_eq!(change.display_line(), 5); // 1-indexed
    }

    #[test]
    fn test_pending_change_can_combine_adjacent() {
        let change1 = PendingChange::insert(5, 5, 2);
        let change2 = PendingChange::insert(5, 7, 1);
        assert!(change1.can_combine_with(&change2, 0));
    }

    #[test]
    fn test_pending_change_can_combine_with_gap() {
        let change1 = PendingChange::delete(5, 2, 5);
        let change2 = PendingChange::delete(10, 1, 8);
        // Gap is 10 - 7 = 3 lines
        assert!(change1.can_combine_with(&change2, 5));
        assert!(!change1.can_combine_with(&change2, 2));
    }

    #[test]
    fn test_pending_change_combine_with() {
        let change1 = PendingChange::delete(5, 2, 5);
        let change2 = PendingChange::delete(8, 1, 6);
        let combined = change1.combine_with(&change2);

        assert_eq!(combined.old_start, 5);
        assert_eq!(combined.old_len, 4); // lines 5-8 inclusive = 4 lines
        assert_eq!(combined.new_len, 0); // both deletions have new_len = 0
        assert!(combined.is_delete());
    }

    #[test]
    fn test_pending_change_combine_inserts() {
        let change1 = PendingChange::insert(5, 5, 2);
        let change2 = PendingChange::insert(5, 7, 3);
        let combined = change1.combine_with(&change2);

        assert_eq!(combined.old_start, 5);
        assert_eq!(combined.old_len, 0);
        assert_eq!(combined.new_len, 5); // 2 + 3 = 5
        assert!(combined.is_insert());
    }

    #[test]
    fn test_pending_change_combine_delete_and_insert() {
        let change1 = PendingChange::delete(5, 2, 5);
        let change2 = PendingChange::insert(7, 5, 3);
        let combined = change1.combine_with(&change2);

        assert_eq!(combined.old_start, 5);
        assert_eq!(combined.old_len, 2);
        assert_eq!(combined.new_len, 3);
        assert!(combined.is_replace());
    }

    #[test]
    fn test_pending_change_display() {
        let insert = PendingChange::insert(5, 5, 3);
        assert!(format!("{}", insert).contains("Insert"));

        let delete = PendingChange::delete(5, 2, 5);
        assert!(format!("{}", delete).contains("Delete"));

        let replace = PendingChange::replace(5, 2, 5, 4);
        assert!(format!("{}", replace).contains("Replace"));
    }

    #[test]
    fn test_pending_change_clone() {
        let change = PendingChange::insert(5, 5, 3);
        let cloned = change.clone();
        assert_eq!(change, cloned);
    }

    // ========================================================================
    // BuiltHunk tests
    // ========================================================================

    #[test]
    fn test_built_hunk_new_edit() {
        let local = Local::new("test.rs", 10);
        let graph_op = BuiltHunk::new_edit(local, Some(Encoding::Utf8), 0, 50);

        assert!(graph_op.is_edit());
        assert!(!graph_op.is_delete());
        assert!(!graph_op.is_replace());
        assert_eq!(graph_op.content_range(), Some((0, 50)));
        assert_eq!(graph_op.content_len(), 50);
        assert_eq!(graph_op.deleted_line_count(), 0);
    }

    #[test]
    fn test_built_hunk_new_delete() {
        let local = Local::new("test.rs", 10);
        let graph_op = BuiltHunk::new_delete(local, Some(Encoding::Utf8), vec![10, 11, 12], 10);

        assert!(!graph_op.is_edit());
        assert!(graph_op.is_delete());
        assert!(!graph_op.is_replace());
        assert!(graph_op.content_range().is_none());
        assert_eq!(graph_op.content_len(), 0);
        assert_eq!(graph_op.deleted_line_count(), 3);
    }

    #[test]
    fn test_built_hunk_new_replace() {
        let local = Local::new("test.rs", 10);
        let graph_op = BuiltHunk::new_replace(local, Some(Encoding::Utf8), 0, 100, vec![10, 11]);

        assert!(!graph_op.is_edit());
        assert!(!graph_op.is_delete());
        assert!(graph_op.is_replace());
        assert_eq!(graph_op.content_range(), Some((0, 100)));
        assert_eq!(graph_op.content_len(), 100);
        assert_eq!(graph_op.deleted_line_count(), 2);
    }

    #[test]
    fn test_built_hunk_path_and_line() {
        let local = Local::new("src/main.rs", 42);
        let graph_op = BuiltHunk::new_edit(local, None, 0, 10);

        assert_eq!(graph_op.path(), "src/main.rs");
        assert_eq!(graph_op.line(), 42);
    }

    #[test]
    fn test_built_hunk_display() {
        let local = Local::new("test.rs", 10);
        let graph_op = BuiltHunk::new_edit(local, None, 0, 10);
        let display = format!("{}", graph_op);
        assert!(display.contains("Insert"));
        assert!(display.contains("test.rs"));
    }

    #[test]
    fn test_built_hunk_clone() {
        let local = Local::new("test.rs", 10);
        let graph_op = BuiltHunk::new_edit(local, Some(Encoding::Utf8), 0, 50);
        let cloned = graph_op.clone();
        assert_eq!(graph_op, cloned);
    }

    // ========================================================================
    // HunkBuildResult tests
    // ========================================================================

    #[test]
    fn test_build_result_new() {
        let result = HunkBuildResult::new();
        assert!(result.is_empty());
        assert_eq!(result.hunk_count(), 0);
        assert_eq!(result.lines_inserted(), 0);
        assert_eq!(result.lines_deleted(), 0);
    }

    #[test]
    fn test_build_result_add_hunk() {
        let mut result = HunkBuildResult::new();
        let local = Local::new("test.rs", 10);
        let graph_op = BuiltHunk::new_edit(local, None, 0, 50);

        result.add_hunk(graph_op);

        assert!(!result.is_empty());
        assert_eq!(result.hunk_count(), 1);
        assert_eq!(result.lines_inserted(), 50);
    }

    #[test]
    fn test_build_result_add_delete_hunk() {
        let mut result = HunkBuildResult::new();
        let local = Local::new("test.rs", 10);
        let graph_op = BuiltHunk::new_delete(local, None, vec![10, 11, 12], 10);

        result.add_hunk(graph_op);

        assert_eq!(result.lines_deleted(), 3);
        assert_eq!(result.lines_inserted(), 0);
    }

    #[test]
    fn test_build_result_hunks() {
        let mut result = HunkBuildResult::new();
        let local1 = Local::new("test.rs", 10);
        let local2 = Local::new("test.rs", 20);
        result.add_hunk(BuiltHunk::new_edit(local1, None, 0, 10));
        result.add_hunk(BuiltHunk::new_edit(local2, None, 10, 20));

        assert_eq!(result.hunks().len(), 2);
    }

    #[test]
    fn test_build_result_into_hunks() {
        let mut result = HunkBuildResult::new();
        let local = Local::new("test.rs", 10);
        result.add_hunk(BuiltHunk::new_edit(local, None, 0, 10));

        let hunks = result.into_hunks();
        assert_eq!(hunks.len(), 1);
    }

    #[test]
    fn test_build_result_iter() {
        let mut result = HunkBuildResult::new();
        let local1 = Local::new("test.rs", 10);
        let local2 = Local::new("test.rs", 20);
        result.add_hunk(BuiltHunk::new_edit(local1, None, 0, 10));
        result.add_hunk(BuiltHunk::new_edit(local2, None, 10, 20));

        let count = result.iter().count();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_build_result_into_iterator() {
        let mut result = HunkBuildResult::new();
        let local = Local::new("test.rs", 10);
        result.add_hunk(BuiltHunk::new_edit(local, None, 0, 10));

        let count = result.into_iter().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_build_result_ref_iterator() {
        let mut result = HunkBuildResult::new();
        let local = Local::new("test.rs", 10);
        result.add_hunk(BuiltHunk::new_edit(local, None, 0, 10));

        let count = (&result).into_iter().count();
        assert_eq!(count, 1);
    }

    // ========================================================================
    // HunkBuilder tests
    // ========================================================================

    #[test]
    fn test_builder_new() {
        let builder = HunkBuilder::new("test.rs");
        assert_eq!(builder.path(), "test.rs");
        assert_eq!(builder.pending_count(), 0);
        assert!(!builder.has_pending());
    }

    #[test]
    fn test_builder_with_options() {
        let options = HunkBuildOptions::new().encoding(Encoding::Utf8);
        let builder = HunkBuilder::with_options("test.rs", options);
        assert_eq!(builder.options().get_encoding(), Some(Encoding::Utf8));
    }

    #[test]
    fn test_builder_process_equal() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_equal(0, 0, 10);

        assert_eq!(builder.pending_count(), 0);
    }

    #[test]
    fn test_builder_process_insert() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_insert(5, 5, 3);

        assert_eq!(builder.pending_count(), 1);
        assert!(builder.has_pending());
    }

    #[test]
    fn test_builder_process_delete() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_delete(5, 5, 2);

        assert_eq!(builder.pending_count(), 1);
    }

    #[test]
    fn test_builder_process_replace() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_replace_params(5, 2, 5, 3);

        assert_eq!(builder.pending_count(), 1);
    }

    #[test]
    fn test_builder_process_diff_op_equal() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_diff_op(&DiffOp::Equal {
            old_pos: 0,
            new_pos: 0,
            len: 10,
        });

        assert_eq!(builder.pending_count(), 0);
    }

    #[test]
    fn test_builder_process_diff_op_insert() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_diff_op(&DiffOp::Insert {
            old_pos: 5,
            new_pos: 5,
            len: 3,
        });

        assert_eq!(builder.pending_count(), 1);
    }

    #[test]
    fn test_builder_process_diff_op_delete() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_diff_op(&DiffOp::Delete {
            old_pos: 5,
            new_pos: 5,
            len: 2,
        });

        assert_eq!(builder.pending_count(), 1);
    }

    #[test]
    fn test_builder_process_diff_op_replace() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_diff_op(&DiffOp::Replace {
            old_pos: 5,
            old_len: 2,
            new_pos: 5,
            new_len: 3,
        });

        assert_eq!(builder.pending_count(), 1);
    }

    #[test]
    fn test_builder_finish_empty() {
        let builder = HunkBuilder::new("test.rs");
        let result = builder.finish();

        assert!(result.is_empty());
        assert_eq!(result.hunk_count(), 0);
    }

    #[test]
    fn test_builder_finish_with_insert() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_insert(5, 5, 3);

        let result = builder.finish();

        assert_eq!(result.hunk_count(), 1);
        assert!(result.hunks()[0].is_edit());
    }

    #[test]
    fn test_builder_finish_with_delete() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_delete(5, 5, 2);

        let result = builder.finish();

        assert_eq!(result.hunk_count(), 1);
        assert!(result.hunks()[0].is_delete());
        assert_eq!(result.hunks()[0].deleted_line_count(), 2);
    }

    #[test]
    fn test_builder_finish_with_replace() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_replace_params(5, 2, 5, 3);

        let result = builder.finish();

        assert_eq!(result.hunk_count(), 1);
        assert!(result.hunks()[0].is_replace());
    }

    #[test]
    fn test_builder_multiple_operations() {
        let mut builder = HunkBuilder::new("test.rs");

        // Simulate a typical diff output
        builder.process_equal(0, 0, 5);
        builder.process_delete(5, 5, 2);
        builder.process_equal(7, 5, 10);
        builder.process_insert(17, 15, 3);

        let result = builder.finish();

        // Should have 2 separate hunks (gap > combine_threshold)
        assert_eq!(result.hunk_count(), 2);
    }

    #[test]
    fn test_builder_combines_adjacent_changes() {
        let mut builder = HunkBuilder::new("test.rs");

        // Two adjacent changes should be combined
        builder.process_delete(5, 5, 1);
        builder.process_delete(6, 4, 1);

        let result = builder.finish();

        // Should be combined into one graph_op
        assert_eq!(result.hunk_count(), 1);
    }

    #[test]
    fn test_builder_reset() {
        let mut builder = HunkBuilder::new("test.rs");
        builder.process_insert(5, 5, 3);

        builder.reset("other.rs");

        assert_eq!(builder.path(), "other.rs");
        assert_eq!(builder.pending_count(), 0);
    }

    #[test]
    fn test_builder_with_encoding() {
        let options = HunkBuildOptions::new().encoding(Encoding::Utf8);
        let mut builder = HunkBuilder::with_options("test.rs", options);
        builder.process_insert(0, 0, 1);

        let result = builder.finish();

        assert_eq!(result.hunks()[0].encoding, Some(Encoding::Utf8));
    }

    #[test]
    fn test_builder_workflow_scenario() {
        // Simulate a real edit scenario:
        // Old: lines 0-9 (10 lines)
        // New: lines 0-4 unchanged, line 5 modified, lines 6-9 unchanged + 2 new lines at end
        let options = HunkBuildOptions::new().encoding(Encoding::Utf8);
        let mut builder = HunkBuilder::with_options("src/main.rs", options);

        // Lines 0-4 unchanged
        builder.process_diff_op(&DiffOp::Equal {
            old_pos: 0,
            new_pos: 0,
            len: 5,
        });

        // Line 5 replaced
        builder.process_diff_op(&DiffOp::Replace {
            old_pos: 5,
            old_len: 1,
            new_pos: 5,
            new_len: 2,
        });

        // Lines 6-9 unchanged (large gap, more than combine_threshold of 6)
        builder.process_diff_op(&DiffOp::Equal {
            old_pos: 6,
            new_pos: 7,
            len: 10,
        });

        // 2 new lines at end (after 10 unchanged lines, should be separate graph_op)
        builder.process_diff_op(&DiffOp::Insert {
            old_pos: 16,
            new_pos: 17,
            len: 2,
        });

        let result = builder.finish();

        // Should have 2 hunks: one replacement and one insert
        // (the equal region of 10 lines is greater than combine_threshold of 6)
        assert_eq!(result.hunk_count(), 2);
        assert!(result.hunks()[0].is_replace());
        assert!(result.hunks()[1].is_edit());
    }
}

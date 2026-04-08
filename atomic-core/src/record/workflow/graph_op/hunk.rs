//! Built hunk types and build results.
//!
//! Contains [`BuiltHunk`], [`BuiltHunkKind`], and [`HunkBuildResult`] —
//! the final output types produced by the hunk building pipeline.

use std::fmt;

use crate::change::{Encoding, Local};

// BUILT HUNK

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
    /// * `old_start` - Starting line in old content (0-indexed)
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

// HUNK BUILD RESULT

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

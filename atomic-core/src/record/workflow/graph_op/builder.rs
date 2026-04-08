//! Hunk builder for converting diff operations into hunks.
//!
//! Contains [`HunkBuilder`] which processes diff operations one at a time,
//! tracking context and accumulating pending changes, then produces a
//! [`HunkBuildResult`] containing all built hunks.

use crate::change::Local;
use crate::diff::DiffOp;

use super::hunk::{BuiltHunk, HunkBuildResult};
use super::options::HunkBuildOptions;
use super::pending::{PendingChange, PendingChangeKind};

// HUNK BUILDER

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
                    BuiltHunk::new_edit_with_lines(
                        local,
                        encoding,
                        pending.old_start,
                        pending.new_start,
                        pending.new_len,
                    )
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
                    BuiltHunk::new_replace_with_lines(
                        local,
                        encoding,
                        deleted,
                        pending.old_start,
                        pending.new_start,
                        pending.new_len,
                    )
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

//! Pending change representation.
//!
//! Contains [`PendingChange`] and [`PendingChangeKind`] — intermediate
//! representations of detected changes before they are converted into
//! fully built hunks.

use std::fmt;

// PENDING CHANGE

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

        // Calculate combined new range.
        //
        // This must span from the first change's new_start through the
        // later change's new end, not merely sum the two changed ranges.
        // When two changes are adjacent in the old file but separated in
        // the new file by preserved or inserted lines, summing the lengths
        // drops that middle section from the replacement hunk.  The graph
        // then records a patch that deletes a broad old range but only
        // inserts the changed fragments, causing materialization to skip or
        // rotate content after sequential inserts.
        let self_new_end = self.new_start + self.new_len;
        let other_new_end = other.new_start + other.new_len;
        let combined_new_len = self_new_end
            .max(other_new_end)
            .saturating_sub(self.new_start);

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

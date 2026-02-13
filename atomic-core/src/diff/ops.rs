//! Diff operations and result types.
//!
//! This module defines the data structures used to represent the output of
//! diff algorithms. A diff is a sequence of operations that describe how to
//! transform one sequence into another.
//!
//! # Operation Types
//!
//! The fundamental operations are:
//!
//! - **Equal**: Lines that appear unchanged in both sequences
//! - **Insert**: Lines added to the new sequence
//! - **Delete**: Lines removed from the old sequence
//! - **Replace**: Lines that were both deleted and inserted at the same position
//!
//! # Example
//!
//! Consider transforming "A\nB\nC\n" to "A\nX\nC\n":
//!
//! ```text
//! Old:  A  B  C
//!       ↓  ↓  ↓
//! New:  A  X  C
//!
//! Operations:
//! 1. Equal(0, 0)     - Line A unchanged
//! 2. Replace(1, 1)   - Line B replaced with X
//! 3. Equal(2, 2)     - Line C unchanged
//! ```
//!
//! # Replacement Representation
//!
//! For compatibility with the Atomic change system, we also provide a
//! [`Replacement`] struct that combines deletions and insertions at a
//! position. This is closer to how hunks are stored in change files.

use std::fmt;
use std::ops::Range;

/// A single diff operation.
///
/// Each operation describes a transformation step needed to convert the
/// old sequence into the new sequence.
///
/// # Position Tracking
///
/// Operations track positions in both sequences:
/// - `old_pos`: Position in the original sequence
/// - `new_pos`: Position in the modified sequence
/// - `len`: Number of lines affected
///
/// For `Insert`, `old_pos` is where the insertion happens (between lines).
/// For `Delete`, `new_pos` is where the deletion was (between lines).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffOp {
    /// Lines that are identical in both sequences.
    ///
    /// These lines don't need any transformation - they exist at
    /// corresponding positions in both old and new sequences.
    Equal {
        /// Starting position in the old sequence.
        old_pos: usize,
        /// Starting position in the new sequence.
        new_pos: usize,
        /// Number of consecutive equal lines.
        len: usize,
    },

    /// Lines that exist only in the new sequence (added).
    ///
    /// These lines were inserted into the new version.
    Insert {
        /// Position in the old sequence where insertion occurs.
        ///
        /// This is the position "between" lines where new content appears.
        old_pos: usize,
        /// Starting position in the new sequence.
        new_pos: usize,
        /// Number of lines inserted.
        len: usize,
    },

    /// Lines that exist only in the old sequence (removed).
    ///
    /// These lines were deleted from the old version.
    Delete {
        /// Starting position in the old sequence.
        old_pos: usize,
        /// Position in the new sequence where deletion occurred.
        new_pos: usize,
        /// Number of lines deleted.
        len: usize,
    },

    /// Lines that were both deleted and inserted at the same position.
    ///
    /// A replace is semantically equivalent to a delete followed by an
    /// insert at the same position, but combining them provides better
    /// context for display and application.
    Replace {
        /// Starting position in the old sequence.
        old_pos: usize,
        /// Number of lines deleted from old sequence.
        old_len: usize,
        /// Starting position in the new sequence.
        new_pos: usize,
        /// Number of lines inserted in new sequence.
        new_len: usize,
    },
}

impl DiffOp {
    /// Create an Equal operation.
    pub fn equal(old_pos: usize, new_pos: usize, len: usize) -> Self {
        DiffOp::Equal {
            old_pos,
            new_pos,
            len,
        }
    }

    /// Create an Insert operation.
    pub fn insert(old_pos: usize, new_pos: usize, len: usize) -> Self {
        DiffOp::Insert {
            old_pos,
            new_pos,
            len,
        }
    }

    /// Create a Delete operation.
    pub fn delete(old_pos: usize, new_pos: usize, len: usize) -> Self {
        DiffOp::Delete {
            old_pos,
            new_pos,
            len,
        }
    }

    /// Create a Replace operation.
    pub fn replace(old_pos: usize, old_len: usize, new_pos: usize, new_len: usize) -> Self {
        DiffOp::Replace {
            old_pos,
            old_len,
            new_pos,
            new_len,
        }
    }

    /// Check if this is an Equal operation.
    pub fn is_equal(&self) -> bool {
        matches!(self, DiffOp::Equal { .. })
    }

    /// Check if this is an Insert operation.
    pub fn is_insert(&self) -> bool {
        matches!(self, DiffOp::Insert { .. })
    }

    /// Check if this is a Delete operation.
    pub fn is_delete(&self) -> bool {
        matches!(self, DiffOp::Delete { .. })
    }

    /// Check if this is a Replace operation.
    pub fn is_replace(&self) -> bool {
        matches!(self, DiffOp::Replace { .. })
    }

    /// Check if this operation represents a change (not equal).
    pub fn is_change(&self) -> bool {
        !self.is_equal()
    }

    /// Get the range of lines affected in the old sequence.
    pub fn old_range(&self) -> Range<usize> {
        match *self {
            DiffOp::Equal { old_pos, len, .. } => old_pos..old_pos + len,
            DiffOp::Insert { old_pos, .. } => old_pos..old_pos,
            DiffOp::Delete { old_pos, len, .. } => old_pos..old_pos + len,
            DiffOp::Replace {
                old_pos, old_len, ..
            } => old_pos..old_pos + old_len,
        }
    }

    /// Get the range of lines affected in the new sequence.
    pub fn new_range(&self) -> Range<usize> {
        match *self {
            DiffOp::Equal { new_pos, len, .. } => new_pos..new_pos + len,
            DiffOp::Insert { new_pos, len, .. } => new_pos..new_pos + len,
            DiffOp::Delete { new_pos, .. } => new_pos..new_pos,
            DiffOp::Replace {
                new_pos, new_len, ..
            } => new_pos..new_pos + new_len,
        }
    }

    /// Get the number of lines deleted by this operation.
    pub fn deletions(&self) -> usize {
        match *self {
            DiffOp::Equal { .. } => 0,
            DiffOp::Insert { .. } => 0,
            DiffOp::Delete { len, .. } => len,
            DiffOp::Replace { old_len, .. } => old_len,
        }
    }

    /// Get the number of lines inserted by this operation.
    pub fn insertions(&self) -> usize {
        match *self {
            DiffOp::Equal { .. } => 0,
            DiffOp::Insert { len, .. } => len,
            DiffOp::Delete { .. } => 0,
            DiffOp::Replace { new_len, .. } => new_len,
        }
    }
}

impl fmt::Display for DiffOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DiffOp::Equal {
                old_pos,
                new_pos,
                len,
            } => write!(f, "Equal(old={}..{}, new={}..{})", old_pos, old_pos + len, new_pos, new_pos + len),
            DiffOp::Insert {
                old_pos,
                new_pos,
                len,
            } => write!(f, "Insert(at old={}, new={}..{})", old_pos, new_pos, new_pos + len),
            DiffOp::Delete {
                old_pos,
                new_pos,
                len,
            } => write!(f, "Delete(old={}..{}, at new={})", old_pos, old_pos + len, new_pos),
            DiffOp::Replace {
                old_pos,
                old_len,
                new_pos,
                new_len,
            } => write!(
                f,
                "Replace(old={}..{}, new={}..{})",
                old_pos,
                old_pos + old_len,
                new_pos,
                new_pos + new_len
            ),
        }
    }
}

/// A replacement operation combining deletion and insertion.
///
/// This is an alternative representation used for generating hunks.
/// It always tracks both the deleted and inserted regions, even when
/// one is empty (pure insertion or pure deletion).
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::Replacement;
///
/// // A pure insertion at line 5
/// let insert = Replacement::new(5, 0, 5, 3);
/// assert!(insert.is_insert_only());
///
/// // A pure deletion of lines 10-12
/// let delete = Replacement::new(10, 3, 10, 0);
/// assert!(delete.is_delete_only());
///
/// // A replacement of line 7 with 2 new lines
/// let replace = Replacement::new(7, 1, 7, 2);
/// assert!(replace.is_true_replace());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Replacement {
    /// Starting line index in the old sequence.
    pub old: usize,
    /// Number of lines deleted from old sequence.
    pub old_len: usize,
    /// Starting line index in the new sequence.
    pub new: usize,
    /// Number of lines inserted from new sequence.
    pub new_len: usize,
}

impl Replacement {
    /// Create a new replacement.
    pub fn new(old: usize, old_len: usize, new: usize, new_len: usize) -> Self {
        Self {
            old,
            old_len,
            new,
            new_len,
        }
    }

    /// Create a pure insertion (no deletion).
    pub fn insertion(old_pos: usize, new_pos: usize, len: usize) -> Self {
        Self::new(old_pos, 0, new_pos, len)
    }

    /// Create a pure deletion (no insertion).
    pub fn deletion(old_pos: usize, len: usize, new_pos: usize) -> Self {
        Self::new(old_pos, len, new_pos, 0)
    }

    /// Check if this is a pure insertion (no deletion).
    pub fn is_insert_only(&self) -> bool {
        self.old_len == 0 && self.new_len > 0
    }

    /// Check if this is a pure deletion (no insertion).
    pub fn is_delete_only(&self) -> bool {
        self.old_len > 0 && self.new_len == 0
    }

    /// Check if this is a true replacement (both deletion and insertion).
    pub fn is_true_replace(&self) -> bool {
        self.old_len > 0 && self.new_len > 0
    }

    /// Check if this replacement has any effect.
    pub fn is_empty(&self) -> bool {
        self.old_len == 0 && self.new_len == 0
    }

    /// Get the range of lines affected in the old sequence.
    pub fn old_range(&self) -> Range<usize> {
        self.old..self.old + self.old_len
    }

    /// Get the range of lines affected in the new sequence.
    pub fn new_range(&self) -> Range<usize> {
        self.new..self.new + self.new_len
    }
}

impl fmt::Display for Replacement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_insert_only() {
            write!(f, "+{}@{}", self.new_len, self.old)
        } else if self.is_delete_only() {
            write!(f, "-{}@{}", self.old_len, self.old)
        } else {
            write!(f, "-{}+{}@{}", self.old_len, self.new_len, self.old)
        }
    }
}

/// The complete result of a diff operation.
///
/// Contains the list of operations and provides methods for querying
/// statistics and iterating over changes.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::{diff_text, Algorithm};
///
/// let result = diff_text(b"a\nb\nc\n", b"a\nx\nc\n", Algorithm::Myers);
///
/// println!("Changes: {}", result.len());
/// println!("Insertions: {}", result.insertions());
/// println!("Deletions: {}", result.deletions());
///
/// for op in result.iter() {
///     println!("{}", op);
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct DiffResult {
    /// The list of diff operations.
    ops: Vec<DiffOp>,
}

impl DiffResult {
    /// Create an empty diff result.
    pub fn new() -> Self {
        Self { ops: Vec::new() }
    }

    /// Create a diff result with the given operations.
    pub fn with_ops(ops: Vec<DiffOp>) -> Self {
        Self { ops }
    }

    /// Create a diff result representing all-equal sequences.
    pub fn equal(len: usize) -> Self {
        if len == 0 {
            Self::new()
        } else {
            Self::with_ops(vec![DiffOp::equal(0, 0, len)])
        }
    }

    /// Add an operation to the result.
    pub fn push(&mut self, op: DiffOp) {
        self.ops.push(op);
    }

    /// Get the number of operations.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Check if there are no operations.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Check if the sequences are unchanged (all Equal operations).
    pub fn is_unchanged(&self) -> bool {
        self.ops.iter().all(|op| op.is_equal())
    }

    /// Get the total number of deleted lines.
    pub fn deletions(&self) -> usize {
        self.ops.iter().map(|op| op.deletions()).sum()
    }

    /// Get the total number of inserted lines.
    pub fn insertions(&self) -> usize {
        self.ops.iter().map(|op| op.insertions()).sum()
    }

    /// Get the edit distance (total changes = insertions + deletions).
    pub fn edit_distance(&self) -> usize {
        self.insertions() + self.deletions()
    }

    /// Iterate over all operations.
    pub fn iter(&self) -> impl Iterator<Item = &DiffOp> {
        self.ops.iter()
    }

    /// Iterate over only the change operations (non-Equal).
    pub fn changes(&self) -> impl Iterator<Item = &DiffOp> {
        self.ops.iter().filter(|op| op.is_change())
    }

    /// Get the operations as a slice.
    pub fn ops(&self) -> &[DiffOp] {
        &self.ops
    }

    /// Take ownership of the operations.
    pub fn into_ops(self) -> Vec<DiffOp> {
        self.ops
    }

    /// Convert to a list of Replacement operations.
    ///
    /// This format is more suitable for generating hunks in the change format.
    pub fn to_replacements(&self) -> Vec<Replacement> {
        self.ops
            .iter()
            .filter_map(|op| match *op {
                DiffOp::Equal { .. } => None,
                DiffOp::Insert {
                    old_pos,
                    new_pos,
                    len,
                } => Some(Replacement::insertion(old_pos, new_pos, len)),
                DiffOp::Delete {
                    old_pos,
                    new_pos,
                    len,
                } => Some(Replacement::deletion(old_pos, len, new_pos)),
                DiffOp::Replace {
                    old_pos,
                    old_len,
                    new_pos,
                    new_len,
                } => Some(Replacement::new(old_pos, old_len, new_pos, new_len)),
            })
            .collect()
    }

    /// Adjust all operation offsets by adding to the positions.
    ///
    /// This is used after stripping common prefixes to restore correct positions.
    pub(crate) fn adjust_offsets(&mut self, offset: usize) {
        for op in &mut self.ops {
            match op {
                DiffOp::Equal {
                    old_pos, new_pos, ..
                } => {
                    *old_pos += offset;
                    *new_pos += offset;
                }
                DiffOp::Insert {
                    old_pos, new_pos, ..
                } => {
                    *old_pos += offset;
                    *new_pos += offset;
                }
                DiffOp::Delete {
                    old_pos, new_pos, ..
                } => {
                    *old_pos += offset;
                    *new_pos += offset;
                }
                DiffOp::Replace {
                    old_pos, new_pos, ..
                } => {
                    *old_pos += offset;
                    *new_pos += offset;
                }
            }
        }
    }

    /// Prepend an Equal operation at the start.
    pub(crate) fn prepend_equal(&mut self, old_pos: usize, len: usize) {
        self.ops.insert(0, DiffOp::equal(old_pos, old_pos, len));
    }

    /// Append an Equal operation at the end.
    pub(crate) fn append_equal(&mut self, old_pos: usize, new_pos: usize, len: usize) {
        self.ops.push(DiffOp::equal(old_pos, new_pos, len));
    }
}

impl IntoIterator for DiffResult {
    type Item = DiffOp;
    type IntoIter = std::vec::IntoIter<DiffOp>;

    fn into_iter(self) -> Self::IntoIter {
        self.ops.into_iter()
    }
}

impl<'a> IntoIterator for &'a DiffResult {
    type Item = &'a DiffOp;
    type IntoIter = std::slice::Iter<'a, DiffOp>;

    fn into_iter(self) -> Self::IntoIter {
        self.ops.iter()
    }
}

impl std::ops::Index<usize> for DiffResult {
    type Output = DiffOp;

    fn index(&self, index: usize) -> &Self::Output {
        &self.ops[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_op_equal() {
        let op = DiffOp::equal(0, 0, 5);
        assert!(op.is_equal());
        assert!(!op.is_change());
        assert_eq!(op.old_range(), 0..5);
        assert_eq!(op.new_range(), 0..5);
        assert_eq!(op.deletions(), 0);
        assert_eq!(op.insertions(), 0);
    }

    #[test]
    fn test_diff_op_insert() {
        let op = DiffOp::insert(3, 3, 2);
        assert!(op.is_insert());
        assert!(op.is_change());
        assert_eq!(op.old_range(), 3..3);
        assert_eq!(op.new_range(), 3..5);
        assert_eq!(op.deletions(), 0);
        assert_eq!(op.insertions(), 2);
    }

    #[test]
    fn test_diff_op_delete() {
        let op = DiffOp::delete(5, 5, 3);
        assert!(op.is_delete());
        assert!(op.is_change());
        assert_eq!(op.old_range(), 5..8);
        assert_eq!(op.new_range(), 5..5);
        assert_eq!(op.deletions(), 3);
        assert_eq!(op.insertions(), 0);
    }

    #[test]
    fn test_diff_op_replace() {
        let op = DiffOp::replace(2, 3, 2, 1);
        assert!(op.is_replace());
        assert!(op.is_change());
        assert_eq!(op.old_range(), 2..5);
        assert_eq!(op.new_range(), 2..3);
        assert_eq!(op.deletions(), 3);
        assert_eq!(op.insertions(), 1);
    }

    #[test]
    fn test_diff_op_display() {
        assert_eq!(
            format!("{}", DiffOp::equal(0, 0, 3)),
            "Equal(old=0..3, new=0..3)"
        );
        assert_eq!(
            format!("{}", DiffOp::insert(5, 5, 2)),
            "Insert(at old=5, new=5..7)"
        );
        assert_eq!(
            format!("{}", DiffOp::delete(3, 3, 2)),
            "Delete(old=3..5, at new=3)"
        );
        assert_eq!(
            format!("{}", DiffOp::replace(1, 2, 1, 3)),
            "Replace(old=1..3, new=1..4)"
        );
    }

    #[test]
    fn test_replacement_new() {
        let r = Replacement::new(10, 2, 10, 3);
        assert_eq!(r.old, 10);
        assert_eq!(r.old_len, 2);
        assert_eq!(r.new, 10);
        assert_eq!(r.new_len, 3);
    }

    #[test]
    fn test_replacement_insert_only() {
        let r = Replacement::insertion(5, 5, 3);
        assert!(r.is_insert_only());
        assert!(!r.is_delete_only());
        assert!(!r.is_true_replace());
        assert!(!r.is_empty());
    }

    #[test]
    fn test_replacement_delete_only() {
        let r = Replacement::deletion(5, 3, 5);
        assert!(!r.is_insert_only());
        assert!(r.is_delete_only());
        assert!(!r.is_true_replace());
        assert!(!r.is_empty());
    }

    #[test]
    fn test_replacement_true_replace() {
        let r = Replacement::new(5, 2, 5, 3);
        assert!(!r.is_insert_only());
        assert!(!r.is_delete_only());
        assert!(r.is_true_replace());
        assert!(!r.is_empty());
    }

    #[test]
    fn test_replacement_empty() {
        let r = Replacement::new(0, 0, 0, 0);
        assert!(r.is_empty());
    }

    #[test]
    fn test_replacement_display() {
        assert_eq!(format!("{}", Replacement::insertion(5, 5, 3)), "+3@5");
        assert_eq!(format!("{}", Replacement::deletion(5, 2, 5)), "-2@5");
        assert_eq!(format!("{}", Replacement::new(5, 2, 5, 3)), "-2+3@5");
    }

    #[test]
    fn test_diff_result_new() {
        let result = DiffResult::new();
        assert!(result.is_empty());
        assert!(result.is_unchanged());
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_diff_result_equal() {
        let result = DiffResult::equal(5);
        assert!(!result.is_empty());
        assert!(result.is_unchanged());
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_diff_result_statistics() {
        let mut result = DiffResult::new();
        result.push(DiffOp::equal(0, 0, 2));
        result.push(DiffOp::delete(2, 2, 1));
        result.push(DiffOp::insert(3, 2, 2));
        result.push(DiffOp::equal(3, 4, 1));

        assert_eq!(result.deletions(), 1);
        assert_eq!(result.insertions(), 2);
        assert_eq!(result.edit_distance(), 3);
        assert!(!result.is_unchanged());
    }

    #[test]
    fn test_diff_result_changes() {
        let mut result = DiffResult::new();
        result.push(DiffOp::equal(0, 0, 2));
        result.push(DiffOp::delete(2, 2, 1));
        result.push(DiffOp::equal(3, 2, 1));

        let changes: Vec<_> = result.changes().collect();
        assert_eq!(changes.len(), 1);
        assert!(changes[0].is_delete());
    }

    #[test]
    fn test_diff_result_to_replacements() {
        let mut result = DiffResult::new();
        result.push(DiffOp::equal(0, 0, 2));
        result.push(DiffOp::replace(2, 1, 2, 2));
        result.push(DiffOp::equal(3, 4, 1));

        let replacements = result.to_replacements();
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacements[0].old, 2);
        assert_eq!(replacements[0].old_len, 1);
        assert_eq!(replacements[0].new_len, 2);
    }

    #[test]
    fn test_diff_result_iteration() {
        let mut result = DiffResult::new();
        result.push(DiffOp::equal(0, 0, 1));
        result.push(DiffOp::insert(1, 1, 1));

        // Borrow iteration
        let ops: Vec<_> = result.iter().collect();
        assert_eq!(ops.len(), 2);

        // Into iteration
        let ops: Vec<_> = result.into_iter().collect();
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn test_diff_result_index() {
        let mut result = DiffResult::new();
        result.push(DiffOp::equal(0, 0, 1));
        result.push(DiffOp::insert(1, 1, 1));

        assert!(result[0].is_equal());
        assert!(result[1].is_insert());
    }

    #[test]
    fn test_adjust_offsets() {
        let mut result = DiffResult::new();
        result.push(DiffOp::equal(0, 0, 1));
        result.push(DiffOp::insert(1, 1, 1));

        result.adjust_offsets(10);

        match &result[0] {
            DiffOp::Equal { old_pos, new_pos, .. } => {
                assert_eq!(*old_pos, 10);
                assert_eq!(*new_pos, 10);
            }
            _ => panic!("Expected Equal"),
        }

        match &result[1] {
            DiffOp::Insert { old_pos, new_pos, .. } => {
                assert_eq!(*old_pos, 11);
                assert_eq!(*new_pos, 11);
            }
            _ => panic!("Expected Insert"),
        }
    }
}

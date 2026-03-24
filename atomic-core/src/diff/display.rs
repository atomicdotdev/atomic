//! Display utilities for rendering diffs in UI contexts.
//!
//! This module provides data structures and iterators for presenting diffs
//! in human-readable formats, suitable for:
//!
//! - Terminal output (unified diff format)
//! - Code review UIs (side-by-side view)
//! - Web interfaces (inline highlighting)
//!
//! # Display Formats
//!
//! ## Unified View
//!
//! The traditional diff format where changes are shown inline with the full file:
//!
//! ```text
//!   1 | fn main() {
//!   2 |     let x = 1;
//! - 3 |     let y = 2;
//! + 3 |     let y = 3;
//!   4 | }
//! ```
//!
//! ## Side-by-Side View
//!
//! Shows old and new versions in parallel columns:
//!
//! ```text
//! Old                      | New
//! -------------------------+-------------------------
//! fn main() {              | fn main() {
//!     let x = 1;           |     let x = 1;
//!     let y = 2;           |     let y = 3;
//! }                        | }
//! ```
//!
//! # Usage
//!
//! ```rust
//! use atomic_core::diff::{diff_text, Algorithm};
//! use atomic_core::diff::display::{UnifiedDiff, DisplayLine, LineStatus};
//!
//! let old = b"line1\nline2\nline3\n";
//! let new = b"line1\nmodified\nline3\n";
//!
//! let unified = UnifiedDiff::new(old, new, Algorithm::Myers);
//!
//! for line in unified.lines() {
//!     match line.status {
//!         LineStatus::Unchanged => print!("  "),
//!         LineStatus::Added => print!("+ "),
//!         LineStatus::Removed => print!("- "),
//!     }
//!     println!("{}", line.content);
//! }
//! ```
//!
//! # Line Numbers
//!
//! Each [`DisplayLine`] includes both old and new line numbers where applicable:
//!
//! - **Unchanged**: Both `old_line_num` and `new_line_num` are `Some`
//! - **Added**: Only `new_line_num` is `Some`
//! - **Removed**: Only `old_line_num` is `Some`
//!
//! This enables UIs to show proper line number gutters on both sides.

use std::fmt;

use super::line::Line;
use super::ops::{DiffOp, DiffResult};
use super::{diff, Algorithm};

/// The status of a line in a diff display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineStatus {
    /// Line is unchanged (appears in both old and new).
    Unchanged,
    /// Line was added (appears only in new).
    Added,
    /// Line was removed (appears only in old).
    Removed,
}

impl LineStatus {
    /// Get the conventional prefix character for this status.
    ///
    /// - Unchanged: ` ` (space)
    /// - Added: `+`
    /// - Removed: `-`
    pub fn prefix(&self) -> char {
        match self {
            LineStatus::Unchanged => ' ',
            LineStatus::Added => '+',
            LineStatus::Removed => '-',
        }
    }

    /// Check if this line represents a change.
    pub fn is_change(&self) -> bool {
        !matches!(self, LineStatus::Unchanged)
    }
}

impl fmt::Display for LineStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.prefix())
    }
}

/// A single line in a diff display with metadata.
///
/// This struct contains all the information needed to render a line
/// in any diff format (unified, side-by-side, etc.).
#[derive(Debug, Clone)]
pub struct DisplayLine<'a> {
    /// The status of this line (unchanged, added, or removed).
    pub status: LineStatus,

    /// The content of the line (without trailing newline for display).
    pub content: &'a str,

    /// The raw bytes of the line (useful for binary content).
    pub raw: &'a [u8],

    /// Line number in the old file (1-based), if applicable.
    ///
    /// - `Some(n)` for unchanged and removed lines
    /// - `None` for added lines
    pub old_line_num: Option<usize>,

    /// Line number in the new file (1-based), if applicable.
    ///
    /// - `Some(n)` for unchanged and added lines
    /// - `None` for removed lines
    pub new_line_num: Option<usize>,

    /// Whether this line ends with a newline in the original content.
    pub has_newline: bool,
}

impl<'a> DisplayLine<'a> {
    /// Create a new display line.
    fn new(
        status: LineStatus,
        raw: &'a [u8],
        old_line_num: Option<usize>,
        new_line_num: Option<usize>,
    ) -> Self {
        // Strip trailing newline for display
        let (content_bytes, has_newline) = if raw.last() == Some(&b'\n') {
            (&raw[..raw.len() - 1], true)
        } else {
            (raw, false)
        };

        // Also strip \r if present (CRLF)
        let content_bytes = if content_bytes.last() == Some(&b'\r') {
            &content_bytes[..content_bytes.len() - 1]
        } else {
            content_bytes
        };

        // Try to interpret as UTF-8, fall back to lossy conversion
        let content = std::str::from_utf8(content_bytes).unwrap_or("");

        Self {
            status,
            content,
            raw,
            old_line_num,
            new_line_num,
            has_newline,
        }
    }

    /// Check if this line contains binary (non-UTF8) content.
    pub fn is_binary(&self) -> bool {
        self.content.is_empty() && !self.raw.is_empty()
    }

    /// Get a display-friendly representation of the content.
    ///
    /// For binary content, returns a placeholder like `[binary data: 42 bytes]`.
    pub fn display_content(&self) -> std::borrow::Cow<'a, str> {
        if self.is_binary() {
            std::borrow::Cow::Owned(format!("[binary data: {} bytes]", self.raw.len()))
        } else {
            std::borrow::Cow::Borrowed(self.content)
        }
    }
}

impl<'a> fmt::Display for DisplayLine<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status.prefix(), self.display_content())
    }
}

/// A unified diff view that iterates through all lines with their status.
///
/// This is the primary way to display a diff for UI purposes. It produces
/// a linear sequence of lines, each annotated with whether it's unchanged,
/// added, or removed.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::{Algorithm, display::UnifiedDiff};
///
/// let old = b"apple\nbanana\ncherry\n";
/// let new = b"apple\nblueberry\ncherry\n";
///
/// let diff = UnifiedDiff::new(old, new, Algorithm::Myers);
///
/// for line in diff.lines() {
///     println!("{}", line);
/// }
/// // Output:
/// //   apple
/// // - banana
/// // + blueberry
/// //   cherry
/// ```
#[allow(dead_code)]
pub struct UnifiedDiff<'a> {
    /// The original (old) content.
    old_content: &'a [u8],
    /// The modified (new) content.
    new_content: &'a [u8],
    /// The diff result containing operations.
    diff_result: DiffResult,
    /// Lines from the old content.
    old_lines: Vec<Line<'a>>,
    /// Lines from the new content.
    new_lines: Vec<Line<'a>>,
}

impl<'a> UnifiedDiff<'a> {
    /// Create a new unified diff from two byte slices.
    ///
    /// # Arguments
    ///
    /// * `old` - The original content
    /// * `new` - The modified content
    /// * `algorithm` - Which diff algorithm to use
    pub fn new(old: &'a [u8], new: &'a [u8], algorithm: Algorithm) -> Self {
        let old_lines = Line::from_bytes(old);
        let new_lines = Line::from_bytes(new);
        let diff_result = diff(&old_lines, &new_lines, algorithm);

        Self {
            old_content: old,
            new_content: new,
            diff_result,
            old_lines,
            new_lines,
        }
    }

    /// Create a unified diff from pre-computed diff result.
    ///
    /// This is useful when you've already computed the diff and want
    /// to display it without re-computing.
    pub fn from_result(
        old: &'a [u8],
        new: &'a [u8],
        old_lines: Vec<Line<'a>>,
        new_lines: Vec<Line<'a>>,
        diff_result: DiffResult,
    ) -> Self {
        Self {
            old_content: old,
            new_content: new,
            diff_result,
            old_lines,
            new_lines,
        }
    }

    /// Get the underlying diff result.
    pub fn diff_result(&self) -> &DiffResult {
        &self.diff_result
    }

    /// Check if there are any changes.
    pub fn has_changes(&self) -> bool {
        !self.diff_result.is_unchanged()
    }

    /// Get the number of lines added.
    pub fn additions(&self) -> usize {
        self.diff_result.insertions()
    }

    /// Get the number of lines removed.
    pub fn deletions(&self) -> usize {
        self.diff_result.deletions()
    }

    /// Iterate through all lines with their display status.
    ///
    /// Lines are yielded in the order they would appear in a unified diff:
    /// - Unchanged lines appear once
    /// - Removed lines appear with status `Removed`
    /// - Added lines appear with status `Added`
    /// - For replacements, removed lines come first, then added lines
    pub fn lines(&'a self) -> UnifiedDiffIter<'a> {
        UnifiedDiffIter::new(self)
    }

    /// Get the total number of lines that will be yielded.
    ///
    /// This equals: unchanged + removed + added
    pub fn total_lines(&self) -> usize {
        let mut count = 0;
        for op in self.diff_result.iter() {
            match op {
                DiffOp::Equal { len, .. } => count += len,
                DiffOp::Insert { len, .. } => count += len,
                DiffOp::Delete { len, .. } => count += len,
                DiffOp::Replace {
                    old_len, new_len, ..
                } => count += old_len + new_len,
            }
        }
        count
    }
}

/// Iterator over lines in a unified diff.
pub struct UnifiedDiffIter<'a> {
    diff: &'a UnifiedDiff<'a>,
    /// Current operation index.
    op_idx: usize,
    /// Position within current operation.
    pos_in_op: usize,
    /// Current line number in old content (1-based).
    old_line_num: usize,
    /// Current line number in new content (1-based).
    new_line_num: usize,
    /// For Replace ops, are we in the delete phase?
    in_delete_phase: bool,
}

impl<'a> UnifiedDiffIter<'a> {
    fn new(diff: &'a UnifiedDiff<'a>) -> Self {
        Self {
            diff,
            op_idx: 0,
            pos_in_op: 0,
            old_line_num: 1,
            new_line_num: 1,
            in_delete_phase: true,
        }
    }
}

impl<'a> Iterator for UnifiedDiffIter<'a> {
    type Item = DisplayLine<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.op_idx >= self.diff.diff_result.len() {
                return None;
            }

            let op = &self.diff.diff_result.ops()[self.op_idx];

            match op {
                DiffOp::Equal {
                    old_pos,
                    new_pos: _,
                    len,
                } => {
                    if self.pos_in_op < *len {
                        let line_idx = old_pos + self.pos_in_op;
                        let raw = self.diff.old_lines.get(line_idx)?.content();

                        let display_line = DisplayLine::new(
                            LineStatus::Unchanged,
                            raw,
                            Some(self.old_line_num),
                            Some(self.new_line_num),
                        );

                        self.pos_in_op += 1;
                        self.old_line_num += 1;
                        self.new_line_num += 1;

                        return Some(display_line);
                    }
                }

                DiffOp::Delete { old_pos, len, .. } => {
                    if self.pos_in_op < *len {
                        let line_idx = old_pos + self.pos_in_op;
                        let raw = self.diff.old_lines.get(line_idx)?.content();

                        let display_line = DisplayLine::new(
                            LineStatus::Removed,
                            raw,
                            Some(self.old_line_num),
                            None,
                        );

                        self.pos_in_op += 1;
                        self.old_line_num += 1;

                        return Some(display_line);
                    }
                }

                DiffOp::Insert { new_pos, len, .. } => {
                    if self.pos_in_op < *len {
                        let line_idx = new_pos + self.pos_in_op;
                        let raw = self.diff.new_lines.get(line_idx)?.content();

                        let display_line =
                            DisplayLine::new(LineStatus::Added, raw, None, Some(self.new_line_num));

                        self.pos_in_op += 1;
                        self.new_line_num += 1;

                        return Some(display_line);
                    }
                }

                DiffOp::Replace {
                    old_pos,
                    old_len,
                    new_pos,
                    new_len,
                } => {
                    // First emit all deleted lines, then all inserted lines
                    if self.in_delete_phase {
                        if self.pos_in_op < *old_len {
                            let line_idx = old_pos + self.pos_in_op;
                            let raw = self.diff.old_lines.get(line_idx)?.content();

                            let display_line = DisplayLine::new(
                                LineStatus::Removed,
                                raw,
                                Some(self.old_line_num),
                                None,
                            );

                            self.pos_in_op += 1;
                            self.old_line_num += 1;

                            return Some(display_line);
                        } else {
                            // Switch to insert phase
                            self.in_delete_phase = false;
                            self.pos_in_op = 0;
                        }
                    }

                    if !self.in_delete_phase && self.pos_in_op < *new_len {
                        let line_idx = new_pos + self.pos_in_op;
                        let raw = self.diff.new_lines.get(line_idx)?.content();

                        let display_line =
                            DisplayLine::new(LineStatus::Added, raw, None, Some(self.new_line_num));

                        self.pos_in_op += 1;
                        self.new_line_num += 1;

                        return Some(display_line);
                    }
                }
            }

            // Move to next operation
            self.op_idx += 1;
            self.pos_in_op = 0;
            self.in_delete_phase = true;
        }
    }
}

/// A side-by-side diff view that pairs old and new lines together.
///
/// This is useful for UIs that want to show old and new content in
/// parallel columns.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::{Algorithm, display::SideBySideDiff};
///
/// let old = b"apple\nbanana\ncherry\n";
/// let new = b"apple\nblueberry\ncherry\n";
///
/// let diff = SideBySideDiff::new(old, new, Algorithm::Myers);
///
/// for pair in diff.pairs() {
///     match (pair.old_line, pair.new_line) {
///         (Some(old), Some(new)) => println!("{:<20} | {}", old.content, new.content),
///         (Some(old), None) => println!("{:<20} | ", old.content),
///         (None, Some(new)) => println!("{:<20} | {}", "", new.content),
///         (None, None) => unreachable!(),
///     }
/// }
/// ```
pub struct SideBySideDiff<'a> {
    unified: UnifiedDiff<'a>,
}

impl<'a> SideBySideDiff<'a> {
    /// Create a new side-by-side diff.
    pub fn new(old: &'a [u8], new: &'a [u8], algorithm: Algorithm) -> Self {
        Self {
            unified: UnifiedDiff::new(old, new, algorithm),
        }
    }

    /// Get the underlying unified diff.
    pub fn unified(&self) -> &UnifiedDiff<'a> {
        &self.unified
    }

    /// Iterate through line pairs.
    pub fn pairs(&'a self) -> SideBySideIter<'a> {
        SideBySideIter::new(self)
    }
}

/// A pair of lines for side-by-side display.
#[derive(Debug, Clone)]
pub struct LinePair<'a> {
    /// The line from the old content, if any.
    pub old_line: Option<DisplayLine<'a>>,
    /// The line from the new content, if any.
    pub new_line: Option<DisplayLine<'a>>,
}

impl<'a> LinePair<'a> {
    /// Check if this pair represents a change.
    pub fn is_change(&self) -> bool {
        match (&self.old_line, &self.new_line) {
            (Some(old), Some(new)) => old.status.is_change() || new.status.is_change(),
            _ => true,
        }
    }

    /// Check if this pair represents unchanged content.
    pub fn is_unchanged(&self) -> bool {
        matches!(
            (&self.old_line, &self.new_line),
            (Some(old), Some(new)) if old.status == LineStatus::Unchanged && new.status == LineStatus::Unchanged
        )
    }
}

/// Iterator over line pairs in a side-by-side diff.
pub struct SideBySideIter<'a> {
    diff: &'a SideBySideDiff<'a>,
    op_idx: usize,
    pos_in_op: usize,
    old_line_num: usize,
    new_line_num: usize,
    /// For Replace: buffered deleted lines.
    delete_buffer: Vec<DisplayLine<'a>>,
    /// For Replace: buffered inserted lines.
    insert_buffer: Vec<DisplayLine<'a>>,
    /// Current position in buffers for Replace.
    buffer_pos: usize,
}

impl<'a> SideBySideIter<'a> {
    fn new(diff: &'a SideBySideDiff<'a>) -> Self {
        Self {
            diff,
            op_idx: 0,
            pos_in_op: 0,
            old_line_num: 1,
            new_line_num: 1,
            delete_buffer: Vec::new(),
            insert_buffer: Vec::new(),
            buffer_pos: 0,
        }
    }
}

impl<'a> Iterator for SideBySideIter<'a> {
    type Item = LinePair<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        // First, drain any buffered replace pairs
        if !self.delete_buffer.is_empty() || !self.insert_buffer.is_empty() {
            let old_line = self.delete_buffer.get(self.buffer_pos).cloned();
            let new_line = self.insert_buffer.get(self.buffer_pos).cloned();

            self.buffer_pos += 1;

            // Check if we've exhausted the buffers
            if self.buffer_pos >= self.delete_buffer.len().max(self.insert_buffer.len()) {
                self.delete_buffer.clear();
                self.insert_buffer.clear();
                self.buffer_pos = 0;
            }

            if old_line.is_some() || new_line.is_some() {
                return Some(LinePair { old_line, new_line });
            }
        }

        loop {
            if self.op_idx >= self.diff.unified.diff_result.len() {
                return None;
            }

            let op = &self.diff.unified.diff_result.ops()[self.op_idx];

            match op {
                DiffOp::Equal {
                    old_pos,
                    new_pos,
                    len,
                } => {
                    if self.pos_in_op < *len {
                        let old_idx = old_pos + self.pos_in_op;
                        let new_idx = new_pos + self.pos_in_op;

                        let old_raw = self.diff.unified.old_lines.get(old_idx)?.content();
                        let new_raw = self.diff.unified.new_lines.get(new_idx)?.content();

                        let old_line = DisplayLine::new(
                            LineStatus::Unchanged,
                            old_raw,
                            Some(self.old_line_num),
                            Some(self.new_line_num),
                        );
                        let new_line = DisplayLine::new(
                            LineStatus::Unchanged,
                            new_raw,
                            Some(self.old_line_num),
                            Some(self.new_line_num),
                        );

                        self.pos_in_op += 1;
                        self.old_line_num += 1;
                        self.new_line_num += 1;

                        return Some(LinePair {
                            old_line: Some(old_line),
                            new_line: Some(new_line),
                        });
                    }
                }

                DiffOp::Delete { old_pos, len, .. } => {
                    if self.pos_in_op < *len {
                        let line_idx = old_pos + self.pos_in_op;
                        let raw = self.diff.unified.old_lines.get(line_idx)?.content();

                        let old_line = DisplayLine::new(
                            LineStatus::Removed,
                            raw,
                            Some(self.old_line_num),
                            None,
                        );

                        self.pos_in_op += 1;
                        self.old_line_num += 1;

                        return Some(LinePair {
                            old_line: Some(old_line),
                            new_line: None,
                        });
                    }
                }

                DiffOp::Insert { new_pos, len, .. } => {
                    if self.pos_in_op < *len {
                        let line_idx = new_pos + self.pos_in_op;
                        let raw = self.diff.unified.new_lines.get(line_idx)?.content();

                        let new_line =
                            DisplayLine::new(LineStatus::Added, raw, None, Some(self.new_line_num));

                        self.pos_in_op += 1;
                        self.new_line_num += 1;

                        return Some(LinePair {
                            old_line: None,
                            new_line: Some(new_line),
                        });
                    }
                }

                DiffOp::Replace {
                    old_pos,
                    old_len,
                    new_pos,
                    new_len,
                } => {
                    // Buffer all deleted and inserted lines, then pair them up
                    if self.delete_buffer.is_empty() && self.insert_buffer.is_empty() {
                        for i in 0..*old_len {
                            let line_idx = old_pos + i;
                            if let Some(line) = self.diff.unified.old_lines.get(line_idx) {
                                self.delete_buffer.push(DisplayLine::new(
                                    LineStatus::Removed,
                                    line.content(),
                                    Some(self.old_line_num + i),
                                    None,
                                ));
                            }
                        }
                        self.old_line_num += old_len;

                        for i in 0..*new_len {
                            let line_idx = new_pos + i;
                            if let Some(line) = self.diff.unified.new_lines.get(line_idx) {
                                self.insert_buffer.push(DisplayLine::new(
                                    LineStatus::Added,
                                    line.content(),
                                    None,
                                    Some(self.new_line_num + i),
                                ));
                            }
                        }
                        self.new_line_num += new_len;

                        self.buffer_pos = 0;

                        // Move to next operation and recurse to drain buffer
                        self.op_idx += 1;
                        self.pos_in_op = 0;
                        return self.next();
                    }
                }
            }

            // Move to next operation
            self.op_idx += 1;
            self.pos_in_op = 0;
        }
    }
}

/// Statistics about a diff for display purposes.
#[derive(Debug, Clone, Default)]
pub struct DiffStats {
    /// Number of files changed.
    pub files_changed: usize,
    /// Total lines added across all files.
    pub lines_added: usize,
    /// Total lines removed across all files.
    pub lines_removed: usize,
}

impl DiffStats {
    /// Create stats from a single diff.
    pub fn from_diff(diff: &DiffResult) -> Self {
        Self {
            files_changed: 1,
            lines_added: diff.insertions(),
            lines_removed: diff.deletions(),
        }
    }

    /// Merge another stats into this one.
    pub fn merge(&mut self, other: &DiffStats) {
        self.files_changed += other.files_changed;
        self.lines_added += other.lines_added;
        self.lines_removed += other.lines_removed;
    }
}

impl fmt::Display for DiffStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} file(s) changed, {} insertion(s)(+), {} deletion(s)(-)",
            self.files_changed, self.lines_added, self.lines_removed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_line_status_prefix() {
        assert_eq!(LineStatus::Unchanged.prefix(), ' ');
        assert_eq!(LineStatus::Added.prefix(), '+');
        assert_eq!(LineStatus::Removed.prefix(), '-');
    }

    #[test]
    fn test_line_status_is_change() {
        assert!(!LineStatus::Unchanged.is_change());
        assert!(LineStatus::Added.is_change());
        assert!(LineStatus::Removed.is_change());
    }

    #[test]
    fn test_display_line_creation() {
        let line = DisplayLine::new(LineStatus::Unchanged, b"hello\n", Some(1), Some(1));
        assert_eq!(line.content, "hello");
        assert!(line.has_newline);
        assert_eq!(line.old_line_num, Some(1));
        assert_eq!(line.new_line_num, Some(1));
    }

    #[test]
    fn test_display_line_crlf() {
        let line = DisplayLine::new(LineStatus::Unchanged, b"hello\r\n", Some(1), Some(1));
        assert_eq!(line.content, "hello");
        assert!(line.has_newline);
    }

    #[test]
    fn test_display_line_no_newline() {
        let line = DisplayLine::new(LineStatus::Unchanged, b"hello", Some(1), Some(1));
        assert_eq!(line.content, "hello");
        assert!(!line.has_newline);
    }

    #[test]
    fn test_unified_diff_identical() {
        let content = b"line1\nline2\nline3\n";
        let diff = UnifiedDiff::new(content, content, Algorithm::Myers);

        assert!(!diff.has_changes());
        assert_eq!(diff.additions(), 0);
        assert_eq!(diff.deletions(), 0);

        let lines: Vec<_> = diff.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in lines {
            assert_eq!(line.status, LineStatus::Unchanged);
        }
    }

    #[test]
    fn test_unified_diff_addition() {
        let old = b"line1\nline3\n";
        let new = b"line1\nline2\nline3\n";
        let diff = UnifiedDiff::new(old, new, Algorithm::Myers);

        assert!(diff.has_changes());
        assert_eq!(diff.additions(), 1);
        assert_eq!(diff.deletions(), 0);

        let lines: Vec<_> = diff.lines().collect();
        // 2 unchanged + 1 added = 3 lines
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].status, LineStatus::Unchanged);
        assert_eq!(lines[1].status, LineStatus::Added);
        assert_eq!(lines[2].status, LineStatus::Unchanged);
    }

    #[test]
    fn test_unified_diff_deletion() {
        let old = b"line1\nline2\nline3\n";
        let new = b"line1\nline3\n";
        let diff = UnifiedDiff::new(old, new, Algorithm::Myers);

        assert!(diff.has_changes());
        assert_eq!(diff.additions(), 0);
        assert_eq!(diff.deletions(), 1);

        let lines: Vec<_> = diff.lines().collect();
        // 2 unchanged + 1 removed = 3 lines
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].status, LineStatus::Unchanged);
        assert_eq!(lines[1].status, LineStatus::Removed);
        assert_eq!(lines[2].status, LineStatus::Unchanged);
    }

    #[test]
    fn test_unified_diff_replacement() {
        let old = b"line1\nold\nline3\n";
        let new = b"line1\nnew\nline3\n";
        let diff = UnifiedDiff::new(old, new, Algorithm::Myers);

        assert!(diff.has_changes());

        let lines: Vec<_> = diff.lines().collect();
        // Should be: unchanged, removed, added, unchanged
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].status, LineStatus::Unchanged);
        assert_eq!(lines[1].status, LineStatus::Removed);
        assert_eq!(lines[2].status, LineStatus::Added);
        assert_eq!(lines[3].status, LineStatus::Unchanged);
    }

    #[test]
    fn test_unified_diff_line_numbers() {
        let old = b"a\nb\nc\n";
        let new = b"a\nx\nc\n";
        let diff = UnifiedDiff::new(old, new, Algorithm::Myers);

        let lines: Vec<_> = diff.lines().collect();

        // First line: unchanged, old=1, new=1
        assert_eq!(lines[0].old_line_num, Some(1));
        assert_eq!(lines[0].new_line_num, Some(1));

        // Second line: removed, old=2, new=None
        assert_eq!(lines[1].old_line_num, Some(2));
        assert_eq!(lines[1].new_line_num, None);

        // Third line: added, old=None, new=2
        assert_eq!(lines[2].old_line_num, None);
        assert_eq!(lines[2].new_line_num, Some(2));

        // Fourth line: unchanged, old=3, new=3
        assert_eq!(lines[3].old_line_num, Some(3));
        assert_eq!(lines[3].new_line_num, Some(3));
    }

    #[test]
    fn test_side_by_side_identical() {
        let content = b"line1\nline2\n";
        let diff = SideBySideDiff::new(content, content, Algorithm::Myers);

        let pairs: Vec<_> = diff.pairs().collect();
        assert_eq!(pairs.len(), 2);

        for pair in pairs {
            assert!(pair.old_line.is_some());
            assert!(pair.new_line.is_some());
            assert!(!pair.is_change());
            assert!(pair.is_unchanged());
        }
    }

    #[test]
    fn test_side_by_side_changes() {
        let old = b"a\nb\nc\n";
        let new = b"a\nx\nc\n";
        let diff = SideBySideDiff::new(old, new, Algorithm::Myers);

        let pairs: Vec<_> = diff.pairs().collect();

        // First pair: unchanged
        assert!(pairs[0].is_unchanged());

        // Second pair: old has 'b', new has 'x' (or they're separate)
        // The exact pairing depends on how Replace is handled
        assert!(pairs.iter().any(|p| p.is_change()));
    }

    #[test]
    fn test_diff_stats() {
        let old = b"a\nb\nc\n";
        let new = b"a\nx\ny\nc\n";
        let diff = UnifiedDiff::new(old, new, Algorithm::Myers);

        let stats = DiffStats::from_diff(diff.diff_result());
        assert_eq!(stats.files_changed, 1);
        assert!(stats.lines_added > 0 || stats.lines_removed > 0);
    }

    #[test]
    fn test_diff_stats_display() {
        let stats = DiffStats {
            files_changed: 3,
            lines_added: 10,
            lines_removed: 5,
        };
        let s = format!("{}", stats);
        assert!(s.contains("3 file"));
        assert!(s.contains("10 insertion"));
        assert!(s.contains("5 deletion"));
    }

    #[test]
    fn test_display_line_display_trait() {
        let line = DisplayLine::new(LineStatus::Added, b"hello\n", None, Some(1));
        let s = format!("{}", line);
        assert!(s.contains("+"));
        assert!(s.contains("hello"));
    }

    #[test]
    fn test_total_lines() {
        let old = b"a\nb\nc\n";
        let new = b"a\nx\ny\nc\n";
        let diff = UnifiedDiff::new(old, new, Algorithm::Myers);

        let total = diff.total_lines();
        let actual: Vec<_> = diff.lines().collect();
        assert_eq!(actual.len(), total);
    }
}

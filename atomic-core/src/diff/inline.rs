//! Inline diff display for code reviews with word-level highlighting.
//!
//! This module provides types and utilities for rendering word-level diffs
//! with the two-tier highlighting pattern used in modern code review tools:
//!
//! - **Light background**: Shows that a line changed (light red or light green)
//! - **Dark highlight**: Shows exactly which words/tokens changed within the line
//!
//! # Visual Pattern
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │ - const result = calculateSum(a, b);        <- light red background      │
//! │ + const result = calculateSum(a, b, c);     <- light green background    │
//! │                                   ^^^^      <- dark green: ", c" added   │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Use Cases
//!
//! - Terminal diff output with ANSI colors
//! - Web-based code review interfaces
//! - IDE diff viewers
//! - Git-style patch generation
//!
//! # Example
//!
//! ```rust
//! use atomic_core::diff::inline::{InlineDiff, compute_inline_diff};
//!
//! let old_line = b"const x = 1;";
//! let new_line = b"const x = 2;";
//!
//! let diff = compute_inline_diff(old_line, new_line);
//!
//! // Get highlighted hunks for the old line (deletions)
//! for hunk in diff.old_hunks() {
//!     println!("Deleted at {}..{}: {:?}",
//!         hunk.start, hunk.end, hunk.kind);
//! }
//!
//! // Get highlighted hunks for the new line (insertions)
//! for hunk in diff.new_hunks() {
//!     println!("Added at {}..{}: {:?}",
//!         hunk.start, hunk.end, hunk.kind);
//! }
//! ```
//!
//! # Integration with Line-Level Diffs
//!
//! This module is designed to work with line-level diffs. The typical workflow:
//!
//! 1. Compute line-level diff to find changed lines
//! 2. For each changed line pair (Replace operation), compute inline diff
//! 3. Render with light background for the line, dark highlight for changed tokens
//!
//! ```rust,ignore
//! use atomic_core::diff::{diff_text, Algorithm, DiffOp};
//! use atomic_core::diff::inline::compute_inline_diff;
//!
//! let line_diff = diff_text(old_content, new_content, Algorithm::Myers);
//!
//! for op in line_diff.iter() {
//!     match op {
//!         DiffOp::Replace { old_pos, old_len, new_pos, new_len } => {
//!             // For replaced lines, compute word-level diff
//!             let inline = compute_inline_diff(old_line, new_line);
//!             // Render with highlighting
//!         }
//!         _ => { /* handle other cases */ }
//!     }
//! }
//! ```

use super::word::{WordDiffConfig, WordDiffOp, WordDiffResult};
use std::ops::Range;

/// The kind of change represented by a hunk.
///
/// This determines how the hunk should be highlighted in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HunkKind {
    /// Content that was deleted (appears in old, not in new).
    ///
    /// Typically rendered with dark red background/text.
    Deleted,

    /// Content that was inserted (appears in new, not in old).
    ///
    /// Typically rendered with dark green background/text.
    Inserted,

    /// Content that was modified (different in old and new).
    ///
    /// In the old line, render as deleted. In the new line, render as inserted.
    /// This is used for Replace operations where content changed.
    Modified,

    /// Content that is unchanged (context).
    ///
    /// No special highlighting needed beyond the line background.
    Unchanged,
}

impl HunkKind {
    /// Check if this hunk represents a change.
    #[inline]
    pub fn is_change(&self) -> bool {
        !matches!(self, HunkKind::Unchanged)
    }

    /// Check if this hunk represents deleted content.
    #[inline]
    pub fn is_deleted(&self) -> bool {
        matches!(self, HunkKind::Deleted | HunkKind::Modified)
    }

    /// Check if this hunk represents inserted content.
    #[inline]
    pub fn is_inserted(&self) -> bool {
        matches!(self, HunkKind::Inserted | HunkKind::Modified)
    }

    /// Get a display name for this hunk kind.
    pub fn name(&self) -> &'static str {
        match self {
            HunkKind::Deleted => "deleted",
            HunkKind::Inserted => "inserted",
            HunkKind::Modified => "modified",
            HunkKind::Unchanged => "unchanged",
        }
    }

    /// Get the ANSI color code for terminal display.
    ///
    /// Returns the escape sequence for the appropriate color.
    pub fn ansi_color(&self) -> &'static str {
        match self {
            HunkKind::Deleted => "\x1b[41m",  // Red background
            HunkKind::Inserted => "\x1b[42m", // Green background
            HunkKind::Modified => "\x1b[43m", // Yellow background
            HunkKind::Unchanged => "",        // No color
        }
    }

    /// Get the ANSI reset code.
    pub fn ansi_reset() -> &'static str {
        "\x1b[0m"
    }
}

impl std::fmt::Display for HunkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// A highlighted hunk within a line.
///
/// Represents a contiguous range of bytes that should be highlighted
/// in a particular way during rendering.
///
/// # Byte Ranges
///
/// The `start` and `end` fields are byte offsets into the original line
/// content. They can be used directly to slice the line:
///
/// ```rust
/// use atomic_core::diff::inline::{ChangeHunk, HunkKind};
///
/// let line = b"hello world";
/// let hunk = ChangeHunk::new(0, 5, HunkKind::Deleted);
///
/// let highlighted_content = &line[hunk.start..hunk.end];
/// assert_eq!(highlighted_content, b"hello");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeHunk {
    /// Start byte offset in the line (inclusive).
    pub start: usize,

    /// End byte offset in the line (exclusive).
    pub end: usize,

    /// The kind of change this hunk represents.
    pub kind: HunkKind,
}

impl ChangeHunk {
    /// Create a new change hunk.
    ///
    /// # Arguments
    ///
    /// * `start` - Start byte offset (inclusive)
    /// * `end` - End byte offset (exclusive)
    /// * `kind` - The kind of change
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::inline::{ChangeHunk, HunkKind};
    ///
    /// let hunk = ChangeHunk::new(10, 20, HunkKind::Inserted);
    /// assert_eq!(hunk.len(), 10);
    /// ```
    pub fn new(start: usize, end: usize, kind: HunkKind) -> Self {
        Self { start, end, kind }
    }

    /// Create a deleted hunk.
    pub fn deleted(start: usize, end: usize) -> Self {
        Self::new(start, end, HunkKind::Deleted)
    }

    /// Create an inserted hunk.
    pub fn inserted(start: usize, end: usize) -> Self {
        Self::new(start, end, HunkKind::Inserted)
    }

    /// Create a modified hunk.
    pub fn modified(start: usize, end: usize) -> Self {
        Self::new(start, end, HunkKind::Modified)
    }

    /// Create an unchanged hunk.
    pub fn unchanged(start: usize, end: usize) -> Self {
        Self::new(start, end, HunkKind::Unchanged)
    }

    /// Get the byte range as a Range.
    #[inline]
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    /// Get the length of this hunk in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// Check if this hunk is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    /// Check if this hunk represents a change.
    #[inline]
    pub fn is_change(&self) -> bool {
        self.kind.is_change()
    }

    /// Check if this hunk is adjacent to another hunk.
    ///
    /// Two hunks are adjacent if one ends where the other begins.
    pub fn is_adjacent_to(&self, other: &Self) -> bool {
        self.end == other.start || other.end == self.start
    }

    /// Try to merge with another hunk of the same kind.
    ///
    /// Returns `Some(merged)` if the hunks are adjacent and same kind,
    /// `None` otherwise.
    pub fn try_merge(&self, other: &Self) -> Option<Self> {
        if self.kind != other.kind {
            return None;
        }

        if self.end == other.start {
            Some(Self::new(self.start, other.end, self.kind))
        } else if other.end == self.start {
            Some(Self::new(other.start, self.end, self.kind))
        } else {
            None
        }
    }

    /// Extract the content of this hunk from a byte slice.
    ///
    /// # Panics
    ///
    /// Panics if the hunk range is out of bounds for the slice.
    pub fn extract<'a>(&self, content: &'a [u8]) -> &'a [u8] {
        &content[self.start..self.end]
    }

    /// Safely extract the content, returning None if out of bounds.
    pub fn try_extract<'a>(&self, content: &'a [u8]) -> Option<&'a [u8]> {
        content.get(self.start..self.end)
    }
}

impl std::fmt::Display for ChangeHunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}[{}..{}]", self.kind, self.start, self.end)
    }
}

/// Result of computing an inline diff between two lines.
///
/// Contains the highlighted hunks for both the old and new lines,
/// allowing rendering with appropriate highlighting.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::inline::compute_inline_diff;
///
/// let diff = compute_inline_diff(b"hello world", b"hello there");
///
/// // Render old line with deletion highlights
/// for hunk in diff.old_hunks() {
///     if hunk.is_change() {
///         print!("[DEL]");
///     }
/// }
///
/// // Render new line with insertion highlights
/// for hunk in diff.new_hunks() {
///     if hunk.is_change() {
///         print!("[INS]");
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct InlineDiff<'a> {
    /// The original old line content.
    old_content: &'a [u8],

    /// The original new line content.
    new_content: &'a [u8],

    /// Highlighted hunks in the old line.
    old_hunks: Vec<ChangeHunk>,

    /// Highlighted hunks in the new line.
    new_hunks: Vec<ChangeHunk>,

    /// Whether any changes were detected.
    has_changes: bool,
}

impl<'a> InlineDiff<'a> {
    /// Create a new inline diff with no changes.
    fn unchanged(old_content: &'a [u8], new_content: &'a [u8]) -> Self {
        let old_hunks = if !old_content.is_empty() {
            vec![ChangeHunk::unchanged(0, old_content.len())]
        } else {
            vec![]
        };

        let new_hunks = if !new_content.is_empty() {
            vec![ChangeHunk::unchanged(0, new_content.len())]
        } else {
            vec![]
        };

        Self {
            old_content,
            new_content,
            old_hunks,
            new_hunks,
            has_changes: false,
        }
    }

    /// Create an inline diff where the old line was completely deleted.
    fn all_deleted(old_content: &'a [u8], new_content: &'a [u8]) -> Self {
        let old_hunks = if !old_content.is_empty() {
            vec![ChangeHunk::deleted(0, old_content.len())]
        } else {
            vec![]
        };

        Self {
            old_content,
            new_content,
            old_hunks,
            new_hunks: vec![],
            has_changes: true,
        }
    }

    /// Create an inline diff where the new line was completely inserted.
    fn all_inserted(old_content: &'a [u8], new_content: &'a [u8]) -> Self {
        let new_hunks = if !new_content.is_empty() {
            vec![ChangeHunk::inserted(0, new_content.len())]
        } else {
            vec![]
        };

        Self {
            old_content,
            new_content,
            old_hunks: vec![],
            new_hunks,
            has_changes: true,
        }
    }

    /// Get the original old line content.
    #[inline]
    pub fn old_content(&self) -> &'a [u8] {
        self.old_content
    }

    /// Get the original new line content.
    #[inline]
    pub fn new_content(&self) -> &'a [u8] {
        self.new_content
    }

    /// Get the highlighted hunks for the old line.
    ///
    /// These hunks cover the entire old line and indicate which parts
    /// were deleted or modified.
    #[inline]
    pub fn old_hunks(&self) -> &[ChangeHunk] {
        &self.old_hunks
    }

    /// Get the highlighted hunks for the new line.
    ///
    /// These hunks cover the entire new line and indicate which parts
    /// were inserted or modified.
    #[inline]
    pub fn new_hunks(&self) -> &[ChangeHunk] {
        &self.new_hunks
    }

    /// Check if there are any changes between the lines.
    #[inline]
    pub fn has_changes(&self) -> bool {
        self.has_changes
    }

    /// Check if the lines are identical.
    #[inline]
    pub fn is_unchanged(&self) -> bool {
        !self.has_changes
    }

    /// Get only the changed hunks from the old line.
    pub fn old_changes(&self) -> impl Iterator<Item = &ChangeHunk> {
        self.old_hunks.iter().filter(|s| s.is_change())
    }

    /// Get only the changed hunks from the new line.
    pub fn new_changes(&self) -> impl Iterator<Item = &ChangeHunk> {
        self.new_hunks.iter().filter(|s| s.is_change())
    }

    /// Count the number of changed hunks in the old line.
    pub fn old_change_count(&self) -> usize {
        self.old_hunks.iter().filter(|s| s.is_change()).count()
    }

    /// Count the number of changed hunks in the new line.
    pub fn new_change_count(&self) -> usize {
        self.new_hunks.iter().filter(|s| s.is_change()).count()
    }

    /// Get the total number of bytes that were deleted.
    pub fn deleted_bytes(&self) -> usize {
        self.old_hunks
            .iter()
            .filter(|s| s.is_change())
            .map(|s| s.len())
            .sum()
    }

    /// Get the total number of bytes that were inserted.
    pub fn inserted_bytes(&self) -> usize {
        self.new_hunks
            .iter()
            .filter(|s| s.is_change())
            .map(|s| s.len())
            .sum()
    }

    /// Render the old line with ANSI highlighting for terminal output.
    ///
    /// Returns a string with ANSI escape codes for colored output.
    pub fn render_old_ansi(&self) -> String {
        self.render_with_ansi(self.old_content, &self.old_hunks, true)
    }

    /// Render the new line with ANSI highlighting for terminal output.
    ///
    /// Returns a string with ANSI escape codes for colored output.
    pub fn render_new_ansi(&self) -> String {
        self.render_with_ansi(self.new_content, &self.new_hunks, false)
    }

    /// Internal helper to render content with ANSI codes.
    fn render_with_ansi(&self, content: &[u8], hunks: &[ChangeHunk], is_old: bool) -> String {
        let mut result = String::new();

        for hunk in hunks {
            let text = String::from_utf8_lossy(hunk.extract(content));

            if hunk.is_change() {
                let color = if is_old {
                    "\x1b[91m" // Bright red for deletions
                } else {
                    "\x1b[92m" // Bright green for insertions
                };
                result.push_str(color);
                result.push_str(&text);
                result.push_str(HunkKind::ansi_reset());
            } else {
                result.push_str(&text);
            }
        }

        result
    }

    /// Render the old line with HTML highlighting.
    ///
    /// Returns HTML with `<span>` tags for styling.
    pub fn render_old_html(&self) -> String {
        self.render_with_html(self.old_content, &self.old_hunks, "deleted")
    }

    /// Render the new line with HTML highlighting.
    ///
    /// Returns HTML with `<span>` tags for styling.
    pub fn render_new_html(&self) -> String {
        self.render_with_html(self.new_content, &self.new_hunks, "inserted")
    }

    /// Internal helper to render content with HTML.
    fn render_with_html(&self, content: &[u8], hunks: &[ChangeHunk], class: &str) -> String {
        let mut result = String::new();

        for hunk in hunks {
            let text = String::from_utf8_lossy(hunk.extract(content));
            // Escape HTML entities
            let escaped = html_escape(&text);

            if hunk.is_change() {
                result.push_str(&format!(
                    "<span class=\"diff-{}\">{}</span>",
                    class, escaped
                ));
            } else {
                result.push_str(&escaped);
            }
        }

        result
    }
}

/// Escape HTML special characters.
fn html_escape(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\'' => result.push_str("&#39;"),
            _ => result.push(c),
        }
    }
    result
}

/// Compute an inline diff between two lines.
///
/// This is the main entry point for inline diff computation. It tokenizes
/// both lines, computes a word-level diff, and converts the results to
/// byte-range hunks suitable for highlighting.
///
/// # Arguments
///
/// * `old_line` - The original line content
/// * `new_line` - The modified line content
///
/// # Returns
///
/// An `InlineDiff` containing highlighted hunks for both lines.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::inline::compute_inline_diff;
///
/// let diff = compute_inline_diff(
///     b"const result = sum(a, b);",
///     b"const result = sum(a, b, c);",
/// );
///
/// assert!(diff.has_changes());
/// assert!(diff.inserted_bytes() > 0);
/// ```
pub fn compute_inline_diff<'a>(old_line: &'a [u8], new_line: &'a [u8]) -> InlineDiff<'a> {
    compute_inline_diff_with_config(old_line, new_line, &WordDiffConfig::default())
}

/// Compute an inline diff with custom configuration.
///
/// # Arguments
///
/// * `old_line` - The original line content
/// * `new_line` - The modified line content
/// * `config` - Word diff configuration
///
/// # Returns
///
/// An `InlineDiff` containing highlighted hunks for both lines.
pub fn compute_inline_diff_with_config<'a>(
    old_line: &'a [u8],
    new_line: &'a [u8],
    config: &WordDiffConfig,
) -> InlineDiff<'a> {
    // Handle empty cases
    if old_line.is_empty() && new_line.is_empty() {
        return InlineDiff::unchanged(old_line, new_line);
    }

    if old_line.is_empty() {
        return InlineDiff::all_inserted(old_line, new_line);
    }

    if new_line.is_empty() {
        return InlineDiff::all_deleted(old_line, new_line);
    }

    // Quick check for identical content
    if old_line == new_line {
        return InlineDiff::unchanged(old_line, new_line);
    }

    // Compute word-level diff
    let word_result = super::word::word_diff_with_config(old_line, new_line, config);

    // Convert word diff to inline hunks
    convert_word_diff_to_hunks(old_line, new_line, &word_result)
}

/// Convert a word diff result to inline hunks.
fn convert_word_diff_to_hunks<'a>(
    old_line: &'a [u8],
    new_line: &'a [u8],
    word_result: &WordDiffResult<'_>,
) -> InlineDiff<'a> {
    let mut old_hunks: Vec<ChangeHunk> = Vec::new();
    let mut new_hunks: Vec<ChangeHunk> = Vec::new();
    let mut has_changes = false;

    let old_tokens = word_result.old_tokens();
    let new_tokens = word_result.new_tokens();

    for op in word_result.ops() {
        match op {
            WordDiffOp::Equal {
                old_range,
                new_range,
            } => {
                // Add unchanged hunks
                if let (Some(first_old), Some(last_old)) = (
                    old_tokens.get(old_range.start),
                    old_tokens.get(old_range.end.saturating_sub(1)),
                ) {
                    let start = first_old.offset();
                    let end = last_old.end_offset();
                    old_hunks.push(ChangeHunk::unchanged(start, end));
                }

                if let (Some(first_new), Some(last_new)) = (
                    new_tokens.get(new_range.start),
                    new_tokens.get(new_range.end.saturating_sub(1)),
                ) {
                    let start = first_new.offset();
                    let end = last_new.end_offset();
                    new_hunks.push(ChangeHunk::unchanged(start, end));
                }
            }

            WordDiffOp::Insert { new_range, .. } => {
                has_changes = true;

                if let (Some(first), Some(last)) = (
                    new_tokens.get(new_range.start),
                    new_tokens.get(new_range.end.saturating_sub(1)),
                ) {
                    let start = first.offset();
                    let end = last.end_offset();
                    new_hunks.push(ChangeHunk::inserted(start, end));
                }
            }

            WordDiffOp::Delete { old_range, .. } => {
                has_changes = true;

                if let (Some(first), Some(last)) = (
                    old_tokens.get(old_range.start),
                    old_tokens.get(old_range.end.saturating_sub(1)),
                ) {
                    let start = first.offset();
                    let end = last.end_offset();
                    old_hunks.push(ChangeHunk::deleted(start, end));
                }
            }

            WordDiffOp::Replace {
                old_range,
                new_range,
            } => {
                has_changes = true;

                if let (Some(first), Some(last)) = (
                    old_tokens.get(old_range.start),
                    old_tokens.get(old_range.end.saturating_sub(1)),
                ) {
                    let start = first.offset();
                    let end = last.end_offset();
                    old_hunks.push(ChangeHunk::deleted(start, end));
                }

                if let (Some(first), Some(last)) = (
                    new_tokens.get(new_range.start),
                    new_tokens.get(new_range.end.saturating_sub(1)),
                ) {
                    let start = first.offset();
                    let end = last.end_offset();
                    new_hunks.push(ChangeHunk::inserted(start, end));
                }
            }
        }
    }

    // Merge adjacent hunks of the same kind
    old_hunks = merge_hunks(old_hunks);
    new_hunks = merge_hunks(new_hunks);

    // Fill any gaps with unchanged hunks
    old_hunks = fill_gaps(old_hunks, old_line.len());
    new_hunks = fill_gaps(new_hunks, new_line.len());

    InlineDiff {
        old_content: old_line,
        new_content: new_line,
        old_hunks,
        new_hunks,
        has_changes,
    }
}

/// Merge adjacent hunks of the same kind.
fn merge_hunks(hunks: Vec<ChangeHunk>) -> Vec<ChangeHunk> {
    if hunks.is_empty() {
        return hunks;
    }

    let mut merged: Vec<ChangeHunk> = Vec::with_capacity(hunks.len());

    for hunk in hunks {
        if let Some(last) = merged.last_mut() {
            if let Some(combined) = last.try_merge(&hunk) {
                *last = combined;
                continue;
            }
        }
        merged.push(hunk);
    }

    merged
}

/// Fill gaps between hunks with unchanged hunks.
fn fill_gaps(mut hunks: Vec<ChangeHunk>, total_len: usize) -> Vec<ChangeHunk> {
    if hunks.is_empty() {
        if total_len > 0 {
            return vec![ChangeHunk::unchanged(0, total_len)];
        }
        return hunks;
    }

    // Sort by start position
    hunks.sort_by_key(|s| s.start);

    let mut filled: Vec<ChangeHunk> = Vec::with_capacity(hunks.len() * 2);
    let mut current_pos = 0;

    for hunk in hunks {
        // Fill gap before this hunk
        if hunk.start > current_pos {
            filled.push(ChangeHunk::unchanged(current_pos, hunk.start));
        }

        filled.push(hunk.clone());
        current_pos = hunk.end;
    }

    // Fill gap at the end
    if current_pos < total_len {
        filled.push(ChangeHunk::unchanged(current_pos, total_len));
    }

    filled
}

/// Configuration for inline diff display.
///
/// Controls how the inline diff is rendered.
#[derive(Debug, Clone)]
pub struct InlineDiffConfig {
    /// Minimum number of equal tokens required to break up changes.
    ///
    /// If changes are separated by fewer than this many equal tokens,
    /// they may be merged into a single change hunk.
    pub min_equal_tokens: usize,

    /// Whether to show whitespace changes.
    pub show_whitespace_changes: bool,

    /// Whether to use word-level or character-level diff.
    pub word_level: bool,
}

impl Default for InlineDiffConfig {
    fn default() -> Self {
        Self {
            min_equal_tokens: 1,
            show_whitespace_changes: true,
            word_level: true,
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // HunkKind tests

    #[test]
    fn test_hunk_kind_is_change() {
        assert!(HunkKind::Deleted.is_change());
        assert!(HunkKind::Inserted.is_change());
        assert!(HunkKind::Modified.is_change());
        assert!(!HunkKind::Unchanged.is_change());
    }

    #[test]
    fn test_hunk_kind_is_deleted() {
        assert!(HunkKind::Deleted.is_deleted());
        assert!(HunkKind::Modified.is_deleted());
        assert!(!HunkKind::Inserted.is_deleted());
        assert!(!HunkKind::Unchanged.is_deleted());
    }

    #[test]
    fn test_hunk_kind_is_inserted() {
        assert!(HunkKind::Inserted.is_inserted());
        assert!(HunkKind::Modified.is_inserted());
        assert!(!HunkKind::Deleted.is_inserted());
        assert!(!HunkKind::Unchanged.is_inserted());
    }

    #[test]
    fn test_hunk_kind_name() {
        assert_eq!(HunkKind::Deleted.name(), "deleted");
        assert_eq!(HunkKind::Inserted.name(), "inserted");
        assert_eq!(HunkKind::Modified.name(), "modified");
        assert_eq!(HunkKind::Unchanged.name(), "unchanged");
    }

    #[test]
    fn test_hunk_kind_display() {
        assert_eq!(format!("{}", HunkKind::Deleted), "deleted");
    }

    #[test]
    fn test_hunk_kind_ansi_color() {
        assert!(!HunkKind::Deleted.ansi_color().is_empty());
        assert!(!HunkKind::Inserted.ansi_color().is_empty());
        assert!(HunkKind::Unchanged.ansi_color().is_empty());
    }

    // ChangeHunk tests

    #[test]
    fn test_change_hunk_new() {
        let hunk = ChangeHunk::new(10, 20, HunkKind::Inserted);
        assert_eq!(hunk.start, 10);
        assert_eq!(hunk.end, 20);
        assert_eq!(hunk.kind, HunkKind::Inserted);
        assert_eq!(hunk.len(), 10);
    }

    #[test]
    fn test_change_hunk_constructors() {
        let deleted = ChangeHunk::deleted(0, 5);
        assert_eq!(deleted.kind, HunkKind::Deleted);

        let inserted = ChangeHunk::inserted(5, 10);
        assert_eq!(inserted.kind, HunkKind::Inserted);

        let modified = ChangeHunk::modified(10, 15);
        assert_eq!(modified.kind, HunkKind::Modified);

        let unchanged = ChangeHunk::unchanged(15, 20);
        assert_eq!(unchanged.kind, HunkKind::Unchanged);
    }

    #[test]
    fn test_change_hunk_range() {
        let hunk = ChangeHunk::new(5, 15, HunkKind::Deleted);
        assert_eq!(hunk.range(), 5..15);
    }

    #[test]
    fn test_change_hunk_is_empty() {
        let empty = ChangeHunk::new(5, 5, HunkKind::Deleted);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);

        let non_empty = ChangeHunk::new(5, 10, HunkKind::Deleted);
        assert!(!non_empty.is_empty());
    }

    #[test]
    fn test_change_hunk_is_change() {
        assert!(ChangeHunk::deleted(0, 5).is_change());
        assert!(ChangeHunk::inserted(0, 5).is_change());
        assert!(!ChangeHunk::unchanged(0, 5).is_change());
    }

    #[test]
    fn test_change_hunk_is_adjacent_to() {
        let hunk1 = ChangeHunk::new(0, 5, HunkKind::Deleted);
        let hunk2 = ChangeHunk::new(5, 10, HunkKind::Deleted);
        let hunk3 = ChangeHunk::new(15, 20, HunkKind::Deleted);

        assert!(hunk1.is_adjacent_to(&hunk2));
        assert!(hunk2.is_adjacent_to(&hunk1));
        assert!(!hunk1.is_adjacent_to(&hunk3));
    }

    #[test]
    fn test_change_hunk_try_merge() {
        let hunk1 = ChangeHunk::new(0, 5, HunkKind::Deleted);
        let hunk2 = ChangeHunk::new(5, 10, HunkKind::Deleted);
        let hunk3 = ChangeHunk::new(5, 10, HunkKind::Inserted);

        let merged = hunk1.try_merge(&hunk2);
        assert!(merged.is_some());
        let merged = merged.unwrap();
        assert_eq!(merged.start, 0);
        assert_eq!(merged.end, 10);

        // Different kinds shouldn't merge
        let no_merge = hunk1.try_merge(&hunk3);
        assert!(no_merge.is_none());
    }

    #[test]
    fn test_change_hunk_extract() {
        let content = b"hello world";
        let hunk = ChangeHunk::new(0, 5, HunkKind::Deleted);
        assert_eq!(hunk.extract(content), b"hello");

        let hunk2 = ChangeHunk::new(6, 11, HunkKind::Inserted);
        assert_eq!(hunk2.extract(content), b"world");
    }

    #[test]
    fn test_change_hunk_try_extract() {
        let content = b"hello";
        let valid = ChangeHunk::new(0, 3, HunkKind::Deleted);
        assert_eq!(valid.try_extract(content), Some(&b"hel"[..]));

        let invalid = ChangeHunk::new(0, 100, HunkKind::Deleted);
        assert_eq!(invalid.try_extract(content), None);
    }

    #[test]
    fn test_change_hunk_display() {
        let hunk = ChangeHunk::new(5, 10, HunkKind::Deleted);
        let display = format!("{}", hunk);
        assert!(display.contains("deleted"));
        assert!(display.contains("5"));
        assert!(display.contains("10"));
    }

    // InlineDiff tests

    #[test]
    fn test_inline_diff_empty() {
        let diff = compute_inline_diff(b"", b"");
        assert!(!diff.has_changes());
        assert!(diff.is_unchanged());
    }

    #[test]
    fn test_inline_diff_identical() {
        let diff = compute_inline_diff(b"hello world", b"hello world");
        assert!(!diff.has_changes());
        assert!(diff.is_unchanged());
        assert_eq!(diff.deleted_bytes(), 0);
        assert_eq!(diff.inserted_bytes(), 0);
    }

    #[test]
    fn test_inline_diff_all_deleted() {
        let diff = compute_inline_diff(b"hello world", b"");
        assert!(diff.has_changes());
        assert!(diff.deleted_bytes() > 0);
        assert_eq!(diff.inserted_bytes(), 0);
    }

    #[test]
    fn test_inline_diff_all_inserted() {
        let diff = compute_inline_diff(b"", b"hello world");
        assert!(diff.has_changes());
        assert_eq!(diff.deleted_bytes(), 0);
        assert!(diff.inserted_bytes() > 0);
    }

    #[test]
    fn test_inline_diff_single_word_change() {
        let diff = compute_inline_diff(b"hello world", b"hello there");
        assert!(diff.has_changes());
        assert!(diff.old_change_count() > 0);
        assert!(diff.new_change_count() > 0);
    }

    #[test]
    fn test_inline_diff_number_change() {
        let diff = compute_inline_diff(b"const x = 1;", b"const x = 2;");
        assert!(diff.has_changes());

        // The number should be highlighted
        let old_changes: Vec<_> = diff.old_changes().collect();
        let new_changes: Vec<_> = diff.new_changes().collect();

        assert!(!old_changes.is_empty());
        assert!(!new_changes.is_empty());
    }

    #[test]
    fn test_inline_diff_insertion_in_middle() {
        let diff = compute_inline_diff(b"sum(a, b)", b"sum(a, b, c)");
        assert!(diff.has_changes());
        assert!(diff.inserted_bytes() > 0);
    }

    #[test]
    fn test_inline_diff_content_accessors() {
        let old = b"hello";
        let new = b"world";
        let diff = compute_inline_diff(old, new);

        assert_eq!(diff.old_content(), old);
        assert_eq!(diff.new_content(), new);
    }

    #[test]
    fn test_inline_diff_hunks_cover_entire_line() {
        let diff = compute_inline_diff(b"hello world", b"hello there");

        // Old hunks should cover all of "hello world" (11 bytes)
        let old_coverage: usize = diff.old_hunks().iter().map(|s| s.len()).sum();
        assert_eq!(old_coverage, 11);

        // New hunks should cover all of "hello there" (11 bytes)
        let new_coverage: usize = diff.new_hunks().iter().map(|s| s.len()).sum();
        assert_eq!(new_coverage, 11);
    }

    #[test]
    fn test_inline_diff_render_ansi() {
        let diff = compute_inline_diff(b"old text", b"new text");

        let old_rendered = diff.render_old_ansi();
        let new_rendered = diff.render_new_ansi();

        // Should contain original text
        assert!(old_rendered.contains("old") || old_rendered.contains("text"));
        assert!(new_rendered.contains("new") || new_rendered.contains("text"));
    }

    #[test]
    fn test_inline_diff_render_html() {
        let diff = compute_inline_diff(b"old", b"new");

        let old_html = diff.render_old_html();
        let new_html = diff.render_new_html();

        // Should contain span tags for changes
        if diff.has_changes() {
            assert!(old_html.contains("<span") || !old_html.contains("old"));
            assert!(new_html.contains("<span") || !new_html.contains("new"));
        }
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("hello"), "hello");
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a & b"), "a &amp; b");
        assert_eq!(html_escape("\"quoted\""), "&quot;quoted&quot;");
    }

    // Helper function tests

    #[test]
    fn test_merge_hunks() {
        let hunks = vec![
            ChangeHunk::deleted(0, 5),
            ChangeHunk::deleted(5, 10),
            ChangeHunk::unchanged(10, 15),
        ];

        let merged = merge_hunks(hunks);

        // First two should be merged
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].start, 0);
        assert_eq!(merged[0].end, 10);
    }

    #[test]
    fn test_fill_gaps() {
        let hunks = vec![ChangeHunk::deleted(5, 10)];

        let filled = fill_gaps(hunks, 20);

        // Should have: unchanged(0-5), deleted(5-10), unchanged(10-20)
        assert_eq!(filled.len(), 3);
        assert_eq!(filled[0].kind, HunkKind::Unchanged);
        assert_eq!(filled[0].range(), 0..5);
        assert_eq!(filled[1].kind, HunkKind::Deleted);
        assert_eq!(filled[1].range(), 5..10);
        assert_eq!(filled[2].kind, HunkKind::Unchanged);
        assert_eq!(filled[2].range(), 10..20);
    }

    #[test]
    fn test_fill_gaps_empty() {
        let filled = fill_gaps(vec![], 10);
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].kind, HunkKind::Unchanged);
        assert_eq!(filled[0].range(), 0..10);
    }

    // Real-world code tests

    #[test]
    fn test_inline_diff_function_argument_added() {
        let old = b"fn calculate(a: i32, b: i32) -> i32";
        let new = b"fn calculate(a: i32, b: i32, c: i32) -> i32";

        let diff = compute_inline_diff(old, new);
        assert!(diff.has_changes());
        assert!(diff.inserted_bytes() > 0);
    }

    #[test]
    fn test_inline_diff_operator_change() {
        let old = b"if x == y {";
        let new = b"if x != y {";

        let diff = compute_inline_diff(old, new);
        assert!(diff.has_changes());
    }

    #[test]
    fn test_inline_diff_string_literal_change() {
        let old = b"let msg = \"hello\";";
        let new = b"let msg = \"goodbye\";";

        let diff = compute_inline_diff(old, new);
        assert!(diff.has_changes());
    }

    #[test]
    fn test_inline_diff_type_change() {
        let old = b"let x: u32 = 1;";
        let new = b"let x: u64 = 1;";

        let diff = compute_inline_diff(old, new);
        assert!(diff.has_changes());
    }

    #[test]
    fn test_inline_diff_variable_rename() {
        let old = b"let foo = bar;";
        let new = b"let baz = bar;";

        let diff = compute_inline_diff(old, new);
        assert!(diff.has_changes());

        // "foo" should be deleted, "baz" inserted
        let deleted: Vec<_> = diff.old_changes().collect();
        let inserted: Vec<_> = diff.new_changes().collect();

        assert!(!deleted.is_empty());
        assert!(!inserted.is_empty());
    }

    // InlineDiffConfig tests

    #[test]
    fn test_inline_diff_config_default() {
        let config = InlineDiffConfig::default();
        assert_eq!(config.min_equal_tokens, 1);
        assert!(config.show_whitespace_changes);
        assert!(config.word_level);
    }
}

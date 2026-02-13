//! Line representation for diff algorithms.
//!
//! This module provides the [`Line`] struct, which represents a single line
//! (or chunk) of content for comparison during diffing. The design optimizes
//! for the common operations in diff algorithms:
//!
//! - **Equality checking**: Lines are compared frequently; we use hash-based
//!   comparison for O(1) average case
//! - **Memory efficiency**: Lines hold references to the original content,
//!   avoiding copies
//! - **Hashing**: Pre-computed hashes enable fast lookup in hash tables
//!   (used by Patience diff)
//!
//! # Design Decisions
//!
//! ## Zero-Copy References
//!
//! Lines hold `&[u8]` references to the original content rather than owning
//! copies. This means:
//! - No allocation overhead per line
//! - Original content must outlive the Line
//! - Content is immutable (which is fine for diffing)
//!
//! ## Hash-Based Equality
//!
//! We compute a hash for each line and use it as a fast pre-filter for equality:
//! - If hashes differ, lines are definitely different
//! - If hashes match, we still compare bytes to handle collisions
//!
//! This is particularly effective because:
//! - Most line comparisons result in "not equal"
//! - Hash comparison is O(1) vs O(n) for byte comparison
//! - The hash is computed once at construction
//!
//! ## Trailing Newline Handling
//!
//! Files may or may not end with a newline, and this affects how we compare
//! the last line. We track whether a line has a trailing newline to ensure
//! correct comparisons:
//!
//! ```text
//! "hello\n" vs "hello" - different if we care about trailing newlines
//! "hello\n" vs "hello" - same if we're comparing content only
//! ```
//!
//! The `Line` struct includes a `trailing_newline` flag and provides both
//! strict and lenient comparison modes.

use std::hash::{Hash, Hasher};

/// A line (or chunk) of content for comparison in diff algorithms.
///
/// `Line` is designed for efficient use in diff operations:
/// - Zero-copy: holds a reference to the original bytes
/// - Fast equality: uses pre-computed hash for quick rejection
/// - Configurable: can include or exclude trailing newline in comparisons
///
/// # Lifetime
///
/// The `'a` lifetime parameter ties the `Line` to the original content.
/// The line is only valid as long as the source content exists.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::Line;
///
/// let content = b"hello\nworld\n";
/// let lines: Vec<Line> = Line::from_bytes(content);
///
/// assert_eq!(lines.len(), 2);
/// assert_eq!(lines[0].content(), b"hello\n");
/// assert_eq!(lines[1].content(), b"world\n");
/// ```
#[derive(Clone)]
pub struct Line<'a> {
    /// The raw bytes of this line (may include trailing newline).
    content: &'a [u8],

    /// Pre-computed hash of the content for fast comparison.
    hash: u64,

    /// Whether this line ends with a newline character.
    ///
    /// This is important for:
    /// - Correctly handling the last line of a file
    /// - Matching lines that differ only in trailing newline
    trailing_newline: bool,

    /// Whether this is the last line in the sequence.
    ///
    /// Used for special handling of final line comparisons
    /// (e.g., matching "hello" with "hello\n" at end of file).
    is_last: bool,
}

impl<'a> Line<'a> {
    /// Create a new line from a byte slice.
    ///
    /// The line's hash is computed immediately and cached for later use.
    ///
    /// # Arguments
    ///
    /// * `content` - The raw bytes of the line (may include newline)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::Line;
    ///
    /// let line = Line::new(b"hello world\n");
    /// assert!(line.has_trailing_newline());
    /// ```
    pub fn new(content: &'a [u8]) -> Self {
        let trailing_newline = content.last() == Some(&b'\n');
        let hash = Self::compute_hash(content);

        Self {
            content,
            hash,
            trailing_newline,
            is_last: false,
        }
    }

    /// Create a line marked as the last in a sequence.
    ///
    /// This affects how trailing newline comparisons work.
    pub fn new_last(content: &'a [u8]) -> Self {
        let mut line = Self::new(content);
        line.is_last = true;
        line
    }

    /// Split a byte slice into lines.
    ///
    /// Lines are split on `\n` characters. The newline is included in each
    /// line (except possibly the last line if the content doesn't end with
    /// a newline).
    ///
    /// # Arguments
    ///
    /// * `content` - The raw bytes to split into lines
    ///
    /// # Returns
    ///
    /// A vector of `Line`s representing each line in the content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::Line;
    ///
    /// let content = b"line1\nline2\nline3";
    /// let lines = Line::from_bytes(content);
    ///
    /// assert_eq!(lines.len(), 3);
    /// assert_eq!(lines[0].content(), b"line1\n");
    /// assert_eq!(lines[1].content(), b"line2\n");
    /// assert_eq!(lines[2].content(), b"line3"); // No trailing newline
    /// ```
    pub fn from_bytes(content: &'a [u8]) -> Vec<Self> {
        if content.is_empty() {
            return Vec::new();
        }

        let mut lines = Vec::new();
        let mut start = 0;

        for (i, &byte) in content.iter().enumerate() {
            if byte == b'\n' {
                lines.push(Self::new(&content[start..=i]));
                start = i + 1;
            }
        }

        // Handle final line without trailing newline
        if start < content.len() {
            lines.push(Self::new_last(&content[start..]));
        } else if !lines.is_empty() {
            // Mark the last line as last
            if let Some(last) = lines.last_mut() {
                last.is_last = true;
            }
        }

        lines
    }

    /// Split a string into lines.
    ///
    /// Convenience wrapper around [`from_bytes`](Self::from_bytes) for string input.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::Line;
    ///
    /// let lines = Line::from_text("hello\nworld\n");
    /// assert_eq!(lines.len(), 2);
    /// ```
    pub fn from_text(text: &'a str) -> Vec<Self> {
        Self::from_bytes(text.as_bytes())
    }

    /// Get the raw content of this line.
    ///
    /// The content includes the trailing newline if present.
    #[inline]
    pub fn content(&self) -> &'a [u8] {
        self.content
    }

    /// Get the content without the trailing newline.
    ///
    /// If the line doesn't have a trailing newline, returns the full content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::Line;
    ///
    /// let line = Line::new(b"hello\n");
    /// assert_eq!(line.content_without_newline(), b"hello");
    /// ```
    #[inline]
    pub fn content_without_newline(&self) -> &'a [u8] {
        if self.trailing_newline {
            &self.content[..self.content.len() - 1]
        } else {
            self.content
        }
    }

    /// Check if this line has a trailing newline.
    #[inline]
    pub fn has_trailing_newline(&self) -> bool {
        self.trailing_newline
    }

    /// Check if this is the last line in the sequence.
    #[inline]
    pub fn is_last(&self) -> bool {
        self.is_last
    }

    /// Get the length of the line in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.content.len()
    }

    /// Check if the line is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    /// Get the pre-computed hash of this line.
    ///
    /// This hash is used for fast equality pre-checking. Lines with
    /// different hashes are definitely not equal; lines with the same
    /// hash might be equal (collision) and need byte comparison.
    #[inline]
    pub fn hash_value(&self) -> u64 {
        self.hash
    }

    /// Compute the hash of content using FNV-1a algorithm.
    ///
    /// FNV-1a is chosen for:
    /// - Speed: Very fast for small inputs (typical line lengths)
    /// - Quality: Good distribution for text data
    /// - Simplicity: Easy to implement and understand
    #[inline]
    fn compute_hash(content: &[u8]) -> u64 {
        // FNV-1a parameters for 64-bit
        const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;

        let mut hash = FNV_OFFSET_BASIS;
        for &byte in content {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    /// Compare two lines for equality, optionally ignoring trailing newline differences.
    ///
    /// When `strict` is true, lines must match exactly including trailing newlines.
    /// When `strict` is false, a line ending in newline can match one without
    /// (useful for comparing last lines of files).
    ///
    /// # Arguments
    ///
    /// * `other` - The line to compare with
    /// * `strict` - Whether to require exact match including newlines
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::Line;
    ///
    /// let line1 = Line::new(b"hello\n");
    /// let line2 = Line::new(b"hello");
    ///
    /// assert!(!line1.equals(&line2, true));  // Strict: different
    /// assert!(line1.equals(&line2, false));  // Lenient: same content
    /// ```
    pub fn equals(&self, other: &Self, strict: bool) -> bool {
        if strict {
            self.content == other.content
        } else {
            self.content_without_newline() == other.content_without_newline()
        }
    }
}

impl<'a> PartialEq for Line<'a> {
    /// Compare two lines for equality.
    ///
    /// Uses hash-based pre-filtering for performance:
    /// 1. If hashes differ, lines are definitely different (fast path)
    /// 2. If hashes match, compare bytes to handle collisions
    ///
    /// This comparison is **strict** - lines must match exactly including
    /// any trailing newline. For lenient comparison, use [`equals`](Self::equals).
    fn eq(&self, other: &Self) -> bool {
        // Fast path: different hashes means definitely not equal
        if self.hash != other.hash {
            return false;
        }

        // Hashes match, compare content (handles hash collisions)
        self.content == other.content
    }
}

impl<'a> Eq for Line<'a> {}

impl<'a> Hash for Line<'a> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Use our pre-computed hash
        state.write_u64(self.hash);
    }
}

impl<'a> std::fmt::Debug for Line<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Try to display as UTF-8 string, fall back to byte representation
        match std::str::from_utf8(self.content) {
            Ok(s) => write!(f, "Line({:?})", s),
            Err(_) => write!(f, "Line({:?})", self.content),
        }
    }
}

impl<'a> AsRef<[u8]> for Line<'a> {
    fn as_ref(&self) -> &[u8] {
        self.content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_line() {
        let line = Line::new(b"hello\n");
        assert_eq!(line.content(), b"hello\n");
        assert!(line.has_trailing_newline());
        assert!(!line.is_last());
    }

    #[test]
    fn test_new_line_without_newline() {
        let line = Line::new(b"hello");
        assert_eq!(line.content(), b"hello");
        assert!(!line.has_trailing_newline());
    }

    #[test]
    fn test_new_last() {
        let line = Line::new_last(b"hello");
        assert!(line.is_last());
    }

    #[test]
    fn test_from_bytes_simple() {
        let lines = Line::from_bytes(b"a\nb\nc\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content(), b"a\n");
        assert_eq!(lines[1].content(), b"b\n");
        assert_eq!(lines[2].content(), b"c\n");
    }

    #[test]
    fn test_from_bytes_no_trailing_newline() {
        let lines = Line::from_bytes(b"a\nb\nc");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content(), b"a\n");
        assert_eq!(lines[1].content(), b"b\n");
        assert_eq!(lines[2].content(), b"c");
        assert!(!lines[2].has_trailing_newline());
        assert!(lines[2].is_last());
    }

    #[test]
    fn test_from_bytes_empty() {
        let lines = Line::from_bytes(b"");
        assert!(lines.is_empty());
    }

    #[test]
    fn test_from_bytes_single_line() {
        let lines = Line::from_bytes(b"hello\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].content(), b"hello\n");
        assert!(lines[0].is_last());
    }

    #[test]
    fn test_from_bytes_only_newlines() {
        let lines = Line::from_bytes(b"\n\n\n");
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(line.content(), b"\n");
        }
    }

    #[test]
    fn test_from_text() {
        let lines = Line::from_text("hello\nworld\n");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_content_without_newline() {
        let with_newline = Line::new(b"hello\n");
        let without_newline = Line::new(b"hello");

        assert_eq!(with_newline.content_without_newline(), b"hello");
        assert_eq!(without_newline.content_without_newline(), b"hello");
    }

    #[test]
    fn test_equality_same() {
        let line1 = Line::new(b"hello\n");
        let line2 = Line::new(b"hello\n");
        assert_eq!(line1, line2);
    }

    #[test]
    fn test_equality_different() {
        let line1 = Line::new(b"hello\n");
        let line2 = Line::new(b"world\n");
        assert_ne!(line1, line2);
    }

    #[test]
    fn test_equality_newline_matters() {
        let line1 = Line::new(b"hello\n");
        let line2 = Line::new(b"hello");
        // Strict equality: they're different
        assert_ne!(line1, line2);
    }

    #[test]
    fn test_equals_lenient() {
        let line1 = Line::new(b"hello\n");
        let line2 = Line::new(b"hello");
        // Lenient comparison: same content
        assert!(line1.equals(&line2, false));
        // Strict comparison: different
        assert!(!line1.equals(&line2, true));
    }

    #[test]
    fn test_hash_consistency() {
        let line1 = Line::new(b"test\n");
        let line2 = Line::new(b"test\n");
        assert_eq!(line1.hash_value(), line2.hash_value());
    }

    #[test]
    fn test_hash_different() {
        let line1 = Line::new(b"test1\n");
        let line2 = Line::new(b"test2\n");
        // Hashes should (almost certainly) be different
        assert_ne!(line1.hash_value(), line2.hash_value());
    }

    #[test]
    fn test_len() {
        let line = Line::new(b"hello\n");
        assert_eq!(line.len(), 6);
        assert!(!line.is_empty());
    }

    #[test]
    fn test_empty_line() {
        let line = Line::new(b"");
        assert!(line.is_empty());
        assert_eq!(line.len(), 0);
    }

    #[test]
    fn test_debug() {
        let line = Line::new(b"hello\n");
        let debug = format!("{:?}", line);
        assert!(debug.contains("hello"));
    }

    #[test]
    fn test_debug_binary() {
        let line = Line::new(&[0xff, 0xfe, 0x00]);
        let debug = format!("{:?}", line);
        // Should not panic, should show bytes
        assert!(debug.contains("Line"));
    }

    #[test]
    fn test_as_ref() {
        let line = Line::new(b"hello");
        let bytes: &[u8] = line.as_ref();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn test_clone() {
        let line1 = Line::new(b"hello\n");
        let line2 = line1.clone();
        assert_eq!(line1, line2);
        assert_eq!(line1.hash_value(), line2.hash_value());
    }

    #[test]
    fn test_hash_in_hashset() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(Line::new(b"hello\n"));
        set.insert(Line::new(b"world\n"));

        assert!(set.contains(&Line::new(b"hello\n")));
        assert!(set.contains(&Line::new(b"world\n")));
        assert!(!set.contains(&Line::new(b"foo\n")));
    }

    #[test]
    fn test_last_line_tracking() {
        let lines = Line::from_bytes(b"a\nb\nc\n");
        assert!(!lines[0].is_last());
        assert!(!lines[1].is_last());
        assert!(lines[2].is_last());
    }
}

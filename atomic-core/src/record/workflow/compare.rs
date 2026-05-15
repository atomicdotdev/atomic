//! Content comparison and diff operations.
//!
//! This module provides functions for comparing file content between the
//! working copy and pristine state. It handles encoding detection, binary
//! file identification, and diff generation.
//!
//! # Overview
//!
//! Content comparison is the core of change detection. For each file that
//! exists in both the working copy and pristine, we need to:
//!
//! 1. Retrieve content from both sources
//! 2. Check if either is binary (skip diffing if so)
//! 3. Detect encoding changes
//! 4. Generate diff operations for text files
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                       Content Comparison Flow                            │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  ┌─────────────────┐          ┌─────────────────┐                      │
//! │  │ Pristine Content│          │ Working Content │                      │
//! │  │    (graph)      │          │   (disk)        │                      │
//! │  └────────┬────────┘          └────────┬────────┘                      │
//! │           │                            │                                │
//! │           └────────────┬───────────────┘                                │
//! │                        │                                                │
//! │                        ▼                                                │
//! │              ┌─────────────────────┐                                    │
//! │              │  Encoding Detection │                                    │
//! │              │  (UTF-8 vs Binary)  │                                    │
//! │              └──────────┬──────────┘                                    │
//! │                         │                                               │
//! │           ┌─────────────┴─────────────┐                                │
//! │           │                           │                                │
//! │           ▼                           ▼                                │
//! │  ┌─────────────────┐        ┌─────────────────┐                        │
//! │  │  Binary File    │        │   Text File     │                        │
//! │  │  (no diff)      │        │  (run diff)     │                        │
//! │  └─────────────────┘        └────────┬────────┘                        │
//! │                                      │                                  │
//! │                                      ▼                                  │
//! │                            ┌─────────────────┐                          │
//! │                            │   DiffResult    │                          │
//! │                            │   (operations)  │                          │
//! │                            └─────────────────┘                          │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_core::record::workflow::compare::{
//!     compare_content, detect_encoding, is_binary, CompareResult,
//! };
//! use atomic_core::diff::Algorithm;
//!
//! let old_content = b"line1\nline2\nline3\n";
//! let new_content = b"line1\nmodified\nline3\n";
//!
//! let result = compare_content(old_content, new_content, Algorithm::Myers);
//!
//! if result.has_changes() {
//!     println!("Found {} diff operations", result.diff_ops.len());
//! }
//! ```

use crate::change::Encoding;
use crate::diff::{diff_raw, Algorithm, DiffOp, Line};

// ============================================================================
// COMPARE RESULT
// ============================================================================

/// Result of comparing two content blobs.
///
/// This structure captures everything we learn from comparing file content:
/// encodings, whether content changed, and the diff operations if applicable.
///
/// # Fields
///
/// - `old_encoding`: Detected encoding of pristine content
/// - `new_encoding`: Detected encoding of working copy content
/// - `content_changed`: Whether the raw bytes differ
/// - `diff_ops`: Diff operations (empty for binary or identical files)
/// - `is_binary`: Whether either file is binary
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::compare::CompareResult;
/// use atomic_core::change::Encoding;
///
/// let result = CompareResult::identical(Encoding::Utf8);
/// assert!(!result.has_changes());
/// assert!(result.diff_ops.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareResult {
    /// Encoding of the old (pristine) content.
    pub old_encoding: Encoding,

    /// Encoding of the new (working copy) content.
    pub new_encoding: Encoding,

    /// Whether the raw content bytes differ.
    pub content_changed: bool,

    /// Diff operations describing the changes.
    ///
    /// Empty if:
    /// - Content is identical
    /// - Either file is binary
    /// - Diff was skipped for other reasons
    pub diff_ops: Vec<DiffOp>,

    /// Whether either file is binary.
    pub is_binary: bool,
}

impl CompareResult {
    /// Create a result for identical content.
    ///
    /// # Arguments
    ///
    /// * `encoding` - The shared encoding of both files
    pub fn identical(encoding: Encoding) -> Self {
        Self {
            old_encoding: encoding,
            new_encoding: encoding,
            content_changed: false,
            diff_ops: Vec::new(),
            is_binary: encoding == Encoding::Binary,
        }
    }

    /// Create a result for binary files.
    ///
    /// Binary files are marked as changed but have no diff operations.
    ///
    /// # Arguments
    ///
    /// * `old_encoding` - Encoding of pristine content
    /// * `new_encoding` - Encoding of working content
    pub fn binary(old_encoding: Encoding, new_encoding: Encoding) -> Self {
        Self {
            old_encoding,
            new_encoding,
            content_changed: true,
            diff_ops: Vec::new(),
            is_binary: true,
        }
    }

    /// Create a result with diff operations.
    ///
    /// # Arguments
    ///
    /// * `old_encoding` - Encoding of pristine content
    /// * `new_encoding` - Encoding of working content
    /// * `diff_ops` - The diff operations
    pub fn with_diff(
        old_encoding: Encoding,
        new_encoding: Encoding,
        diff_ops: Vec<DiffOp>,
    ) -> Self {
        let content_changed = !diff_ops.is_empty();
        Self {
            old_encoding,
            new_encoding,
            content_changed,
            diff_ops,
            is_binary: false,
        }
    }

    /// Check if the content has any changes.
    ///
    /// Returns `true` if:
    /// - Raw bytes differ
    /// - There are diff operations
    /// - Encoding changed (even if content is the same)
    pub fn has_changes(&self) -> bool {
        self.content_changed || self.old_encoding != self.new_encoding
    }

    /// Check if only encoding changed (content bytes identical).
    pub fn encoding_only_change(&self) -> bool {
        !self.content_changed && self.old_encoding != self.new_encoding
    }

    /// Get the number of diff operations.
    pub fn diff_count(&self) -> usize {
        self.diff_ops.len()
    }
}

impl Default for CompareResult {
    fn default() -> Self {
        Self::identical(Encoding::Utf8)
    }
}

// ============================================================================
// ENCODING DETECTION
// ============================================================================

/// Detect the encoding of content.
///
/// Analyzes content to determine if it's valid UTF-8 text or binary data.
///
/// # Arguments
///
/// * `content` - The content to analyze
///
/// # Returns
///
/// `Encoding::Utf8` for valid text, `Encoding::Binary` otherwise.
///
/// # Algorithm
///
/// Content is considered binary if:
/// 1. It contains null bytes (0x00)
/// 2. It's not valid UTF-8
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::compare::detect_encoding;
/// use atomic_core::change::Encoding;
///
/// assert_eq!(detect_encoding(b"Hello, World!"), Encoding::Utf8);
/// assert_eq!(detect_encoding(&[0x00, 0x01, 0x02]), Encoding::Binary);
/// ```
pub fn detect_encoding(content: &[u8]) -> Encoding {
    // Check for null bytes (strong indicator of binary)
    if content.contains(&0) {
        return Encoding::Binary;
    }

    // Check for valid UTF-8
    if std::str::from_utf8(content).is_ok() {
        Encoding::Utf8
    } else {
        Encoding::Binary
    }
}

/// Check if content is binary.
///
/// Uses heuristics to determine if content is binary data rather than text.
///
/// # Arguments
///
/// * `content` - The content to check
///
/// # Returns
///
/// `true` if the content appears to be binary.
///
/// # Algorithm
///
/// Content is considered binary if:
/// 1. It contains null bytes
/// 2. More than 30% of bytes are non-printable (excluding whitespace)
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::compare::is_binary;
///
/// assert!(!is_binary(b"Hello\nWorld\n"));
/// assert!(is_binary(&[0x00, 0x01, 0x02]));
/// ```
pub fn is_binary(content: &[u8]) -> bool {
    // Quick check: null bytes indicate binary
    if content.contains(&0) {
        return true;
    }

    // Empty content is not binary
    if content.is_empty() {
        return false;
    }

    // Check ratio of non-printable characters
    let non_printable = content
        .iter()
        .filter(|&&b| {
            // Non-printable: < 0x20 except tab (0x09), newline (0x0A), carriage return (0x0D)
            b < 0x20 && b != b'\t' && b != b'\n' && b != b'\r'
        })
        .count();

    // If more than 30% non-printable, consider binary
    non_printable * 100 / content.len() > 30
}

// ============================================================================
// CONTENT COMPARISON
// ============================================================================

/// Compare two content blobs and produce a diff.
///
/// This is the main entry point for content comparison. It handles:
/// - Encoding detection for both contents
/// - Binary file detection (skips diffing)
/// - Diff generation for text files
///
/// # Arguments
///
/// * `old_content` - Content from pristine (empty for new files)
/// * `new_content` - Content from working copy
/// * `algorithm` - Diff algorithm to use
///
/// # Returns
///
/// A `CompareResult` with encodings, change status, and diff operations.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::compare::compare_content;
/// use atomic_core::diff::Algorithm;
///
/// let old = b"line1\nline2\n";
/// let new = b"line1\nmodified\nline2\n";
///
/// let result = compare_content(old, new, Algorithm::Myers);
/// assert!(result.has_changes());
/// assert!(!result.is_binary);
/// ```
pub fn compare_content(
    old_content: &[u8],
    new_content: &[u8],
    algorithm: Algorithm,
) -> CompareResult {
    // Detect encodings
    let old_encoding = detect_encoding(old_content);
    let new_encoding = detect_encoding(new_content);

    // Quick check: if bytes are identical, no changes
    if old_content == new_content {
        return CompareResult::identical(old_encoding);
    }

    // Check if either is binary
    if is_binary(old_content) || is_binary(new_content) {
        return CompareResult::binary(old_encoding, new_encoding);
    }

    // Generate diff for text content
    let diff_ops = generate_diff(old_content, new_content, algorithm);

    CompareResult::with_diff(old_encoding, new_encoding, diff_ops)
}

/// Compare content with size limit check.
///
/// Like `compare_content`, but treats files over the size limit as binary.
///
/// # Arguments
///
/// * `old_content` - Content from pristine
/// * `new_content` - Content from working copy
/// * `algorithm` - Diff algorithm to use
/// * `max_size` - Maximum size for text diffing
///
/// # Returns
///
/// A `CompareResult` (files over size limit are treated as binary).
pub fn compare_content_with_limit(
    old_content: &[u8],
    new_content: &[u8],
    algorithm: Algorithm,
    max_size: u64,
) -> CompareResult {
    // Check size limits
    if old_content.len() as u64 > max_size || new_content.len() as u64 > max_size {
        let old_encoding = detect_encoding(old_content);
        let new_encoding = detect_encoding(new_content);
        return CompareResult::binary(old_encoding, new_encoding);
    }

    compare_content(old_content, new_content, algorithm)
}

/// Generate diff operations between two content blobs.
///
/// Converts content to lines and runs the specified diff algorithm.
///
/// # Arguments
///
/// * `old_content` - Old content bytes
/// * `new_content` - New content bytes
/// * `algorithm` - Diff algorithm to use
///
/// # Returns
///
/// Vector of diff operations.
pub fn generate_diff(old_content: &[u8], new_content: &[u8], algorithm: Algorithm) -> Vec<DiffOp> {
    // Convert to lines for diffing
    let old_lines: Vec<Line> = Line::from_bytes(old_content);
    let new_lines: Vec<Line> = Line::from_bytes(new_content);

    // Use `diff_raw` instead of `diff` to skip the
    // `rewrite_positional_shifts` heuristic.  That heuristic is helpful
    // for **displaying** diffs to users but corrupts patch-theory graph
    // semantics: it rewrites pure insertions into Replace+Insert pairs,
    // which causes `globalize_replace` to delete unchanged vertices.
    // Records and graph operations need the algorithmically minimal
    // diff, not the user-friendly one.
    let result = diff_raw(&old_lines, &new_lines, algorithm);

    // Return operations
    result.ops().to_vec()
}

/// Check if content is identical (byte-for-byte).
///
/// # Arguments
///
/// * `old_content` - Old content bytes
/// * `new_content` - New content bytes
///
/// # Returns
///
/// `true` if the content is byte-for-byte identical.
pub fn content_identical(old_content: &[u8], new_content: &[u8]) -> bool {
    old_content == new_content
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // CompareResult Tests
    // ========================================================================

    #[test]
    fn test_compare_result_identical() {
        let result = CompareResult::identical(Encoding::Utf8);

        assert!(!result.has_changes());
        assert!(!result.is_binary);
        assert!(result.diff_ops.is_empty());
        assert_eq!(result.old_encoding, Encoding::Utf8);
        assert_eq!(result.new_encoding, Encoding::Utf8);
    }

    #[test]
    fn test_compare_result_binary() {
        let result = CompareResult::binary(Encoding::Utf8, Encoding::Binary);

        assert!(result.has_changes());
        assert!(result.is_binary);
        assert!(result.diff_ops.is_empty());
        assert_eq!(result.old_encoding, Encoding::Utf8);
        assert_eq!(result.new_encoding, Encoding::Binary);
    }


    #[test]
    fn test_compare_result_with_diff() {
        let ops = vec![DiffOp::Insert {
            old_pos: 0,
            new_pos: 0,
            len: 1,
        }];
        let result = CompareResult::with_diff(Encoding::Utf8, Encoding::Utf8, ops);

        assert!(result.has_changes());
        assert!(!result.is_binary);
        assert_eq!(result.diff_count(), 1);
    }

    #[test]
    fn test_compare_result_encoding_only_change() {
        let mut result = CompareResult::identical(Encoding::Utf8);
        result.new_encoding = Encoding::Binary;

        assert!(result.has_changes());
        assert!(result.encoding_only_change());
    }

    #[test]
    fn test_compare_result_default() {
        let result = CompareResult::default();

        assert!(!result.has_changes());
        assert_eq!(result.old_encoding, Encoding::Utf8);
    }

    // ========================================================================
    // Encoding Detection Tests
    // ========================================================================

    #[test]
    fn test_detect_encoding_utf8() {
        assert_eq!(detect_encoding(b"Hello, World!"), Encoding::Utf8);
        assert_eq!(detect_encoding(b""), Encoding::Utf8);
        assert_eq!(
            detect_encoding("UTF-8 with émojis 🎉".as_bytes()),
            Encoding::Utf8
        );
    }

    #[test]
    fn test_detect_encoding_binary_null() {
        assert_eq!(detect_encoding(&[0x00]), Encoding::Binary);
        assert_eq!(detect_encoding(b"text\x00binary"), Encoding::Binary);
    }

    #[test]
    fn test_detect_encoding_binary_invalid_utf8() {
        assert_eq!(detect_encoding(&[0xFF, 0xFE]), Encoding::Binary);
        assert_eq!(detect_encoding(&[0x80, 0x81, 0x82]), Encoding::Binary);
    }

    // ========================================================================
    // Is Binary Tests
    // ========================================================================

    #[test]
    fn test_is_binary_text() {
        assert!(!is_binary(b"Hello, World!"));
        assert!(!is_binary(b"Line 1\nLine 2\nLine 3\n"));
        assert!(!is_binary(b"Tab\there\nNewline"));
    }

    #[test]
    fn test_is_binary_null() {
        assert!(is_binary(&[0x00]));
        assert!(is_binary(b"text\x00more"));
    }

    #[test]
    fn test_is_binary_high_non_printable() {
        // More than 30% non-printable
        let content = vec![0x01, 0x02, 0x03, 0x04, b'a', b'b'];
        assert!(is_binary(&content));
    }

    #[test]
    fn test_is_binary_empty() {
        assert!(!is_binary(b""));
    }

    #[test]
    fn test_is_binary_whitespace() {
        // Whitespace characters should not count as non-printable
        assert!(!is_binary(b"\t\n\r "));
    }

    // ========================================================================
    // Content Comparison Tests
    // ========================================================================

    #[test]
    fn test_compare_content_identical() {
        let content = b"Hello, World!";
        let result = compare_content(content, content, Algorithm::Myers);

        assert!(!result.has_changes());
        assert!(result.diff_ops.is_empty());
    }

    #[test]
    fn test_compare_content_modified() {
        let old = b"line1\nline2\n";
        let new = b"line1\nmodified\n";

        let result = compare_content(old, new, Algorithm::Myers);

        assert!(result.has_changes());
        assert!(!result.is_binary);
        assert!(!result.diff_ops.is_empty());
    }

    #[test]
    fn test_compare_content_binary() {
        let old = b"text";
        let new = &[0x00, 0x01, 0x02];

        let result = compare_content(old, new, Algorithm::Myers);

        assert!(result.has_changes());
        assert!(result.is_binary);
        assert!(result.diff_ops.is_empty());
    }

    #[test]
    fn test_compare_content_new_file() {
        let old = b"";
        let new = b"new content\n";

        let result = compare_content(old, new, Algorithm::Myers);

        assert!(result.has_changes());
        assert!(!result.diff_ops.is_empty());
    }

    #[test]
    fn test_compare_content_deleted_file() {
        let old = b"old content\n";
        let new = b"";

        let result = compare_content(old, new, Algorithm::Myers);

        assert!(result.has_changes());
        assert!(!result.diff_ops.is_empty());
    }

    #[test]
    fn test_compare_content_patience() {
        let old = b"a\nb\nc\n";
        let new = b"a\nx\nc\n";

        let result = compare_content(old, new, Algorithm::Patience);

        assert!(result.has_changes());
        assert!(!result.diff_ops.is_empty());
    }

    // ========================================================================
    // Compare with Limit Tests
    // ========================================================================

    #[test]
    fn test_compare_content_with_limit_under() {
        let old = b"small";
        let new = b"modified";

        let result = compare_content_with_limit(old, new, Algorithm::Myers, 1024);

        assert!(result.has_changes());
        assert!(!result.is_binary); // Not treated as binary
    }

    #[test]
    fn test_compare_content_with_limit_over() {
        let old = b"content";
        let new = b"modified";

        // Set limit smaller than content
        let result = compare_content_with_limit(old, new, Algorithm::Myers, 5);

        assert!(result.has_changes());
        assert!(result.is_binary); // Treated as binary due to size
        assert!(result.diff_ops.is_empty());
    }

    #[test]
    fn test_compare_content_with_limit_old_over() {
        let old = b"this is a long old content";
        let new = b"short";

        let result = compare_content_with_limit(old, new, Algorithm::Myers, 10);

        assert!(result.is_binary);
    }

    #[test]
    fn test_compare_content_with_limit_new_over() {
        let old = b"short";
        let new = b"this is a long new content";

        let result = compare_content_with_limit(old, new, Algorithm::Myers, 10);

        assert!(result.is_binary);
    }

    // ========================================================================
    // Generate Diff Tests
    // ========================================================================

    #[test]
    fn test_generate_diff_empty() {
        let ops = generate_diff(b"same\n", b"same\n", Algorithm::Myers);
        // When content is identical, we may get Equal ops but no Insert/Delete/Replace
        let has_changes = ops.iter().any(|op| !matches!(op, DiffOp::Equal { .. }));
        assert!(!has_changes, "identical content should have no change ops");
    }

    #[test]
    fn test_generate_diff_insert() {
        let ops = generate_diff(b"", b"new\n", Algorithm::Myers);
        assert!(!ops.is_empty());
    }

    #[test]
    fn test_generate_diff_delete() {
        let ops = generate_diff(b"old\n", b"", Algorithm::Myers);
        assert!(!ops.is_empty());
    }

    #[test]
    fn test_generate_diff_replace() {
        let ops = generate_diff(b"old\n", b"new\n", Algorithm::Myers);
        assert!(!ops.is_empty());
    }

    // ========================================================================
    // Content Identical Tests
    // ========================================================================

    #[test]
    fn test_content_identical_true() {
        assert!(content_identical(b"same", b"same"));
        assert!(content_identical(b"", b""));
    }

    #[test]
    fn test_content_identical_false() {
        assert!(!content_identical(b"one", b"two"));
        assert!(!content_identical(b"", b"something"));
    }

    // ========================================================================
    // Clone and Debug Tests
    // ========================================================================

    #[test]
    fn test_compare_result_clone() {
        let result = CompareResult::with_diff(
            Encoding::Utf8,
            Encoding::Utf8,
            vec![DiffOp::Insert {
                old_pos: 0,
                new_pos: 0,
                len: 1,
            }],
        );
        let cloned = result.clone();

        assert_eq!(result, cloned);
    }

    #[test]
    fn test_compare_result_debug() {
        let result = CompareResult::identical(Encoding::Utf8);
        let debug = format!("{:?}", result);

        assert!(debug.contains("CompareResult"));
    }
}

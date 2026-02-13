//! Diff algorithms for Atomic VCS
//!
//! This module provides algorithms for computing the difference between two
//! sequences (typically lines of text or chunks of binary data). These diffs
//! are fundamental to version control - they tell us what changed between
//! the old and new versions of a file.
//!
//! # Algorithms
//!
//! We implement two classic diff algorithms:
//!
//! ## Myers Diff Algorithm
//!
//! The default algorithm, based on Eugene Myers' 1986 paper "An O(ND) Difference
//! Algorithm and Its Variations". This algorithm:
//!
//! - Finds the **shortest edit script** (minimum number of insertions + deletions)
//! - Runs in O(ND) time where N is the sum of sequence lengths and D is the edit distance
//! - Is optimal for most source code changes where edits are small
//!
//! ## Patience Diff Algorithm
//!
//! An alternative algorithm that often produces more readable diffs by:
//!
//! - First finding **unique matching lines** in both sequences
//! - Using the Longest Increasing Subsequence (LIS) of unique matches as anchors
//! - Recursively diffing the regions between anchors
//!
//! Patience diff is particularly good for:
//! - Code with repeated patterns (like closing braces)
//! - Large structural changes where Myers might sync on the wrong lines
//!
//! # Line Representation
//!
//! Lines are represented by the [`Line`] struct which supports:
//! - Zero-copy references to original content
//! - Fast hash-based equality checking
//! - Proper handling of trailing newlines
//!
//! # Diff Operations
//!
//! The result of a diff is a sequence of [`DiffOp`] operations:
//!
//! - `Equal`: Lines that appear in both sequences (unchanged)
//! - `Insert`: Lines that appear only in the new sequence (added)
//! - `Delete`: Lines that appear only in the old sequence (removed)
//! - `Replace`: Lines that were replaced (combined delete + insert)
//!
//! # Binary Diff Support
//!
//! For binary files (or files without a consistent line structure), we provide
//! content-defined chunking using a rolling hash. This allows efficient diffing
//! of binary data by finding common chunks between versions.
//!
//! # Example
//!
//! ```rust
//! use atomic_core::diff::{diff, Algorithm, Line};
//!
//! let old_text = "hello\nworld\n";
//! let new_text = "hello\nbeautiful\nworld\n";
//!
//! let old_lines: Vec<Line> = Line::from_text(old_text);
//! let new_lines: Vec<Line> = Line::from_text(new_text);
//!
//! let ops = diff(&old_lines, &new_lines, Algorithm::Myers);
//!
//! // ops will contain:
//! // - Equal { old: 0..1, new: 0..1 }  (hello)
//! // - Insert { new: 1..2 }            (beautiful)
//! // - Equal { old: 1..2, new: 2..3 }  (world)
//! ```
//!
//! # Integration with Atomic
//!
//! The diff module is used by the `record` module to detect changes between
//! the working copy and the last recorded state. The resulting operations
//! are converted into `GraphOp`s and `Atom`s for storage in changes.
//!
//! ```text
//! Working Copy    Pristine Graph
//!      │               │
//!      └───────┬───────┘
//!              ▼
//!         diff module
//!              │
//!              ▼
//!     List of DiffOps
//!              │
//!              ▼
//!   record module (Hunks/Atoms)
//!              │
//!              ▼
//!      Change file
//! ```
//!
//! # Performance Considerations
//!
//! - **Line hashing**: Lines are hashed once and cached for O(1) equality checks
//! - **Early termination**: Common prefixes/suffixes are stripped before diffing
//! - **Binary chunking**: Rolling hash enables O(n) chunking of binary files
//! - **Memory**: We avoid copying line content; Lines hold references

mod algorithm;
pub mod display;
pub mod inline;
mod line;
mod myers;
mod ops;
mod patience;
pub mod semantic;
pub mod semantic_to_crdt;
mod split;
pub mod token;
pub mod word;

// Re-export public types
pub use algorithm::Algorithm;
pub use display::{DiffStats, DisplayLine, LinePair, LineStatus, SideBySideDiff, UnifiedDiff};
pub use inline::{compute_inline_diff, ChangeHunk, HunkKind, InlineDiff};
pub use line::Line;
pub use ops::{DiffOp, DiffResult, Replacement};
pub use semantic::{
    semantic_diff, semantic_diff_with_config, LineChange, SemanticDiff, SemanticDiffConfig,
    SemanticDiffStats, SemanticLine, TokenChange,
};
pub use semantic_to_crdt::{
    convert_diff_to_file_ops, convert_diff_to_file_ops_with_config, ConversionConfig,
    ConversionError, ConversionResult, ConversionStats, SemanticToCrdt,
};
pub use split::{LineSplit, Separator};
pub use token::{Token, TokenKind, Tokenizer, TokenizerConfig};
pub use word::{
    word_diff, word_diff_str, word_diff_with_config, WordDiffConfig, WordDiffOp, WordDiffResult,
};

/// Compute the diff between two sequences of lines.
///
/// This is the main entry point for diffing. It takes two slices of [`Line`]s
/// and returns a [`DiffResult`] containing the operations needed to transform
/// the old sequence into the new one.
///
/// # Arguments
///
/// * `old` - The original sequence of lines
/// * `new` - The modified sequence of lines
/// * `algorithm` - Which diff algorithm to use
///
/// # Returns
///
/// A [`DiffResult`] containing the list of [`DiffOp`]s and metadata.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::{diff, Algorithm, Line};
///
/// let old = vec![
///     Line::new(b"first\n"),
///     Line::new(b"second\n"),
///     Line::new(b"third\n"),
/// ];
/// let new = vec![
///     Line::new(b"first\n"),
///     Line::new(b"SECOND\n"),
///     Line::new(b"third\n"),
/// ];
///
/// let result = diff(&old, &new, Algorithm::Myers);
/// assert!(!result.is_empty());
/// ```
pub fn diff<'a>(old: &[Line<'a>], new: &[Line<'a>], algorithm: Algorithm) -> DiffResult {
    // Optimization: strip common prefix and suffix
    let (prefix_len, suffix_len) = common_affixes(old, new);

    let old_mid = &old[prefix_len..old.len().saturating_sub(suffix_len)];
    let new_mid = &new[prefix_len..new.len().saturating_sub(suffix_len)];

    // If everything is equal, return early
    if old_mid.is_empty() && new_mid.is_empty() {
        return DiffResult::equal(old.len());
    }

    // Dispatch to the selected algorithm
    let mut ops = match algorithm {
        Algorithm::Myers => myers::diff(old_mid, new_mid),
        Algorithm::Patience => patience::diff(old_mid, new_mid),
    };

    // Adjust offsets for the stripped prefix
    ops.adjust_offsets(prefix_len);

    // Add the equal prefix and suffix
    if prefix_len > 0 {
        ops.prepend_equal(0, prefix_len);
    }
    if suffix_len > 0 {
        let old_start = old.len() - suffix_len;
        let new_start = new.len() - suffix_len;
        ops.append_equal(old_start, new_start, suffix_len);
    }

    // Post-process: detect positional shifts that should be Replace ops.
    //
    // Myers finds the minimal edit (LCS), which can match identical lines
    // at different positions. For example:
    //   old: [A, B]  new: [A, C, B]
    // Myers says: Equal(A), Insert(C), Equal(B) — treating B as "moved down".
    //
    // But if the user modified line 2 (B→C) and added a new line 3 (B),
    // the intent is: Equal(A), Replace(B→C), Insert(B).
    //
    // We detect this pattern: when an Equal op maps old_pos N to new_pos M
    // where M > N (line shifted down), AND there's an Insert immediately
    // before it that occupies the original position, convert to Replace+Insert.
    ops = rewrite_positional_shifts(ops, old, new);

    ops
}

/// Rewrite diff ops to convert positional shifts into Replace operations.
///
/// When a line appears at the same content but a different position, and
/// there are insertions that "pushed" it down, this is better represented
/// as a modification of the original line plus an insertion of the new copy.
fn rewrite_positional_shifts<'a>(
    result: DiffResult,
    old: &[Line<'a>],
    new: &[Line<'a>],
) -> DiffResult {
    let ops = result.ops();
    if ops.len() < 2 {
        return result;
    }

    let mut new_ops: Vec<DiffOp> = Vec::with_capacity(ops.len());
    let mut i = 0;

    while i < ops.len() {
        // Look for pattern: Insert then Equal where the Equal's old_pos
        // matches where the Insert landed (the line "shifted down")
        if i + 1 < ops.len() {
            if let (
                DiffOp::Insert {
                    old_pos: ins_old_pos,
                    new_pos: ins_new_pos,
                    len: ins_len,
                },
                DiffOp::Equal {
                    old_pos: eq_old_pos,
                    new_pos: eq_new_pos,
                    len: eq_len,
                },
            ) = (&ops[i], &ops[i + 1])
            {
                // The Equal starts at the same old position as the Insert,
                // meaning the Insert "pushed" the Equal line to a new position.
                // AND the Equal maps to a different new position than old position
                // (the line shifted).
                if *eq_old_pos == *ins_old_pos && *eq_new_pos > *eq_old_pos {
                    // The Insert occupies the old position, and the Equal
                    // line was "pushed down". This MIGHT be a modification
                    // (user changed the line and a copy of the old content
                    // ended up below it). We only rewrite when the inserted
                    // content is SIMILAR to the old line — sharing tokens
                    // indicates a word-level edit (e.g. "line two" → "line three").
                    // If the content is completely different, it's a genuine
                    // insertion and we leave it alone.
                    let overlap = (*ins_len).min(*eq_len);

                    if overlap > 0 {
                        let mut rewrote = false;

                        for j in 0..overlap {
                            let o_idx = eq_old_pos + j;
                            let n_idx = ins_new_pos + j;
                            let eq_n_idx = eq_new_pos + j;

                            if o_idx < old.len() && n_idx < new.len() && old[o_idx] != new[n_idx] {
                                // Check similarity: the inserted line should share
                                // significant content with the old line to qualify
                                // as a modification. Compare tokens (words).
                                let old_content = old[o_idx].content_without_newline();
                                let new_content = new[n_idx].content_without_newline();

                                if lines_are_similar(old_content, new_content) {
                                    // Similar content → this is a modification
                                    new_ops.push(DiffOp::Replace {
                                        old_pos: o_idx,
                                        old_len: 1,
                                        new_pos: n_idx,
                                        new_len: 1,
                                    });
                                    // The displaced Equal line is a new copy
                                    new_ops.push(DiffOp::Insert {
                                        old_pos: o_idx + 1,
                                        new_pos: eq_n_idx,
                                        len: 1,
                                    });
                                    rewrote = true;
                                    continue;
                                }
                            }

                            // Not similar or same content — keep original ops
                            new_ops.push(DiffOp::Insert {
                                old_pos: *ins_old_pos + j,
                                new_pos: n_idx,
                                len: 1,
                            });
                            new_ops.push(DiffOp::Equal {
                                old_pos: o_idx,
                                new_pos: eq_n_idx,
                                len: 1,
                            });
                        }

                        // Remaining Insert lines (beyond the overlap)
                        if *ins_len > overlap {
                            new_ops.push(DiffOp::Insert {
                                old_pos: ins_old_pos + overlap,
                                new_pos: ins_new_pos + overlap,
                                len: ins_len - overlap,
                            });
                        }

                        // Remaining Equal lines (beyond the overlap)
                        if *eq_len > overlap && rewrote {
                            // Old lines consumed by Replace — remaining are inserts
                            new_ops.push(DiffOp::Insert {
                                old_pos: eq_old_pos + overlap,
                                new_pos: eq_new_pos + overlap,
                                len: eq_len - overlap,
                            });
                        } else if *eq_len > overlap {
                            new_ops.push(DiffOp::Equal {
                                old_pos: eq_old_pos + overlap,
                                new_pos: eq_new_pos + overlap,
                                len: eq_len - overlap,
                            });
                        }

                        i += 2;
                        continue;
                    }
                }
            }
        }

        // No pattern match — pass through
        new_ops.push(ops[i].clone());
        i += 1;
    }

    DiffResult::with_ops(new_ops)
}

/// Check if two lines are similar enough to be considered a modification
/// rather than unrelated content. Uses simple word overlap: if more than
/// half the whitespace-separated words match, the lines are similar.
fn lines_are_similar(old: &[u8], new: &[u8]) -> bool {
    let old_str = std::str::from_utf8(old).unwrap_or("");
    let new_str = std::str::from_utf8(new).unwrap_or("");

    let old_words: Vec<&str> = old_str.split_whitespace().collect();
    let new_words: Vec<&str> = new_str.split_whitespace().collect();

    // Both empty or single-char lines — not enough signal
    if old_words.is_empty() && new_words.is_empty() {
        return false;
    }

    let max_words = old_words.len().max(new_words.len());
    if max_words == 0 {
        return false;
    }

    let matching = old_words.iter().filter(|w| new_words.contains(w)).count();

    // More than 30% word overlap → similar (modification)
    // This catches "line two" → "line three" (1/2 = 50% overlap on "line")
    // but rejects "line3" vs "line2" insertion (completely different words)
    matching * 100 / max_words > 30
}

/// Find the length of common prefix and suffix between two sequences.
///
/// This optimization significantly speeds up diffing when changes are
/// localized to a small portion of the file.
///
/// # Suffix Limiting
///
/// The suffix is reduced if stripping it would leave one middle section
/// empty while the other still has content. In that case, the diff
/// algorithm would see only insertions or deletions, missing modifications.
///
/// Example: old = `[A, B]`, new = `[A, C, B]`
///   - prefix = 1 (`A`), naive suffix = 1 (`B`)
///   - old_mid = `[]`, new_mid = `[C]` → empty vs non-empty
///   - Reduced suffix = 0 → old_mid = `[B]`, new_mid = `[C, B]`
///   - Now the diff algorithm can detect `B` → `C` as a Replace
///     followed by an Insert of `B`
fn common_affixes<T: PartialEq>(old: &[T], new: &[T]) -> (usize, usize) {
    // Common prefix
    let prefix_len = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();

    // Common suffix (but don't overlap with prefix)
    let remaining_old = old.len() - prefix_len;
    let remaining_new = new.len() - prefix_len;
    let max_suffix = remaining_old.min(remaining_new);

    let suffix_len = old[prefix_len..]
        .iter()
        .rev()
        .zip(new[prefix_len..].iter().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();

    // Don't let the suffix strip make one middle section empty while the
    // other still has content. That hides modifications as pure inserts
    // or pure deletes — the diff algorithm needs both sides non-empty
    // to detect Replace operations.
    let old_mid = remaining_old - suffix_len;
    let new_mid = remaining_new - suffix_len;

    if (old_mid == 0) != (new_mid == 0) {
        // One side is empty, the other isn't. Reduce suffix until both
        // sides have content (or suffix is zero).
        let mut reduced = suffix_len;
        while reduced > 0 {
            reduced -= 1;
            let om = remaining_old - reduced;
            let nm = remaining_new - reduced;
            if (om > 0 && nm > 0) || reduced == 0 {
                return (prefix_len, reduced);
            }
        }
        return (prefix_len, 0);
    }

    (prefix_len, suffix_len)
}

/// Diff two byte slices as text, splitting on newlines.
///
/// This is a convenience function that handles the common case of diffing
/// two text files. It splits both inputs on newlines, diffs the resulting
/// lines, and returns the operations.
///
/// # Arguments
///
/// * `old` - The original text content
/// * `new` - The modified text content
/// * `algorithm` - Which diff algorithm to use
///
/// # Returns
///
/// A [`DiffResult`] containing the diff operations.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::{diff_text, Algorithm};
///
/// let old = b"line1\nline2\nline3\n";
/// let new = b"line1\nmodified\nline3\n";
///
/// let result = diff_text(old, new, Algorithm::Myers);
/// assert!(!result.is_empty());
/// ```
pub fn diff_text(old: &[u8], new: &[u8], algorithm: Algorithm) -> DiffResult {
    let old_lines: Vec<Line> = Line::from_bytes(old);
    let new_lines: Vec<Line> = Line::from_bytes(new);
    diff(&old_lines, &new_lines, algorithm)
}

/// Diff two byte slices as text using a custom separator pattern.
///
/// This allows diffing with custom line separators (e.g., for languages
/// that use different conventions or for record-oriented data).
///
/// # Arguments
///
/// * `old` - The original content
/// * `new` - The modified content
/// * `separator` - The separator to use for splitting
/// * `algorithm` - Which diff algorithm to use
///
/// # Returns
///
/// A [`DiffResult`] containing the diff operations.
pub fn diff_with_separator<'a>(
    old: &'a [u8],
    new: &'a [u8],
    separator: &Separator,
    algorithm: Algorithm,
) -> DiffResult {
    let old_lines: Vec<Line> = LineSplit::new(old, separator).collect();
    let new_lines: Vec<Line> = LineSplit::new(new, separator).collect();
    diff(&old_lines, &new_lines, algorithm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diff_identical() {
        let text = b"line1\nline2\nline3\n";
        let result = diff_text(text, text, Algorithm::Myers);
        assert!(result.is_unchanged());
    }

    #[test]
    fn test_diff_empty() {
        let result = diff_text(b"", b"", Algorithm::Myers);
        assert!(result.is_empty());
        assert!(result.is_unchanged());
    }

    #[test]
    fn test_diff_insert() {
        let old = b"line1\nline3\n";
        let new = b"line1\nline2\nline3\n";
        let result = diff_text(old, new, Algorithm::Myers);
        assert!(!result.is_unchanged());
        assert_eq!(result.insertions(), 1);
        assert_eq!(result.deletions(), 0);
    }

    #[test]
    fn test_diff_delete() {
        let old = b"line1\nline2\nline3\n";
        let new = b"line1\nline3\n";
        let result = diff_text(old, new, Algorithm::Myers);
        assert!(!result.is_unchanged());
        assert_eq!(result.deletions(), 1);
        assert_eq!(result.insertions(), 0);
    }

    #[test]
    fn test_diff_replace() {
        let old = b"line1\nold\nline3\n";
        let new = b"line1\nnew\nline3\n";
        let result = diff_text(old, new, Algorithm::Myers);
        assert!(!result.is_unchanged());
    }

    #[test]
    fn test_common_affixes() {
        let old = vec![1, 2, 3, 4, 5];
        let new = vec![1, 2, 9, 4, 5];
        let (prefix, suffix) = common_affixes(&old, &new);
        assert_eq!(prefix, 2);
        assert_eq!(suffix, 2);
    }

    #[test]
    fn test_common_affixes_all_equal() {
        let old = vec![1, 2, 3];
        let new = vec![1, 2, 3];
        let (prefix, suffix) = common_affixes(&old, &new);
        assert_eq!(prefix, 3);
        assert_eq!(suffix, 0); // All consumed by prefix
    }

    #[test]
    fn test_common_affixes_none() {
        let old = vec![1, 2, 3];
        let new = vec![4, 5, 6];
        let (prefix, suffix) = common_affixes(&old, &new);
        assert_eq!(prefix, 0);
        assert_eq!(suffix, 0);
    }

    /// Test that modifying a line and inserting a copy of the original
    /// is detected as a Replace + Insert, not just an Insert.
    ///
    /// Before: line one\nline two\n
    /// After:  line one\nline three\nline two\n
    ///
    /// The user edited line 2 (two → three) and added a new line 3.
    /// The diff should show a Replace on "line two" → "line three"
    /// plus an Insert of "line two", NOT just an Insert of "line three".
    #[test]
    fn test_diff_modify_then_insert_original() {
        let old = b"line one\nline two\n";
        let new = b"line one\nline three\nline two\n";

        let result = diff_text(old, new, Algorithm::Myers);
        let ops = result.ops();

        // Should have: Equal(line one), Replace(line two → line three), Insert(line two)
        // NOT: Equal(line one), Insert(line three), Equal(line two)
        let has_replace = ops.iter().any(|op| matches!(op, DiffOp::Replace { .. }));
        let has_insert = ops.iter().any(|op| matches!(op, DiffOp::Insert { .. }));

        assert!(
            has_replace,
            "Expected a Replace op for 'line two' → 'line three', got: {:?}",
            ops
        );
        assert!(
            has_insert,
            "Expected an Insert op for the new 'line two', got: {:?}",
            ops
        );

        // Verify the Replace is at position 1 (line two → line three)
        let replace = ops.iter().find(|op| matches!(op, DiffOp::Replace { .. }));
        if let Some(DiffOp::Replace {
            old_pos,
            old_len,
            new_pos,
            new_len,
        }) = replace
        {
            assert_eq!(*old_pos, 1, "Replace should be at old line 1 (line two)");
            assert_eq!(*old_len, 1);
            assert_eq!(*new_pos, 1, "Replace should be at new line 1 (line three)");
            assert_eq!(*new_len, 1);
        }
    }

    /// Same test with Patience algorithm.
    #[test]
    fn test_diff_modify_then_insert_original_patience() {
        let old = b"line one\nline two\n";
        let new = b"line one\nline three\nline two\n";

        let result = diff_text(old, new, Algorithm::Patience);
        let ops = result.ops();

        let has_replace = ops.iter().any(|op| matches!(op, DiffOp::Replace { .. }));
        assert!(
            has_replace,
            "Patience should also detect Replace for 'line two' → 'line three', got: {:?}",
            ops
        );
    }

    /// Ensure the fix doesn't break the simple insert case where the
    /// suffix match is legitimate.
    #[test]
    fn test_diff_pure_insert_still_works() {
        let old = b"aaa\nccc\n";
        let new = b"aaa\nbbb\nccc\n";

        let result = diff_text(old, new, Algorithm::Myers);
        let ops = result.ops();

        // This IS a pure insert — "ccc" genuinely survived, "bbb" was inserted.
        // Both old lines appear in new unchanged.
        let has_insert = ops.iter().any(|op| matches!(op, DiffOp::Insert { .. }));
        assert!(has_insert, "Pure insert case should still work: {:?}", ops);
    }

    #[test]
    fn test_diff_patience_vs_myers() {
        // Both algorithms should produce valid diffs
        let old = b"a\nb\nc\nd\n";
        let new = b"a\nx\nc\ny\n";

        let myers = diff_text(old, new, Algorithm::Myers);
        let patience = diff_text(old, new, Algorithm::Patience);

        // Both should indicate changes happened
        assert!(!myers.is_unchanged());
        assert!(!patience.is_unchanged());
    }
}

//! Word-level diff algorithm for fine-grained change detection.
//!
//! This module provides token-level diffing within lines, enabling
//! precise identification of what changed - down to individual words
//! or characters. This is essential for code reviews where you want
//! to see exactly what changed, not just that a line changed.
//!
//! # Use Case: Code Reviews
//!
//! Line-level diffs often show entire lines as changed when only a
//! small part was modified. Word-level diffs solve this by:
//!
//! 1. Identifying which lines changed (using line-level diff)
//! 2. For changed line pairs, computing token-level diff
//! 3. Highlighting exactly which tokens were added/removed/modified
//!
//! # Display Pattern
//!
//! The visual pattern this enables:
//!
//! ```text
//! - const result = calculateSum(a, b);        <- light red background
//! + const result = calculateSum(a, b, c);     <- light green background
//!                                   ^^^^      <- dark green: ", c" added
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_core::diff::word::{word_diff, WordDiffOp};
//!
//! let old = b"const result = sum(a, b);";
//! let new = b"const result = sum(a, b, c);";
//!
//! let result = word_diff(old, new);
//! assert!(result.has_changes());
//!
//! // Find the insertion
//! let insertions: Vec<_> = result.ops().iter()
//!     .filter(|op| matches!(op, WordDiffOp::Insert { .. }))
//!     .collect();
//! assert!(!insertions.is_empty());
//! ```
//!
//! # CRDT-Style Tracking
//!
//! The word-level diff can be used to track changes at a granular level,
//! enabling features like:
//!
//! - Per-word author attribution
//! - Fine-grained AI provenance tracking
//! - Precise merge conflict resolution
//!
//! # Algorithm
//!
//! The word-level diff uses the same LCS-based approach as line-level diff,
//! but operates on tokens instead of lines. This gives optimal edit distance
//! while being fast enough for interactive use.

use super::token::{Token, Tokenizer, TokenizerConfig};
use std::ops::Range;

/// A word-level diff operation.
///
/// These operations describe how to transform the old token sequence
/// into the new token sequence. Each operation references ranges in
/// the original token vectors.
///
/// # Operation Types
///
/// - `Equal`: Tokens that match in both sequences (unchanged)
/// - `Insert`: Tokens that appear only in the new sequence (added)
/// - `Delete`: Tokens that appear only in the old sequence (removed)
/// - `Replace`: Tokens that differ between sequences (modified)
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::word::{word_diff, WordDiffOp};
///
/// let result = word_diff(b"hello world", b"hello there");
///
/// for op in result.ops() {
///     match op {
///         WordDiffOp::Equal { .. } => println!("unchanged"),
///         WordDiffOp::Insert { .. } => println!("added"),
///         WordDiffOp::Delete { .. } => println!("removed"),
///         WordDiffOp::Replace { .. } => println!("replaced"),
///     }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WordDiffOp {
    /// Tokens that are equal in both sequences.
    ///
    /// The ranges refer to token indices in the old and new sequences.
    Equal {
        /// Range of tokens in the old sequence.
        old_range: Range<usize>,
        /// Range of tokens in the new sequence.
        new_range: Range<usize>,
    },

    /// Tokens that were inserted (only in new sequence).
    Insert {
        /// Position in old sequence where insertion occurs.
        old_pos: usize,
        /// Range of tokens in the new sequence that were inserted.
        new_range: Range<usize>,
    },

    /// Tokens that were deleted (only in old sequence).
    Delete {
        /// Range of tokens in the old sequence that were deleted.
        old_range: Range<usize>,
        /// Position in new sequence where deletion occurred.
        new_pos: usize,
    },

    /// Tokens that were replaced (different in old and new).
    ///
    /// This is semantically equivalent to a delete followed by an insert,
    /// but grouped for cleaner display.
    Replace {
        /// Range of tokens in the old sequence that were replaced.
        old_range: Range<usize>,
        /// Range of tokens in the new sequence that replaced them.
        new_range: Range<usize>,
    },
}

impl WordDiffOp {
    /// Check if this is an equal (unchanged) operation.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::word::WordDiffOp;
    ///
    /// let op = WordDiffOp::Equal { old_range: 0..1, new_range: 0..1 };
    /// assert!(op.is_equal());
    /// ```
    #[inline]
    pub fn is_equal(&self) -> bool {
        matches!(self, WordDiffOp::Equal { .. })
    }

    /// Check if this is an insert operation.
    #[inline]
    pub fn is_insert(&self) -> bool {
        matches!(self, WordDiffOp::Insert { .. })
    }

    /// Check if this is a delete operation.
    #[inline]
    pub fn is_delete(&self) -> bool {
        matches!(self, WordDiffOp::Delete { .. })
    }

    /// Check if this is a replace operation.
    #[inline]
    pub fn is_replace(&self) -> bool {
        matches!(self, WordDiffOp::Replace { .. })
    }

    /// Check if this is a change (insert, delete, or replace).
    ///
    /// Returns `true` for any operation that modifies content.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::word::WordDiffOp;
    ///
    /// let equal = WordDiffOp::Equal { old_range: 0..1, new_range: 0..1 };
    /// let insert = WordDiffOp::Insert { old_pos: 0, new_range: 0..1 };
    ///
    /// assert!(!equal.is_change());
    /// assert!(insert.is_change());
    /// ```
    #[inline]
    pub fn is_change(&self) -> bool {
        !self.is_equal()
    }

    /// Get the old range if this operation references old tokens.
    ///
    /// Returns `Some(range)` for Equal, Delete, and Replace operations.
    /// Returns `None` for Insert operations.
    pub fn old_range(&self) -> Option<Range<usize>> {
        match self {
            WordDiffOp::Equal { old_range, .. } => Some(old_range.clone()),
            WordDiffOp::Delete { old_range, .. } => Some(old_range.clone()),
            WordDiffOp::Replace { old_range, .. } => Some(old_range.clone()),
            WordDiffOp::Insert { .. } => None,
        }
    }

    /// Get the new range if this operation references new tokens.
    ///
    /// Returns `Some(range)` for Equal, Insert, and Replace operations.
    /// Returns `None` for Delete operations.
    pub fn new_range(&self) -> Option<Range<usize>> {
        match self {
            WordDiffOp::Equal { new_range, .. } => Some(new_range.clone()),
            WordDiffOp::Insert { new_range, .. } => Some(new_range.clone()),
            WordDiffOp::Replace { new_range, .. } => Some(new_range.clone()),
            WordDiffOp::Delete { .. } => None,
        }
    }

    /// Get the number of old tokens affected by this operation.
    pub fn old_len(&self) -> usize {
        match self {
            WordDiffOp::Equal { old_range, .. } => old_range.len(),
            WordDiffOp::Delete { old_range, .. } => old_range.len(),
            WordDiffOp::Replace { old_range, .. } => old_range.len(),
            WordDiffOp::Insert { .. } => 0,
        }
    }

    /// Get the number of new tokens affected by this operation.
    pub fn new_len(&self) -> usize {
        match self {
            WordDiffOp::Equal { new_range, .. } => new_range.len(),
            WordDiffOp::Insert { new_range, .. } => new_range.len(),
            WordDiffOp::Replace { new_range, .. } => new_range.len(),
            WordDiffOp::Delete { .. } => 0,
        }
    }
}

impl std::fmt::Display for WordDiffOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WordDiffOp::Equal { old_range, new_range } => {
                write!(f, "Equal(old:{:?}, new:{:?})", old_range, new_range)
            }
            WordDiffOp::Insert { old_pos, new_range } => {
                write!(f, "Insert(@{}, new:{:?})", old_pos, new_range)
            }
            WordDiffOp::Delete { old_range, new_pos } => {
                write!(f, "Delete(old:{:?}, @{})", old_range, new_pos)
            }
            WordDiffOp::Replace { old_range, new_range } => {
                write!(f, "Replace(old:{:?}, new:{:?})", old_range, new_range)
            }
        }
    }
}

/// Result of a word-level diff operation.
///
/// Contains the diff operations and references to the original tokens,
/// allowing the caller to extract the actual content that changed.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::word::word_diff;
///
/// let result = word_diff(b"hello world", b"hello there world");
///
/// println!("Changes: {}", result.has_changes());
/// println!("Insertions: {}", result.insertions());
/// println!("Deletions: {}", result.deletions());
/// ```
#[derive(Debug, Clone)]
pub struct WordDiffResult<'a> {
    /// The diff operations.
    ops: Vec<WordDiffOp>,

    /// Tokens from the old content.
    old_tokens: Vec<Token<'a>>,

    /// Tokens from the new content.
    new_tokens: Vec<Token<'a>>,
}

impl<'a> WordDiffResult<'a> {
    /// Create a new empty diff result.
    fn new(old_tokens: Vec<Token<'a>>, new_tokens: Vec<Token<'a>>) -> Self {
        Self {
            ops: Vec::new(),
            old_tokens,
            new_tokens,
        }
    }

    /// Create a result indicating no changes (all equal).
    fn all_equal(old_tokens: Vec<Token<'a>>, new_tokens: Vec<Token<'a>>) -> Self {
        let len = old_tokens.len();
        let ops = if len > 0 {
            vec![WordDiffOp::Equal {
                old_range: 0..len,
                new_range: 0..len,
            }]
        } else {
            vec![]
        };
        Self {
            ops,
            old_tokens,
            new_tokens,
        }
    }

    /// Get the diff operations.
    ///
    /// These describe how to transform the old tokens into the new tokens.
    #[inline]
    pub fn ops(&self) -> &[WordDiffOp] {
        &self.ops
    }

    /// Get the old tokens.
    #[inline]
    pub fn old_tokens(&self) -> &[Token<'a>] {
        &self.old_tokens
    }

    /// Get the new tokens.
    #[inline]
    pub fn new_tokens(&self) -> &[Token<'a>] {
        &self.new_tokens
    }

    /// Check if there are any changes.
    ///
    /// Returns `true` if any operation is not Equal.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::diff::word::word_diff;
    ///
    /// let same = word_diff(b"hello", b"hello");
    /// assert!(!same.has_changes());
    ///
    /// let different = word_diff(b"hello", b"world");
    /// assert!(different.has_changes());
    /// ```
    pub fn has_changes(&self) -> bool {
        self.ops.iter().any(|op| op.is_change())
    }

    /// Check if the sequences are identical.
    ///
    /// Returns `true` if all operations are Equal.
    #[inline]
    pub fn is_unchanged(&self) -> bool {
        !self.has_changes()
    }

    /// Check if the result is empty (both sequences were empty).
    pub fn is_empty(&self) -> bool {
        self.old_tokens.is_empty() && self.new_tokens.is_empty()
    }

    /// Count the number of inserted tokens.
    pub fn insertions(&self) -> usize {
        self.ops
            .iter()
            .map(|op| match op {
                WordDiffOp::Insert { new_range, .. } => new_range.len(),
                WordDiffOp::Replace { new_range, .. } => new_range.len(),
                _ => 0,
            })
            .sum()
    }

    /// Count the number of deleted tokens.
    pub fn deletions(&self) -> usize {
        self.ops
            .iter()
            .map(|op| match op {
                WordDiffOp::Delete { old_range, .. } => old_range.len(),
                WordDiffOp::Replace { old_range, .. } => old_range.len(),
                _ => 0,
            })
            .sum()
    }

    /// Get the edit distance (number of token changes).
    ///
    /// This counts insertions + deletions, where replacements count
    /// as both a deletion and an insertion.
    pub fn edit_distance(&self) -> usize {
        self.insertions() + self.deletions()
    }

    /// Iterate over only the change operations (non-equal).
    pub fn changes(&self) -> impl Iterator<Item = &WordDiffOp> {
        self.ops.iter().filter(|op| op.is_change())
    }

    /// Get tokens for an old range.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    pub fn old_tokens_in_range(&self, range: &Range<usize>) -> &[Token<'a>] {
        &self.old_tokens[range.clone()]
    }

    /// Get tokens for a new range.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    pub fn new_tokens_in_range(&self, range: &Range<usize>) -> &[Token<'a>] {
        &self.new_tokens[range.clone()]
    }

    /// Get the concatenated content of old tokens in a range.
    pub fn old_content_in_range(&self, range: &Range<usize>) -> String {
        self.old_tokens[range.clone()]
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .concat()
    }

    /// Get the concatenated content of new tokens in a range.
    pub fn new_content_in_range(&self, range: &Range<usize>) -> String {
        self.new_tokens[range.clone()]
            .iter()
            .map(|t| t.as_str())
            .collect::<Vec<_>>()
            .concat()
    }

    /// Add an operation to the result.
    fn push(&mut self, op: WordDiffOp) {
        self.ops.push(op);
    }
}

/// Configuration for word-level diffing.
///
/// Controls how tokenization and comparison are performed.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::word::WordDiffConfig;
///
/// // Ignore whitespace changes
/// let config = WordDiffConfig::default().with_ignore_whitespace(true);
///
/// // Use minimal tokenization (faster, less semantic)
/// let minimal = WordDiffConfig::minimal();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordDiffConfig {
    /// Configuration for tokenization.
    pub tokenizer_config: TokenizerConfig,

    /// Whether to ignore whitespace-only changes.
    ///
    /// When true, sequences that differ only in whitespace are
    /// considered equal.
    pub ignore_whitespace_changes: bool,

    /// Whether to ignore changes that only affect whitespace tokens.
    ///
    /// Different from `ignore_whitespace_changes`: this filters out
    /// whitespace tokens entirely before diffing.
    pub filter_whitespace: bool,
}

impl Default for WordDiffConfig {
    /// Returns default configuration with code-aware tokenization.
    fn default() -> Self {
        Self {
            tokenizer_config: TokenizerConfig::default(),
            ignore_whitespace_changes: false,
            filter_whitespace: false,
        }
    }
}

impl WordDiffConfig {
    /// Create a new default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a minimal configuration for faster, simpler tokenization.
    pub fn minimal() -> Self {
        Self {
            tokenizer_config: TokenizerConfig::minimal(),
            ignore_whitespace_changes: false,
            filter_whitespace: false,
        }
    }

    /// Create a configuration that ignores whitespace.
    pub fn ignoring_whitespace() -> Self {
        Self {
            tokenizer_config: TokenizerConfig::default(),
            ignore_whitespace_changes: true,
            filter_whitespace: true,
        }
    }

    /// Set whether to ignore whitespace changes (builder method).
    pub fn with_ignore_whitespace(mut self, ignore: bool) -> Self {
        self.ignore_whitespace_changes = ignore;
        self.filter_whitespace = ignore;
        self
    }

    /// Set the tokenizer configuration.
    pub fn with_tokenizer(mut self, config: TokenizerConfig) -> Self {
        self.tokenizer_config = config;
        self
    }
}

/// Compute word-level diff between two byte slices.
///
/// This is the main entry point for word-level diffing. It tokenizes
/// both inputs and computes the optimal edit sequence.
///
/// # Arguments
///
/// * `old` - The original content
/// * `new` - The modified content
///
/// # Returns
///
/// A `WordDiffResult` containing the diff operations and token references.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::word::word_diff;
///
/// let old = b"const x = 1;";
/// let new = b"const x = 2;";
///
/// let result = word_diff(old, new);
/// assert!(result.has_changes());
/// ```
pub fn word_diff<'a>(old: &'a [u8], new: &'a [u8]) -> WordDiffResult<'a> {
    word_diff_with_config(old, new, &WordDiffConfig::default())
}

/// Compute word-level diff with custom configuration.
///
/// # Arguments
///
/// * `old` - The original content
/// * `new` - The modified content
/// * `config` - Configuration options
///
/// # Returns
///
/// A `WordDiffResult` containing the diff operations.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::word::{word_diff_with_config, WordDiffConfig};
///
/// let config = WordDiffConfig::ignoring_whitespace();
/// let result = word_diff_with_config(b"a  b", b"a b", &config);
///
/// // With ignore_whitespace, these might be considered equal
/// ```
pub fn word_diff_with_config<'a>(
    old: &'a [u8],
    new: &'a [u8],
    config: &WordDiffConfig,
) -> WordDiffResult<'a> {
    // Tokenize both inputs
    let old_tokens: Vec<Token<'a>> =
        Tokenizer::with_config(old, config.tokenizer_config.clone()).collect();
    let new_tokens: Vec<Token<'a>> =
        Tokenizer::with_config(new, config.tokenizer_config.clone()).collect();

    // Optionally filter whitespace
    let (old_filtered, new_filtered): (Vec<Token<'a>>, Vec<Token<'a>>) = if config.filter_whitespace
    {
        (
            old_tokens
                .into_iter()
                .filter(|t| t.is_significant())
                .collect(),
            new_tokens
                .into_iter()
                .filter(|t| t.is_significant())
                .collect(),
        )
    } else {
        (old_tokens, new_tokens)
    };

    // Handle empty cases
    if old_filtered.is_empty() && new_filtered.is_empty() {
        return WordDiffResult::new(old_filtered, new_filtered);
    }

    if old_filtered.is_empty() {
        let len = new_filtered.len();
        let mut result = WordDiffResult::new(old_filtered, new_filtered);
        result.push(WordDiffOp::Insert {
            old_pos: 0,
            new_range: 0..len,
        });
        return result;
    }

    if new_filtered.is_empty() {
        let len = old_filtered.len();
        let mut result = WordDiffResult::new(old_filtered, new_filtered);
        result.push(WordDiffOp::Delete {
            old_range: 0..len,
            new_pos: 0,
        });
        return result;
    }

    // Quick check: if sequences are identical, return early
    if old_filtered.len() == new_filtered.len()
        && old_filtered
            .iter()
            .zip(new_filtered.iter())
            .all(|(a, b)| a == b)
    {
        return WordDiffResult::all_equal(old_filtered, new_filtered);
    }

    // Compute LCS and build diff
    compute_word_diff(old_filtered, new_filtered)
}

/// Compute the word diff using LCS algorithm.
fn compute_word_diff<'a>(
    old_tokens: Vec<Token<'a>>,
    new_tokens: Vec<Token<'a>>,
) -> WordDiffResult<'a> {
    let n = old_tokens.len();
    let m = new_tokens.len();

    // Build LCS table
    let mut dp = vec![vec![0usize; m + 1]; n + 1];

    for i in 1..=n {
        for j in 1..=m {
            if old_tokens[i - 1] == new_tokens[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    // Backtrack to find operations
    let mut ops = Vec::new();
    let mut i = n;
    let mut j = m;

    // We build ops in reverse, then reverse at the end
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old_tokens[i - 1] == new_tokens[j - 1] {
            // Equal
            ops.push(WordDiffOp::Equal {
                old_range: (i - 1)..i,
                new_range: (j - 1)..j,
            });
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            // Insert
            ops.push(WordDiffOp::Insert {
                old_pos: i,
                new_range: (j - 1)..j,
            });
            j -= 1;
        } else {
            // Delete
            ops.push(WordDiffOp::Delete {
                old_range: (i - 1)..i,
                new_pos: j,
            });
            i -= 1;
        }
    }

    ops.reverse();

    // Merge adjacent operations of the same type and convert delete+insert pairs to replace
    let merged_ops = merge_operations(ops);

    WordDiffResult {
        ops: merged_ops,
        old_tokens,
        new_tokens,
    }
}

/// Merge adjacent operations of the same type and convert delete+insert to replace.
fn merge_operations(ops: Vec<WordDiffOp>) -> Vec<WordDiffOp> {
    if ops.is_empty() {
        return ops;
    }

    let mut merged: Vec<WordDiffOp> = Vec::with_capacity(ops.len());

    for op in ops {
        if merged.is_empty() {
            merged.push(op);
            continue;
        }

        let last = merged.last_mut().unwrap();

        // Try to merge with the previous operation
        match (&last, &op) {
            // Merge adjacent Equals
            (
                WordDiffOp::Equal {
                    old_range: old1,
                    new_range: new1,
                },
                WordDiffOp::Equal {
                    old_range: old2,
                    new_range: new2,
                },
            ) if old1.end == old2.start && new1.end == new2.start => {
                *last = WordDiffOp::Equal {
                    old_range: old1.start..old2.end,
                    new_range: new1.start..new2.end,
                };
            }

            // Merge adjacent Inserts
            (
                WordDiffOp::Insert {
                    old_pos: pos1,
                    new_range: range1,
                },
                WordDiffOp::Insert {
                    old_pos: pos2,
                    new_range: range2,
                },
            ) if pos1 == pos2 && range1.end == range2.start => {
                *last = WordDiffOp::Insert {
                    old_pos: *pos1,
                    new_range: range1.start..range2.end,
                };
            }

            // Merge adjacent Deletes
            (
                WordDiffOp::Delete {
                    old_range: range1,
                    new_pos: pos1,
                },
                WordDiffOp::Delete {
                    old_range: range2,
                    new_pos: pos2,
                },
            ) if pos1 == pos2 && range1.end == range2.start => {
                *last = WordDiffOp::Delete {
                    old_range: range1.start..range2.end,
                    new_pos: *pos1,
                };
            }

            // Convert Delete followed by Insert at same position to Replace
            (
                WordDiffOp::Delete {
                    old_range,
                    new_pos: del_new_pos,
                },
                WordDiffOp::Insert {
                    old_pos: ins_old_pos,
                    new_range,
                },
            ) if old_range.end == *ins_old_pos && *del_new_pos == new_range.start => {
                *last = WordDiffOp::Replace {
                    old_range: old_range.clone(),
                    new_range: new_range.clone(),
                };
            }

            // Extend existing Replace with more inserts
            (
                WordDiffOp::Replace {
                    old_range,
                    new_range: rep_new_range,
                },
                WordDiffOp::Insert {
                    old_pos,
                    new_range: ins_new_range,
                },
            ) if old_range.end == *old_pos && rep_new_range.end == ins_new_range.start => {
                *last = WordDiffOp::Replace {
                    old_range: old_range.clone(),
                    new_range: rep_new_range.start..ins_new_range.end,
                };
            }

            // Extend existing Replace with more deletes
            (
                WordDiffOp::Replace {
                    old_range: rep_old_range,
                    new_range,
                },
                WordDiffOp::Delete {
                    old_range: del_old_range,
                    new_pos,
                },
            ) if rep_old_range.end == del_old_range.start && new_range.end == *new_pos => {
                *last = WordDiffOp::Replace {
                    old_range: rep_old_range.start..del_old_range.end,
                    new_range: new_range.clone(),
                };
            }

            // No merge possible
            _ => {
                merged.push(op);
            }
        }
    }

    merged
}

/// Compute word-level diff between two strings.
///
/// Convenience function for string inputs.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::word::word_diff_str;
///
/// let result = word_diff_str("hello world", "hello there");
/// assert!(result.has_changes());
/// ```
pub fn word_diff_str<'a>(old: &'a str, new: &'a str) -> WordDiffResult<'a> {
    word_diff(old.as_bytes(), new.as_bytes())
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // WordDiffOp tests

    #[test]
    fn test_word_diff_op_is_equal() {
        let equal = WordDiffOp::Equal {
            old_range: 0..1,
            new_range: 0..1,
        };
        let insert = WordDiffOp::Insert {
            old_pos: 0,
            new_range: 0..1,
        };

        assert!(equal.is_equal());
        assert!(!equal.is_change());
        assert!(!insert.is_equal());
        assert!(insert.is_change());
    }

    #[test]
    fn test_word_diff_op_is_insert() {
        let op = WordDiffOp::Insert {
            old_pos: 0,
            new_range: 0..1,
        };
        assert!(op.is_insert());
        assert!(!op.is_delete());
        assert!(!op.is_replace());
    }

    #[test]
    fn test_word_diff_op_is_delete() {
        let op = WordDiffOp::Delete {
            old_range: 0..1,
            new_pos: 0,
        };
        assert!(op.is_delete());
        assert!(!op.is_insert());
        assert!(!op.is_replace());
    }

    #[test]
    fn test_word_diff_op_is_replace() {
        let op = WordDiffOp::Replace {
            old_range: 0..1,
            new_range: 0..1,
        };
        assert!(op.is_replace());
        assert!(!op.is_insert());
        assert!(!op.is_delete());
    }

    #[test]
    fn test_word_diff_op_old_range() {
        let equal = WordDiffOp::Equal {
            old_range: 0..5,
            new_range: 0..5,
        };
        let insert = WordDiffOp::Insert {
            old_pos: 0,
            new_range: 0..3,
        };
        let delete = WordDiffOp::Delete {
            old_range: 2..4,
            new_pos: 0,
        };

        assert_eq!(equal.old_range(), Some(0..5));
        assert_eq!(insert.old_range(), None);
        assert_eq!(delete.old_range(), Some(2..4));
    }

    #[test]
    fn test_word_diff_op_new_range() {
        let equal = WordDiffOp::Equal {
            old_range: 0..5,
            new_range: 0..5,
        };
        let insert = WordDiffOp::Insert {
            old_pos: 0,
            new_range: 0..3,
        };
        let delete = WordDiffOp::Delete {
            old_range: 2..4,
            new_pos: 0,
        };

        assert_eq!(equal.new_range(), Some(0..5));
        assert_eq!(insert.new_range(), Some(0..3));
        assert_eq!(delete.new_range(), None);
    }

    #[test]
    fn test_word_diff_op_lengths() {
        let equal = WordDiffOp::Equal {
            old_range: 0..5,
            new_range: 0..5,
        };
        let insert = WordDiffOp::Insert {
            old_pos: 0,
            new_range: 0..3,
        };
        let delete = WordDiffOp::Delete {
            old_range: 2..6,
            new_pos: 0,
        };
        let replace = WordDiffOp::Replace {
            old_range: 0..2,
            new_range: 0..4,
        };

        assert_eq!(equal.old_len(), 5);
        assert_eq!(equal.new_len(), 5);
        assert_eq!(insert.old_len(), 0);
        assert_eq!(insert.new_len(), 3);
        assert_eq!(delete.old_len(), 4);
        assert_eq!(delete.new_len(), 0);
        assert_eq!(replace.old_len(), 2);
        assert_eq!(replace.new_len(), 4);
    }

    #[test]
    fn test_word_diff_op_display() {
        let equal = WordDiffOp::Equal {
            old_range: 0..1,
            new_range: 0..1,
        };
        let display = format!("{}", equal);
        assert!(display.contains("Equal"));
    }

    // WordDiffResult tests

    #[test]
    fn test_word_diff_empty() {
        let result = word_diff(b"", b"");
        assert!(result.is_empty());
        assert!(!result.has_changes());
        assert!(result.is_unchanged());
    }

    #[test]
    fn test_word_diff_identical() {
        let result = word_diff(b"hello world", b"hello world");
        assert!(!result.has_changes());
        assert!(result.is_unchanged());
        assert_eq!(result.insertions(), 0);
        assert_eq!(result.deletions(), 0);
    }

    #[test]
    fn test_word_diff_all_inserted() {
        let result = word_diff(b"", b"hello world");
        assert!(result.has_changes());
        assert!(result.insertions() > 0);
        assert_eq!(result.deletions(), 0);
    }

    #[test]
    fn test_word_diff_all_deleted() {
        let result = word_diff(b"hello world", b"");
        assert!(result.has_changes());
        assert_eq!(result.insertions(), 0);
        assert!(result.deletions() > 0);
    }

    #[test]
    fn test_word_diff_single_word_change() {
        let result = word_diff(b"hello world", b"hello there");
        assert!(result.has_changes());

        // Should have some changes
        let changes: Vec<_> = result.changes().collect();
        assert!(!changes.is_empty());
    }

    #[test]
    fn test_word_diff_insertion_in_middle() {
        let result = word_diff(b"a b", b"a x b");
        assert!(result.has_changes());
        assert!(result.insertions() > 0);
    }

    #[test]
    fn test_word_diff_deletion_in_middle() {
        let result = word_diff(b"a x b", b"a b");
        assert!(result.has_changes());
        assert!(result.deletions() > 0);
    }

    #[test]
    fn test_word_diff_replacement() {
        let result = word_diff(b"const x = 1;", b"const x = 2;");
        assert!(result.has_changes());

        // The number 1 should be replaced with 2
        let has_replace_or_del_ins = result.ops().iter().any(|op| {
            op.is_replace() || op.is_delete() || op.is_insert()
        });
        assert!(has_replace_or_del_ins);
    }

    #[test]
    fn test_word_diff_edit_distance() {
        let result = word_diff(b"a b c", b"a x c");
        assert!(result.edit_distance() > 0);
    }

    #[test]
    fn test_word_diff_tokens_in_range() {
        let result = word_diff(b"hello world", b"hello world");

        // Get first token from old
        let old_tokens = result.old_tokens_in_range(&(0..1));
        assert_eq!(old_tokens.len(), 1);
        assert_eq!(old_tokens[0].as_str(), "hello");
    }

    #[test]
    fn test_word_diff_content_in_range() {
        let result = word_diff(b"hello world", b"hello world");

        let content = result.old_content_in_range(&(0..1));
        assert_eq!(content, "hello");
    }

    // WordDiffConfig tests

    #[test]
    fn test_word_diff_config_default() {
        let config = WordDiffConfig::default();
        assert!(!config.ignore_whitespace_changes);
        assert!(!config.filter_whitespace);
    }

    #[test]
    fn test_word_diff_config_minimal() {
        let config = WordDiffConfig::minimal();
        assert!(!config.ignore_whitespace_changes);
    }

    #[test]
    fn test_word_diff_config_ignore_whitespace() {
        let config = WordDiffConfig::ignoring_whitespace();
        assert!(config.ignore_whitespace_changes);
        assert!(config.filter_whitespace);
    }

    #[test]
    fn test_word_diff_config_builder() {
        let config = WordDiffConfig::new()
            .with_ignore_whitespace(true)
            .with_tokenizer(TokenizerConfig::minimal());

        assert!(config.filter_whitespace);
    }

    #[test]
    fn test_word_diff_with_config_ignore_whitespace() {
        let config = WordDiffConfig::ignoring_whitespace();
        let result = word_diff_with_config(b"a b", b"a  b", &config);

        // With whitespace filtered, only non-whitespace tokens remain
        // Both should have same significant tokens
        assert!(!result.has_changes());
    }

    // Real-world code tests

    #[test]
    fn test_word_diff_function_arg_added() {
        let old = b"calculateSum(a, b)";
        let new = b"calculateSum(a, b, c)";

        let result = word_diff(old, new);
        assert!(result.has_changes());
        assert!(result.insertions() > 0);
    }

    #[test]
    fn test_word_diff_operator_change() {
        let old = b"x == y";
        let new = b"x != y";

        let result = word_diff(old, new);
        assert!(result.has_changes());
    }

    #[test]
    fn test_word_diff_string_literal_change() {
        let old = b"let s = \"hello\";";
        let new = b"let s = \"world\";";

        let result = word_diff(old, new);
        assert!(result.has_changes());
    }

    #[test]
    fn test_word_diff_number_change() {
        let old = b"timeout = 5000";
        let new = b"timeout = 10000";

        let result = word_diff(old, new);
        assert!(result.has_changes());
    }

    #[test]
    fn test_word_diff_variable_rename() {
        let old = b"let foo = 1;";
        let new = b"let bar = 1;";

        let result = word_diff(old, new);
        assert!(result.has_changes());
    }

    #[test]
    fn test_word_diff_type_annotation_added() {
        let old = b"let x = 1";
        let new = b"let x: i32 = 1";

        let result = word_diff(old, new);
        assert!(result.has_changes());
        assert!(result.insertions() > 0);
    }

    #[test]
    fn test_word_diff_preserves_token_order() {
        let result = word_diff(b"a b c", b"a b c");

        let tokens = result.old_tokens();
        assert_eq!(tokens.len(), 5); // a, space, b, space, c
        assert_eq!(tokens[0].as_str(), "a");
        assert_eq!(tokens[2].as_str(), "b");
        assert_eq!(tokens[4].as_str(), "c");
    }

    // word_diff_str tests

    #[test]
    fn test_word_diff_str() {
        let result = word_diff_str("hello world", "hello there");
        assert!(result.has_changes());
    }

    #[test]
    fn test_word_diff_str_identical() {
        let result = word_diff_str("same text", "same text");
        assert!(!result.has_changes());
    }

    // Edge cases

    #[test]
    fn test_word_diff_single_char() {
        let result = word_diff(b"a", b"b");
        assert!(result.has_changes());
    }

    #[test]
    fn test_word_diff_whitespace_only() {
        let result = word_diff(b"   ", b"\t\t");
        // Both are whitespace, but different whitespace
        assert!(result.has_changes());
    }

    #[test]
    fn test_word_diff_newline_handling() {
        let result = word_diff(b"a\nb", b"a\nc");
        assert!(result.has_changes());
    }

    #[test]
    fn test_word_diff_complex_code() {
        let old = b"pub fn process(items: Vec<Item>) -> Result<(), Error> {";
        let new = b"pub fn process(items: &[Item]) -> Result<(), Error> {";

        let result = word_diff(old, new);
        assert!(result.has_changes());

        // Should detect the Vec<Item> -> &[Item] change
        let changes: Vec<_> = result.changes().collect();
        assert!(!changes.is_empty());
    }

    #[test]
    fn test_word_diff_many_changes() {
        let old = b"a b c d e";
        let new = b"w x y z";

        let result = word_diff(old, new);
        assert!(result.has_changes());
        assert!(result.edit_distance() > 0);
    }

    #[test]
    fn test_merge_adjacent_equals() {
        let result = word_diff(b"a b c", b"a b c");

        // Should be one Equal operation, not three separate ones
        let equals: Vec<_> = result.ops().iter()
            .filter(|op| op.is_equal())
            .collect();

        // After merging, equal ranges should be combined
        assert!(!equals.is_empty());
    }

    #[test]
    fn test_merge_delete_insert_to_replace() {
        let result = word_diff(b"old", b"new");

        // A single word change should ideally be a Replace
        // But the exact operation depends on merging
        assert!(result.has_changes());
    }
}

//! Semantic diff with token-level granularity.
//!
//! This module provides the core functionality for diffing files at multiple
//! levels of granularity:
//!
//! 1. **Line-level**: Which lines changed (added, deleted, modified)
//! 2. **Token-level**: Within modified lines, which tokens changed
//!
//! This two-level approach is essential for code reviews where you want to
//! see exactly what changed - not just that a line changed, but specifically
//! which words/tokens within that line were modified.
//!
//! # Visual Pattern
//!
//! The semantic diff enables the modern code review display pattern:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │ - const result = calculateSum(a, b);        <- light red background      │
//! │ + const result = calculateSum(a, b, c);     <- light green background    │
//! │                                   ^^^^      <- dark green: ", c" added   │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_core::diff::semantic::{semantic_diff, LineChange};
//!
//! let old = b"fn main() {\n    let x = 1;\n}\n";
//! let new = b"fn main() {\n    let x = 42;\n}\n";
//!
//! let diff = semantic_diff(old, new);
//!
//! assert!(diff.has_changes());
//! for change in diff.changes() {
//!     match change {
//!         LineChange::Modified { old_line_num, new_line_num, before, after, token_changes } => {
//!             println!("Line {} -> {} modified:", old_line_num, new_line_num);
//!             for tc in token_changes {
//!                 println!("  {:?}", tc);
//!             }
//!         }
//!         LineChange::Added { line_num, line, .. } => {
//!             println!("Line {} added: {:?}", line_num, line.content_str());
//!         }
//!         LineChange::Deleted { line_num, line, .. } => {
//!             println!("Line {} deleted: {:?}", line_num, line.content_str());
//!         }
//!     }
//! }
//! ```
//!
//! # Integration with CRDT
//!
//! The semantic diff results can be used to generate CRDT operations:
//!
//! - `LineChange::Added` → `BranchOp::Insert` with `LeafOp::Insert` for tokens
//! - `LineChange::Deleted` → `BranchOp::Delete` with original content
//! - `LineChange::Modified` → Combination of token-level operations

use super::line::Line;
use super::ops::DiffOp;
use super::token::{Token, TokenKind, Tokenizer, TokenizerConfig};
use super::word::{word_diff_with_config, WordDiffConfig, WordDiffOp, WordDiffResult};
use super::{diff, Algorithm};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;

// =============================================================================
// SemanticLine - A line with pre-tokenized content
// =============================================================================

/// A line with pre-computed tokens for efficient diffing.
///
/// This struct holds both the raw line content and its tokenized form,
/// avoiding repeated tokenization during diff operations.
#[derive(Clone)]
pub struct SemanticLine<'a> {
    /// The raw line content.
    line: Line<'a>,

    /// Pre-computed tokens for this line.
    tokens: Vec<Token<'a>>,

    /// The original line number (1-indexed, for display).
    line_num: usize,
}

impl<'a> SemanticLine<'a> {
    /// Create a new semantic line from raw content.
    ///
    /// Tokenizes the content immediately for later use.
    pub fn new(content: &'a [u8], line_num: usize) -> Self {
        let line = Line::new(content);
        let tokens = Tokenizer::new(content).collect();
        Self {
            line,
            tokens,
            line_num,
        }
    }

    /// Create a semantic line from an existing Line.
    pub fn from_line(line: Line<'a>, line_num: usize) -> Self {
        let tokens = Tokenizer::new(line.content()).collect();
        Self {
            line,
            tokens,
            line_num,
        }
    }

    /// Create semantic lines from byte content.
    pub fn from_bytes(content: &'a [u8]) -> Vec<Self> {
        Line::from_bytes(content)
            .into_iter()
            .enumerate()
            .map(|(i, line)| Self::from_line(line, i + 1))
            .collect()
    }

    /// Get the underlying Line.
    #[inline]
    pub fn line(&self) -> &Line<'a> {
        &self.line
    }

    /// Get the raw content of this line.
    #[inline]
    pub fn content(&self) -> &'a [u8] {
        self.line.content()
    }

    /// Get the content as a string (lossy conversion for non-UTF8).
    pub fn content_str(&self) -> std::borrow::Cow<'a, str> {
        String::from_utf8_lossy(self.content())
    }

    /// Get the content without trailing newline.
    #[inline]
    pub fn content_without_newline(&self) -> &'a [u8] {
        self.line.content_without_newline()
    }

    /// Get the pre-computed tokens.
    #[inline]
    pub fn tokens(&self) -> &[Token<'a>] {
        &self.tokens
    }

    /// Get the line number (1-indexed).
    #[inline]
    pub fn line_num(&self) -> usize {
        self.line_num
    }

    /// Check if the line is empty (or only whitespace).
    pub fn is_blank(&self) -> bool {
        self.tokens
            .iter()
            .all(|t| t.kind() == TokenKind::Whitespace || t.kind() == TokenKind::Newline)
    }

    /// Get the number of significant (non-whitespace) tokens.
    pub fn significant_token_count(&self) -> usize {
        self.tokens
            .iter()
            .filter(|t| t.kind().is_significant())
            .count()
    }
}

impl<'a> fmt::Debug for SemanticLine<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemanticLine")
            .field("line_num", &self.line_num)
            .field("content", &self.content_str())
            .field("token_count", &self.tokens.len())
            .finish()
    }
}

impl<'a> PartialEq for SemanticLine<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.line == other.line
    }
}

impl<'a> Eq for SemanticLine<'a> {}

// =============================================================================
// TokenChange - Changes within a single token
// =============================================================================

/// A change to a single token within a line.
///
/// This represents the finest granularity of change tracking - what
/// happened to a specific token (word, operator, etc.) within a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenChange<'a> {
    /// Token is unchanged (context).
    Unchanged {
        /// The token content.
        token: Token<'a>,
        /// Byte range in the old line.
        old_range: Range<usize>,
        /// Byte range in the new line.
        new_range: Range<usize>,
    },

    /// Token was inserted (only in new line).
    Inserted {
        /// The inserted token.
        token: Token<'a>,
        /// Byte range in the new line.
        new_range: Range<usize>,
    },

    /// Token was deleted (only in old line).
    Deleted {
        /// The deleted token.
        token: Token<'a>,
        /// Byte range in the old line.
        old_range: Range<usize>,
    },

    /// Token was replaced (different in old and new).
    Replaced {
        /// The old token that was replaced.
        old_token: Token<'a>,
        /// The new token that replaced it.
        new_token: Token<'a>,
        /// Byte range in the old line.
        old_range: Range<usize>,
        /// Byte range in the new line.
        new_range: Range<usize>,
    },
}

impl<'a> TokenChange<'a> {
    /// Check if this is an unchanged token.
    #[inline]
    pub fn is_unchanged(&self) -> bool {
        matches!(self, TokenChange::Unchanged { .. })
    }

    /// Check if this represents a change (insert, delete, or replace).
    #[inline]
    pub fn is_change(&self) -> bool {
        !self.is_unchanged()
    }

    /// Check if this is an insertion.
    #[inline]
    pub fn is_inserted(&self) -> bool {
        matches!(self, TokenChange::Inserted { .. })
    }

    /// Check if this is a deletion.
    #[inline]
    pub fn is_deleted(&self) -> bool {
        matches!(self, TokenChange::Deleted { .. })
    }

    /// Check if this is a replacement.
    #[inline]
    pub fn is_replaced(&self) -> bool {
        matches!(self, TokenChange::Replaced { .. })
    }

    /// Get the old token if present.
    pub fn old_token(&self) -> Option<&Token<'a>> {
        match self {
            TokenChange::Unchanged { token, .. } => Some(token),
            TokenChange::Deleted { token, .. } => Some(token),
            TokenChange::Replaced { old_token, .. } => Some(old_token),
            TokenChange::Inserted { .. } => None,
        }
    }

    /// Get the new token if present.
    pub fn new_token(&self) -> Option<&Token<'a>> {
        match self {
            TokenChange::Unchanged { token, .. } => Some(token),
            TokenChange::Inserted { token, .. } => Some(token),
            TokenChange::Replaced { new_token, .. } => Some(new_token),
            TokenChange::Deleted { .. } => None,
        }
    }

    /// Get the byte range in the old line (if applicable).
    pub fn old_range(&self) -> Option<Range<usize>> {
        match self {
            TokenChange::Unchanged { old_range, .. } => Some(old_range.clone()),
            TokenChange::Deleted { old_range, .. } => Some(old_range.clone()),
            TokenChange::Replaced { old_range, .. } => Some(old_range.clone()),
            TokenChange::Inserted { .. } => None,
        }
    }

    /// Get the byte range in the new line (if applicable).
    pub fn new_range(&self) -> Option<Range<usize>> {
        match self {
            TokenChange::Unchanged { new_range, .. } => Some(new_range.clone()),
            TokenChange::Inserted { new_range, .. } => Some(new_range.clone()),
            TokenChange::Replaced { new_range, .. } => Some(new_range.clone()),
            TokenChange::Deleted { .. } => None,
        }
    }

    /// Get a display-friendly description of this change.
    pub fn description(&self) -> String {
        match self {
            TokenChange::Unchanged { token, .. } => {
                format!("unchanged: {:?}", token.as_str())
            }
            TokenChange::Inserted { token, .. } => {
                format!("inserted: {:?}", token.as_str())
            }
            TokenChange::Deleted { token, .. } => {
                format!("deleted: {:?}", token.as_str())
            }
            TokenChange::Replaced {
                old_token,
                new_token,
                ..
            } => {
                format!(
                    "replaced: {:?} → {:?}",
                    old_token.as_str(),
                    new_token.as_str()
                )
            }
        }
    }
}

impl<'a> fmt::Display for TokenChange<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

// =============================================================================
// LineChange - Changes to a line
// =============================================================================

/// A change to a line in the diff.
///
/// This is the primary output type for semantic diffs. Each variant
/// describes what happened to a line and, for modified lines, includes
/// token-level detail about what changed within the line.
#[derive(Debug, Clone)]
pub enum LineChange<'a> {
    /// A line was added (only in new version).
    Added {
        /// Line number in the new version (1-indexed).
        line_num: usize,
        /// The added line with its tokens.
        line: SemanticLine<'a>,
        /// Token operations for this line (all insertions).
        tokens: Vec<TokenChange<'a>>,
    },

    /// A line was deleted (only in old version).
    Deleted {
        /// Line number in the old version (1-indexed).
        line_num: usize,
        /// The deleted line with its tokens.
        line: SemanticLine<'a>,
        /// Token operations for this line (all deletions).
        tokens: Vec<TokenChange<'a>>,
    },

    /// A line was modified (exists in both but different).
    Modified {
        /// Line number in the old version (1-indexed).
        old_line_num: usize,
        /// Line number in the new version (1-indexed).
        new_line_num: usize,
        /// The old version of the line.
        before: SemanticLine<'a>,
        /// The new version of the line.
        after: SemanticLine<'a>,
        /// Token-level changes within the line.
        token_changes: Vec<TokenChange<'a>>,
    },
}

impl<'a> LineChange<'a> {
    /// Check if this is an added line.
    #[inline]
    pub fn is_added(&self) -> bool {
        matches!(self, LineChange::Added { .. })
    }

    /// Check if this is a deleted line.
    #[inline]
    pub fn is_deleted(&self) -> bool {
        matches!(self, LineChange::Deleted { .. })
    }

    /// Check if this is a modified line.
    #[inline]
    pub fn is_modified(&self) -> bool {
        matches!(self, LineChange::Modified { .. })
    }

    /// Get the old line number (if applicable).
    pub fn old_line_num(&self) -> Option<usize> {
        match self {
            LineChange::Deleted { line_num, .. } => Some(*line_num),
            LineChange::Modified { old_line_num, .. } => Some(*old_line_num),
            LineChange::Added { .. } => None,
        }
    }

    /// Get the new line number (if applicable).
    pub fn new_line_num(&self) -> Option<usize> {
        match self {
            LineChange::Added { line_num, .. } => Some(*line_num),
            LineChange::Modified { new_line_num, .. } => Some(*new_line_num),
            LineChange::Deleted { .. } => None,
        }
    }

    /// Get the token changes for this line change.
    pub fn token_changes(&self) -> &[TokenChange<'a>] {
        match self {
            LineChange::Added { tokens, .. } => tokens,
            LineChange::Deleted { tokens, .. } => tokens,
            LineChange::Modified { token_changes, .. } => token_changes,
        }
    }

    /// Count the number of token insertions.
    pub fn token_insertions(&self) -> usize {
        self.token_changes()
            .iter()
            .filter(|tc| tc.is_inserted() || tc.is_replaced())
            .count()
    }

    /// Count the number of token deletions.
    pub fn token_deletions(&self) -> usize {
        self.token_changes()
            .iter()
            .filter(|tc| tc.is_deleted() || tc.is_replaced())
            .count()
    }

    /// Get a summary of what changed.
    pub fn summary(&self) -> String {
        match self {
            LineChange::Added { line_num, line, .. } => {
                format!(
                    "+{}: {} ({} tokens)",
                    line_num,
                    line.content_str().trim_end(),
                    line.tokens().len()
                )
            }
            LineChange::Deleted { line_num, line, .. } => {
                format!(
                    "-{}: {} ({} tokens)",
                    line_num,
                    line.content_str().trim_end(),
                    line.tokens().len()
                )
            }
            LineChange::Modified {
                old_line_num,
                new_line_num,
                token_changes,
                ..
            } => {
                let changes = token_changes.iter().filter(|tc| tc.is_change()).count();
                format!(
                    "~{}→{}: {} token changes",
                    old_line_num, new_line_num, changes
                )
            }
        }
    }
}

impl<'a> fmt::Display for LineChange<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary())
    }
}

// =============================================================================
// SemanticDiff - The complete diff result
// =============================================================================

/// Configuration for semantic diff operations.
#[derive(Debug, Clone)]
pub struct SemanticDiffConfig {
    /// The line-level diff algorithm to use.
    pub algorithm: Algorithm,

    /// Configuration for token-level diffing.
    pub token_config: TokenizerConfig,

    /// Configuration for word-level diffing within lines.
    pub word_config: WordDiffConfig,

    /// Whether to include context lines (unchanged lines) in the output.
    pub include_context: bool,

    /// Number of context lines to include around changes.
    pub context_lines: usize,

    /// Whether to ignore whitespace-only changes.
    pub ignore_whitespace: bool,

    /// Whether to treat blank line changes as significant.
    pub ignore_blank_lines: bool,
}

impl Default for SemanticDiffConfig {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Myers,
            token_config: TokenizerConfig::default(),
            word_config: WordDiffConfig::default(),
            include_context: false,
            context_lines: 3,
            ignore_whitespace: false,
            ignore_blank_lines: false,
        }
    }
}

impl SemanticDiffConfig {
    /// Create a new configuration with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the diff algorithm.
    pub fn algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Include context lines in output.
    pub fn with_context(mut self, lines: usize) -> Self {
        self.include_context = true;
        self.context_lines = lines;
        self
    }

    /// Ignore whitespace-only changes.
    pub fn ignore_whitespace(mut self) -> Self {
        self.ignore_whitespace = true;
        self
    }

    /// Ignore blank line changes.
    pub fn ignore_blank_lines(mut self) -> Self {
        self.ignore_blank_lines = true;
        self
    }
}

/// Statistics about a semantic diff.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticDiffStats {
    /// Number of lines added.
    pub lines_added: usize,

    /// Number of lines deleted.
    pub lines_deleted: usize,

    /// Number of lines modified.
    pub lines_modified: usize,

    /// Total number of tokens inserted.
    pub tokens_inserted: usize,

    /// Total number of tokens deleted.
    pub tokens_deleted: usize,

    /// Total number of tokens replaced.
    pub tokens_replaced: usize,
}

impl SemanticDiffStats {
    /// Check if there are any changes.
    pub fn has_changes(&self) -> bool {
        self.lines_added > 0
            || self.lines_deleted > 0
            || self.lines_modified > 0
            || self.tokens_inserted > 0
            || self.tokens_deleted > 0
            || self.tokens_replaced > 0
    }

    /// Get the total number of line changes.
    pub fn total_line_changes(&self) -> usize {
        self.lines_added + self.lines_deleted + self.lines_modified
    }

    /// Get the total number of token changes.
    pub fn total_token_changes(&self) -> usize {
        self.tokens_inserted + self.tokens_deleted + self.tokens_replaced
    }
}

impl fmt::Display for SemanticDiffStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} insertions(+), {} deletions(-), {} modifications(~), {} token changes",
            self.lines_added,
            self.lines_deleted,
            self.lines_modified,
            self.total_token_changes()
        )
    }
}

/// The result of a semantic diff operation.
///
/// Contains line-level changes with token-level detail for modified lines.
#[derive(Debug, Clone)]
pub struct SemanticDiff<'a> {
    /// The line changes in order.
    changes: Vec<LineChange<'a>>,

    /// Statistics about the diff.
    stats: SemanticDiffStats,

    /// The old lines (for reference).
    old_lines: Vec<SemanticLine<'a>>,

    /// The new lines (for reference).
    new_lines: Vec<SemanticLine<'a>>,
}

impl<'a> SemanticDiff<'a> {
    /// Create an empty semantic diff (no changes).
    pub fn empty() -> Self {
        Self {
            changes: Vec::new(),
            stats: SemanticDiffStats::default(),
            old_lines: Vec::new(),
            new_lines: Vec::new(),
        }
    }

    /// Get the line changes.
    #[inline]
    pub fn changes(&self) -> &[LineChange<'a>] {
        &self.changes
    }

    /// Get the diff statistics.
    #[inline]
    pub fn stats(&self) -> &SemanticDiffStats {
        &self.stats
    }

    /// Get the old lines.
    #[inline]
    pub fn old_lines(&self) -> &[SemanticLine<'a>] {
        &self.old_lines
    }

    /// Get the new lines.
    #[inline]
    pub fn new_lines(&self) -> &[SemanticLine<'a>] {
        &self.new_lines
    }

    /// Check if there are any changes.
    #[inline]
    pub fn has_changes(&self) -> bool {
        !self.changes.is_empty()
    }

    /// Check if the content is unchanged.
    #[inline]
    pub fn is_unchanged(&self) -> bool {
        self.changes.is_empty()
    }

    /// Get the number of line changes.
    #[inline]
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Check if there are no changes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Iterate over added lines.
    pub fn added_lines(&self) -> impl Iterator<Item = &LineChange<'a>> {
        self.changes.iter().filter(|c| c.is_added())
    }

    /// Iterate over deleted lines.
    pub fn deleted_lines(&self) -> impl Iterator<Item = &LineChange<'a>> {
        self.changes.iter().filter(|c| c.is_deleted())
    }

    /// Iterate over modified lines.
    pub fn modified_lines(&self) -> impl Iterator<Item = &LineChange<'a>> {
        self.changes.iter().filter(|c| c.is_modified())
    }

    /// Get all token changes across all line changes.
    pub fn all_token_changes(&self) -> impl Iterator<Item = &TokenChange<'a>> {
        self.changes.iter().flat_map(|c| c.token_changes())
    }
}

impl<'a> fmt::Display for SemanticDiff<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_unchanged() {
            return write!(f, "No changes");
        }

        writeln!(f, "{}", self.stats)?;
        for change in &self.changes {
            writeln!(f, "  {}", change)?;
        }
        Ok(())
    }
}

// =============================================================================
// Core diffing functions
// =============================================================================

/// Compute a semantic diff between two byte slices.
///
/// This is the main entry point for semantic diffing. It:
/// 1. Splits content into lines
/// 2. Computes line-level diff
/// 3. For modified lines, computes token-level diff
///
/// # Arguments
///
/// * `old` - The original content
/// * `new` - The modified content
///
/// # Returns
///
/// A [`SemanticDiff`] with line and token level changes.
///
/// # Example
///
/// ```rust
/// use atomic_core::diff::semantic::semantic_diff;
///
/// let old = b"let x = 1;\n";
/// let new = b"let x = 42;\n";
///
/// let diff = semantic_diff(old, new);
/// assert!(diff.has_changes());
/// ```
pub fn semantic_diff<'a>(old: &'a [u8], new: &'a [u8]) -> SemanticDiff<'a> {
    semantic_diff_with_config(old, new, &SemanticDiffConfig::default())
}

/// Compute a semantic diff with custom configuration.
///
/// # Arguments
///
/// * `old` - The original content
/// * `new` - The modified content
/// * `config` - Configuration for the diff operation
///
/// # Returns
///
/// A [`SemanticDiff`] with line and token level changes.
pub fn semantic_diff_with_config<'a>(
    old: &'a [u8],
    new: &'a [u8],
    config: &SemanticDiffConfig,
) -> SemanticDiff<'a> {
    // Parse into semantic lines
    let old_lines = SemanticLine::from_bytes(old);
    let new_lines = SemanticLine::from_bytes(new);

    // Compute line-level diff
    let old_raw: Vec<Line> = old_lines.iter().map(|sl| sl.line.clone()).collect();
    let new_raw: Vec<Line> = new_lines.iter().map(|sl| sl.line.clone()).collect();
    let line_diff = diff(&old_raw, &new_raw, config.algorithm);

    // Process the diff operations into semantic changes
    let mut changes = Vec::new();
    let mut stats = SemanticDiffStats::default();

    for op in line_diff.iter() {
        match op {
            DiffOp::Equal { .. } => {
                // Skip unchanged lines (unless including context)
            }

            DiffOp::Insert {
                old_pos: _,
                new_pos,
                len,
            } => {
                // Lines were added
                for i in 0..*len {
                    let line_idx = new_pos + i;
                    if line_idx < new_lines.len() {
                        let line = new_lines[line_idx].clone();

                        // Skip blank lines if configured
                        if config.ignore_blank_lines && line.is_blank() {
                            continue;
                        }

                        // All tokens are insertions
                        let tokens = create_insertion_tokens(&line);
                        stats.tokens_inserted += tokens.len();

                        changes.push(LineChange::Added {
                            line_num: line_idx + 1,
                            line,
                            tokens,
                        });
                        stats.lines_added += 1;
                    }
                }
            }

            DiffOp::Delete {
                old_pos,
                new_pos: _,
                len,
            } => {
                // Lines were deleted
                for i in 0..*len {
                    let line_idx = old_pos + i;
                    if line_idx < old_lines.len() {
                        let line = old_lines[line_idx].clone();

                        // Skip blank lines if configured
                        if config.ignore_blank_lines && line.is_blank() {
                            continue;
                        }

                        // All tokens are deletions
                        let tokens = create_deletion_tokens(&line);
                        stats.tokens_deleted += tokens.len();

                        changes.push(LineChange::Deleted {
                            line_num: line_idx + 1,
                            line,
                            tokens,
                        });
                        stats.lines_deleted += 1;
                    }
                }
            }

            DiffOp::Replace {
                old_pos,
                old_len,
                new_pos,
                new_len,
            } => {
                // Lines were modified - this is where token-level diff shines
                let min_len = (*old_len).min(*new_len);

                // Process paired lines (modified)
                for i in 0..min_len {
                    let old_idx = old_pos + i;
                    let new_idx = new_pos + i;

                    if old_idx < old_lines.len() && new_idx < new_lines.len() {
                        let before = old_lines[old_idx].clone();
                        let after = new_lines[new_idx].clone();

                        // Skip if both are blank and configured to ignore
                        if config.ignore_blank_lines && before.is_blank() && after.is_blank() {
                            continue;
                        }

                        // Compute token-level diff for this line pair
                        let token_changes =
                            compute_token_changes(&before, &after, &config.word_config);

                        // Update stats
                        for tc in &token_changes {
                            match tc {
                                TokenChange::Inserted { .. } => stats.tokens_inserted += 1,
                                TokenChange::Deleted { .. } => stats.tokens_deleted += 1,
                                TokenChange::Replaced { .. } => stats.tokens_replaced += 1,
                                TokenChange::Unchanged { .. } => {}
                            }
                        }

                        changes.push(LineChange::Modified {
                            old_line_num: old_idx + 1,
                            new_line_num: new_idx + 1,
                            before,
                            after,
                            token_changes,
                        });
                        stats.lines_modified += 1;
                    }
                }

                // Process extra deleted lines (old_len > new_len)
                for i in min_len..*old_len {
                    let line_idx = old_pos + i;
                    if line_idx < old_lines.len() {
                        let line = old_lines[line_idx].clone();

                        if config.ignore_blank_lines && line.is_blank() {
                            continue;
                        }

                        let tokens = create_deletion_tokens(&line);
                        stats.tokens_deleted += tokens.len();

                        changes.push(LineChange::Deleted {
                            line_num: line_idx + 1,
                            line,
                            tokens,
                        });
                        stats.lines_deleted += 1;
                    }
                }

                // Process extra inserted lines (new_len > old_len)
                for i in min_len..*new_len {
                    let line_idx = new_pos + i;
                    if line_idx < new_lines.len() {
                        let line = new_lines[line_idx].clone();

                        if config.ignore_blank_lines && line.is_blank() {
                            continue;
                        }

                        let tokens = create_insertion_tokens(&line);
                        stats.tokens_inserted += tokens.len();

                        changes.push(LineChange::Added {
                            line_num: line_idx + 1,
                            line,
                            tokens,
                        });
                        stats.lines_added += 1;
                    }
                }
            }
        }
    }

    SemanticDiff {
        changes,
        stats,
        old_lines,
        new_lines,
    }
}

// =============================================================================
// Helper functions for token change creation
// =============================================================================

/// Create token changes for a line that was entirely added.
fn create_insertion_tokens<'a>(line: &SemanticLine<'a>) -> Vec<TokenChange<'a>> {
    let mut offset = 0;
    line.tokens()
        .iter()
        .map(|token| {
            let start = offset;
            let end = start + token.content().len();
            offset = end;
            TokenChange::Inserted {
                token: token.clone(),
                new_range: start..end,
            }
        })
        .collect()
}

/// Create token changes for a line that was entirely deleted.
fn create_deletion_tokens<'a>(line: &SemanticLine<'a>) -> Vec<TokenChange<'a>> {
    let mut offset = 0;
    line.tokens()
        .iter()
        .map(|token| {
            let start = offset;
            let end = start + token.content().len();
            offset = end;
            TokenChange::Deleted {
                token: token.clone(),
                old_range: start..end,
            }
        })
        .collect()
}

/// Compute token-level changes between two lines.
///
/// This is the core of the token-level diff - it compares the tokens
/// of two lines and produces a sequence of token changes.
fn compute_token_changes<'a>(
    before: &SemanticLine<'a>,
    after: &SemanticLine<'a>,
    config: &WordDiffConfig,
) -> Vec<TokenChange<'a>> {
    // Use word diff to find token-level changes
    let word_result = word_diff_with_config(
        before.content_without_newline(),
        after.content_without_newline(),
        config,
    );

    convert_word_diff_to_token_changes(&word_result, before, after)
}

/// Convert word diff operations to token changes.
fn convert_word_diff_to_token_changes<'a>(
    word_result: &WordDiffResult<'a>,
    _before: &SemanticLine<'a>,
    _after: &SemanticLine<'a>,
) -> Vec<TokenChange<'a>> {
    let mut changes = Vec::new();

    let old_tokens = word_result.old_tokens();
    let new_tokens = word_result.new_tokens();

    for op in word_result.ops() {
        match op {
            WordDiffOp::Equal {
                old_range,
                new_range,
            } => {
                // Tokens that are unchanged
                for (old_idx, new_idx) in old_range.clone().zip(new_range.clone()) {
                    if old_idx < old_tokens.len() && new_idx < new_tokens.len() {
                        let token = old_tokens[old_idx].clone();
                        let old_byte_range = token_byte_range(&old_tokens, old_idx);
                        let new_byte_range = token_byte_range(&new_tokens, new_idx);

                        changes.push(TokenChange::Unchanged {
                            token,
                            old_range: old_byte_range,
                            new_range: new_byte_range,
                        });
                    }
                }
            }

            WordDiffOp::Insert {
                old_pos: _,
                new_range,
            } => {
                // Tokens that were inserted
                for new_idx in new_range.clone() {
                    if new_idx < new_tokens.len() {
                        let token = new_tokens[new_idx].clone();
                        let new_byte_range = token_byte_range(&new_tokens, new_idx);

                        changes.push(TokenChange::Inserted {
                            token,
                            new_range: new_byte_range,
                        });
                    }
                }
            }

            WordDiffOp::Delete {
                old_range,
                new_pos: _,
            } => {
                // Tokens that were deleted
                for old_idx in old_range.clone() {
                    if old_idx < old_tokens.len() {
                        let token = old_tokens[old_idx].clone();
                        let old_byte_range = token_byte_range(&old_tokens, old_idx);

                        changes.push(TokenChange::Deleted {
                            token,
                            old_range: old_byte_range,
                        });
                    }
                }
            }

            WordDiffOp::Replace {
                old_range,
                new_range,
            } => {
                // Tokens that were replaced
                // If the counts match, pair them up as replacements
                // Otherwise, treat as deletes followed by inserts
                let old_count = old_range.len();
                let new_count = new_range.len();

                if old_count == new_count {
                    // One-to-one replacement
                    for (old_idx, new_idx) in old_range.clone().zip(new_range.clone()) {
                        if old_idx < old_tokens.len() && new_idx < new_tokens.len() {
                            let old_token = old_tokens[old_idx].clone();
                            let new_token = new_tokens[new_idx].clone();
                            let old_byte_range = token_byte_range(&old_tokens, old_idx);
                            let new_byte_range = token_byte_range(&new_tokens, new_idx);

                            changes.push(TokenChange::Replaced {
                                old_token,
                                new_token,
                                old_range: old_byte_range,
                                new_range: new_byte_range,
                            });
                        }
                    }
                } else {
                    // Different counts - emit deletes then inserts
                    for old_idx in old_range.clone() {
                        if old_idx < old_tokens.len() {
                            let token = old_tokens[old_idx].clone();
                            let old_byte_range = token_byte_range(&old_tokens, old_idx);

                            changes.push(TokenChange::Deleted {
                                token,
                                old_range: old_byte_range,
                            });
                        }
                    }

                    for new_idx in new_range.clone() {
                        if new_idx < new_tokens.len() {
                            let token = new_tokens[new_idx].clone();
                            let new_byte_range = token_byte_range(&new_tokens, new_idx);

                            changes.push(TokenChange::Inserted {
                                token,
                                new_range: new_byte_range,
                            });
                        }
                    }
                }
            }
        }
    }

    changes
}

/// Calculate the byte range for a token at a given index.
fn token_byte_range(tokens: &[Token<'_>], index: usize) -> Range<usize> {
    let mut offset = 0;
    for (i, token) in tokens.iter().enumerate() {
        let len = token.content().len();
        if i == index {
            return offset..(offset + len);
        }
        offset += len;
    }
    // Fallback (shouldn't happen with valid indices)
    offset..offset
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // SemanticLine tests
    // =========================================================================

    #[test]
    fn test_semantic_line_new() {
        let line = SemanticLine::new(b"let x = 42;\n", 1);
        assert_eq!(line.line_num(), 1);
        assert!(!line.tokens().is_empty());
    }

    #[test]
    fn test_semantic_line_from_bytes() {
        let content = b"line1\nline2\nline3\n";
        let lines = SemanticLine::from_bytes(content);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].line_num(), 1);
        assert_eq!(lines[1].line_num(), 2);
        assert_eq!(lines[2].line_num(), 3);
    }

    #[test]
    fn test_semantic_line_content_str() {
        let line = SemanticLine::new(b"hello world\n", 1);
        assert_eq!(line.content_str(), "hello world\n");
    }

    #[test]
    fn test_semantic_line_is_blank() {
        let blank = SemanticLine::new(b"   \n", 1);
        assert!(blank.is_blank());

        let not_blank = SemanticLine::new(b"hello\n", 1);
        assert!(!not_blank.is_blank());
    }

    #[test]
    fn test_semantic_line_significant_token_count() {
        let line = SemanticLine::new(b"let x = 42;\n", 1);
        // "let", "x", "=", "42", ";" are significant
        assert!(line.significant_token_count() >= 4);
    }

    // =========================================================================
    // TokenChange tests
    // =========================================================================

    #[test]
    fn test_token_change_unchanged() {
        let token = Token::new(b"hello", TokenKind::Word, 0);
        let change = TokenChange::Unchanged {
            token: token.clone(),
            old_range: 0..5,
            new_range: 0..5,
        };
        assert!(change.is_unchanged());
        assert!(!change.is_change());
        assert!(change.old_token().is_some());
        assert!(change.new_token().is_some());
    }

    #[test]
    fn test_token_change_inserted() {
        let token = Token::new(b"world", TokenKind::Word, 6);
        let change = TokenChange::Inserted {
            token: token.clone(),
            new_range: 6..11,
        };
        assert!(change.is_inserted());
        assert!(change.is_change());
        assert!(change.old_token().is_none());
        assert!(change.new_token().is_some());
        assert!(change.old_range().is_none());
        assert!(change.new_range().is_some());
    }

    #[test]
    fn test_token_change_deleted() {
        let token = Token::new(b"old", TokenKind::Word, 0);
        let change = TokenChange::Deleted {
            token: token.clone(),
            old_range: 0..3,
        };
        assert!(change.is_deleted());
        assert!(change.is_change());
        assert!(change.old_token().is_some());
        assert!(change.new_token().is_none());
    }

    #[test]
    fn test_token_change_replaced() {
        let old_token = Token::new(b"foo", TokenKind::Word, 0);
        let new_token = Token::new(b"bar", TokenKind::Word, 0);
        let change = TokenChange::Replaced {
            old_token,
            new_token,
            old_range: 0..3,
            new_range: 0..3,
        };
        assert!(change.is_replaced());
        assert!(change.is_change());
    }

    #[test]
    fn test_token_change_description() {
        let token = Token::new(b"hello", TokenKind::Word, 0);
        let change = TokenChange::Inserted {
            token,
            new_range: 0..5,
        };
        let desc = change.description();
        assert!(desc.contains("inserted"));
        assert!(desc.contains("hello"));
    }

    // =========================================================================
    // LineChange tests
    // =========================================================================

    #[test]
    fn test_line_change_added() {
        let line = SemanticLine::new(b"new line\n", 5);
        let tokens = create_insertion_tokens(&line);
        let change = LineChange::Added {
            line_num: 5,
            line,
            tokens,
        };
        assert!(change.is_added());
        assert!(!change.is_deleted());
        assert!(!change.is_modified());
        assert!(change.old_line_num().is_none());
        assert_eq!(change.new_line_num(), Some(5));
    }

    #[test]
    fn test_line_change_deleted() {
        let line = SemanticLine::new(b"old line\n", 3);
        let tokens = create_deletion_tokens(&line);
        let change = LineChange::Deleted {
            line_num: 3,
            line,
            tokens,
        };
        assert!(change.is_deleted());
        assert_eq!(change.old_line_num(), Some(3));
        assert!(change.new_line_num().is_none());
    }

    #[test]
    fn test_line_change_modified() {
        let before = SemanticLine::new(b"let x = 1;\n", 1);
        let after = SemanticLine::new(b"let x = 2;\n", 1);
        let token_changes = compute_token_changes(&before, &after, &WordDiffConfig::default());

        let change = LineChange::Modified {
            old_line_num: 1,
            new_line_num: 1,
            before,
            after,
            token_changes,
        };
        assert!(change.is_modified());
        assert_eq!(change.old_line_num(), Some(1));
        assert_eq!(change.new_line_num(), Some(1));
    }

    #[test]
    fn test_line_change_summary() {
        let line = SemanticLine::new(b"added\n", 1);
        let tokens = create_insertion_tokens(&line);
        let change = LineChange::Added {
            line_num: 1,
            line,
            tokens,
        };
        let summary = change.summary();
        assert!(summary.starts_with("+1:"));
    }

    // =========================================================================
    // SemanticDiffStats tests
    // =========================================================================

    #[test]
    fn test_semantic_diff_stats_default() {
        let stats = SemanticDiffStats::default();
        assert!(!stats.has_changes());
        assert_eq!(stats.total_line_changes(), 0);
        assert_eq!(stats.total_token_changes(), 0);
    }

    #[test]
    fn test_semantic_diff_stats_has_changes() {
        let mut stats = SemanticDiffStats::default();
        stats.lines_added = 1;
        assert!(stats.has_changes());
    }

    #[test]
    fn test_semantic_diff_stats_totals() {
        let stats = SemanticDiffStats {
            lines_added: 2,
            lines_deleted: 1,
            lines_modified: 3,
            tokens_inserted: 10,
            tokens_deleted: 5,
            tokens_replaced: 2,
        };
        assert_eq!(stats.total_line_changes(), 6);
        assert_eq!(stats.total_token_changes(), 17);
    }

    // =========================================================================
    // SemanticDiffConfig tests
    // =========================================================================

    #[test]
    fn test_semantic_diff_config_default() {
        let config = SemanticDiffConfig::default();
        assert!(!config.include_context);
        assert!(!config.ignore_whitespace);
        assert!(!config.ignore_blank_lines);
    }

    #[test]
    fn test_semantic_diff_config_builder() {
        let config = SemanticDiffConfig::new()
            .algorithm(Algorithm::Patience)
            .with_context(5)
            .ignore_whitespace()
            .ignore_blank_lines();

        assert_eq!(config.algorithm, Algorithm::Patience);
        assert!(config.include_context);
        assert_eq!(config.context_lines, 5);
        assert!(config.ignore_whitespace);
        assert!(config.ignore_blank_lines);
    }

    // =========================================================================
    // semantic_diff tests
    // =========================================================================

    #[test]
    fn test_semantic_diff_identical() {
        let content = b"line1\nline2\nline3\n";
        let diff = semantic_diff(content, content);
        assert!(diff.is_unchanged());
        assert!(!diff.has_changes());
        assert!(diff.changes().is_empty());
    }

    #[test]
    fn test_semantic_diff_empty() {
        let diff = semantic_diff(b"", b"");
        assert!(diff.is_unchanged());
        assert!(diff.is_empty());
    }

    #[test]
    fn test_semantic_diff_add_line() {
        let old = b"line1\nline3\n";
        let new = b"line1\nline2\nline3\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());
        assert_eq!(diff.stats().lines_added, 1);
        assert_eq!(diff.stats().lines_deleted, 0);

        let added: Vec<_> = diff.added_lines().collect();
        assert_eq!(added.len(), 1);
    }

    #[test]
    fn test_semantic_diff_delete_line() {
        let old = b"line1\nline2\nline3\n";
        let new = b"line1\nline3\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());
        assert_eq!(diff.stats().lines_deleted, 1);
        assert_eq!(diff.stats().lines_added, 0);

        let deleted: Vec<_> = diff.deleted_lines().collect();
        assert_eq!(deleted.len(), 1);
    }

    #[test]
    fn test_semantic_diff_modify_line() {
        let old = b"let x = 1;\n";
        let new = b"let x = 42;\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());
        assert_eq!(diff.stats().lines_modified, 1);

        let modified: Vec<_> = diff.modified_lines().collect();
        assert_eq!(modified.len(), 1);

        // Check that we have token-level changes
        let change = &modified[0];
        let token_changes = change.token_changes();
        assert!(!token_changes.is_empty());

        // Should have some replaced tokens (1 -> 42)
        let replaced: Vec<_> = token_changes.iter().filter(|tc| tc.is_replaced()).collect();
        assert!(!replaced.is_empty(), "Expected replaced tokens for 1 -> 42");
    }

    #[test]
    fn test_semantic_diff_token_level_detail() {
        // This is THE test - proving we get token-level granularity
        let old = b"const result = calculateSum(a, b);\n";
        let new = b"const result = calculateSum(a, b, c);\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());

        // Should be a modified line, not add+delete
        assert_eq!(diff.stats().lines_modified, 1);
        assert_eq!(diff.stats().lines_added, 0);
        assert_eq!(diff.stats().lines_deleted, 0);

        // Get the modification
        let modified: Vec<_> = diff.modified_lines().collect();
        let change = &modified[0];

        // The token changes should show the ", c" being added
        let token_changes = change.token_changes();
        let insertions: Vec<_> = token_changes.iter().filter(|tc| tc.is_inserted()).collect();

        // We should have insertions for ", c"
        assert!(!insertions.is_empty(), "Expected token insertions for ', c'");

        // Verify we can find the 'c' token
        let has_c = insertions.iter().any(|tc| {
            if let TokenChange::Inserted { token, .. } = tc {
                token.as_str() == "c"
            } else {
                false
            }
        });
        assert!(has_c, "Expected to find inserted 'c' token");
    }

    #[test]
    fn test_semantic_diff_variable_rename() {
        let old = b"let foo = 42;\n";
        let new = b"let bar = 42;\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());
        assert_eq!(diff.stats().lines_modified, 1);

        let modified: Vec<_> = diff.modified_lines().collect();
        let token_changes = modified[0].token_changes();

        // Should have a replacement from 'foo' to 'bar'
        let replaced: Vec<_> = token_changes.iter().filter(|tc| tc.is_replaced()).collect();
        assert!(!replaced.is_empty());

        // Check for foo -> bar replacement
        let has_foo_bar = replaced.iter().any(|tc| {
            if let TokenChange::Replaced {
                old_token,
                new_token,
                ..
            } = tc
            {
                old_token.as_str() == "foo" && new_token.as_str() == "bar"
            } else {
                false
            }
        });
        assert!(has_foo_bar, "Expected foo -> bar replacement");
    }

    #[test]
    fn test_semantic_diff_multiline() {
        let old = b"fn main() {\n    let x = 1;\n    println!(x);\n}\n";
        let new = b"fn main() {\n    let x = 42;\n    let y = 2;\n    println!(x);\n}\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());

        // Line 2 modified (1 -> 42), line 3 added (let y = 2)
        assert!(diff.stats().lines_modified >= 1);
        assert!(diff.stats().lines_added >= 1);
    }

    #[test]
    fn test_semantic_diff_all_token_changes() {
        let old = b"a b c\n";
        let new = b"a x c\n";

        let diff = semantic_diff(old, new);
        let all_changes: Vec<_> = diff.all_token_changes().collect();

        // Should have multiple token changes (unchanged 'a', replaced 'b'->'x', unchanged 'c')
        assert!(!all_changes.is_empty());
    }

    #[test]
    fn test_semantic_diff_display() {
        let old = b"old\n";
        let new = b"new\n";

        let diff = semantic_diff(old, new);
        let display = format!("{}", diff);

        // Should produce some output
        assert!(!display.is_empty());
    }

    // =========================================================================
    // Helper function tests
    // =========================================================================

    #[test]
    fn test_create_insertion_tokens() {
        let line = SemanticLine::new(b"hello world\n", 1);
        let tokens = create_insertion_tokens(&line);

        // All should be insertions
        for tc in &tokens {
            assert!(tc.is_inserted());
        }
    }

    #[test]
    fn test_create_deletion_tokens() {
        let line = SemanticLine::new(b"goodbye world\n", 1);
        let tokens = create_deletion_tokens(&line);

        // All should be deletions
        for tc in &tokens {
            assert!(tc.is_deleted());
        }
    }

    #[test]
    fn test_token_byte_range() {
        let content = b"hello world";
        let tokens: Vec<Token> = Tokenizer::new(content).collect();

        // First token "hello" should be at 0..5
        let range0 = token_byte_range(&tokens, 0);
        assert_eq!(range0, 0..5);

        // Space should be at 5..6
        let range1 = token_byte_range(&tokens, 1);
        assert_eq!(range1, 5..6);

        // "world" should be at 6..11
        let range2 = token_byte_range(&tokens, 2);
        assert_eq!(range2, 6..11);
    }

    #[test]
    fn test_compute_token_changes_identical() {
        let line1 = SemanticLine::new(b"let x = 42;\n", 1);
        let line2 = SemanticLine::new(b"let x = 42;\n", 1);

        let changes = compute_token_changes(&line1, &line2, &WordDiffConfig::default());

        // All tokens should be unchanged
        for tc in &changes {
            assert!(tc.is_unchanged(), "Expected unchanged, got: {:?}", tc);
        }
    }

    #[test]
    fn test_semantic_diff_hello_world_highlighting() {
        // This is THE canonical test from the spec:
        // "hello" → "hello_world" highlighting
        let old = b"let name = \"hello\";\n";
        let new = b"let name = \"hello_world\";\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());
        assert_eq!(diff.stats().lines_modified, 1);

        // Get the modified line
        let modified: Vec<_> = diff.modified_lines().collect();
        assert_eq!(modified.len(), 1);

        let change = &modified[0];
        let token_changes = change.token_changes();

        // Should have a replacement for the string token
        let replaced: Vec<_> = token_changes.iter().filter(|tc| tc.is_replaced()).collect();
        assert!(!replaced.is_empty(), "Expected string replacement");

        // Verify the string changed from "hello" to "hello_world"
        let has_hello_change = replaced.iter().any(|tc| {
            if let TokenChange::Replaced {
                old_token,
                new_token,
                ..
            } = tc
            {
                old_token.as_str().contains("hello") && new_token.as_str().contains("hello_world")
            } else {
                false
            }
        });
        assert!(
            has_hello_change,
            "Expected 'hello' -> 'hello_world' replacement"
        );
    }

    #[test]
    fn test_semantic_diff_function_argument_added() {
        // Test the example from the spec:
        // calculateSum(a, b) → calculateSum(a, b, c)
        let old = b"const result = calculateSum(a, b);\n";
        let new = b"const result = calculateSum(a, b, c);\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());

        // Should be a modified line (not delete + add)
        assert_eq!(diff.stats().lines_modified, 1);
        assert_eq!(diff.stats().lines_added, 0);
        assert_eq!(diff.stats().lines_deleted, 0);

        // Get token changes
        let modified: Vec<_> = diff.modified_lines().collect();
        let token_changes = modified[0].token_changes();

        // Should have insertions for ", c"
        let insertions: Vec<_> = token_changes.iter().filter(|tc| tc.is_inserted()).collect();
        assert!(!insertions.is_empty(), "Expected insertions for ', c'");

        // Most tokens should be unchanged
        let unchanged_count = token_changes.iter().filter(|tc| tc.is_unchanged()).count();
        assert!(unchanged_count > insertions.len(), "Most tokens should be unchanged");
    }

    #[test]
    fn test_compute_token_changes_single_token_change() {
        let line1 = SemanticLine::new(b"let x = 1;\n", 1);
        let line2 = SemanticLine::new(b"let x = 2;\n", 1);

        let changes = compute_token_changes(&line1, &line2, &WordDiffConfig::default());

        // Most tokens unchanged, one replaced (1 -> 2)
        let replaced: Vec<_> = changes.iter().filter(|tc| tc.is_replaced()).collect();
        assert!(!replaced.is_empty());

        // Verify it's the number that changed
        let has_num_change = replaced.iter().any(|tc| {
            if let TokenChange::Replaced {
                old_token,
                new_token,
                ..
            } = tc
            {
                old_token.as_str() == "1" && new_token.as_str() == "2"
            } else {
                false
            }
        });
        assert!(has_num_change);
    }
}

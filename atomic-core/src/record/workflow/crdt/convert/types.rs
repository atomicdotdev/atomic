//! Conversion types: options, errors, stats, and result containers.

use crate::crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkOp};
use serde::{Deserialize, Serialize};
use std::fmt;

use super::super::tokenize::TokenizeOptions;

// CONVERSION OPTIONS

/// Options controlling graph_op to CRDT conversion.
///
/// These options allow customization of how hunks are converted to
/// CRDT operations, including tokenization settings and optimization flags.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::convert::ConversionOptions;
///
/// let options = ConversionOptions::default()
///     .with_tokenize_content(true)
///     .with_preserve_whitespace(true);
///
/// assert!(options.tokenize_content());
/// assert!(options.preserve_whitespace());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionOptions {
    /// Whether to tokenize content into individual leaf operations.
    /// If false, each line becomes a single leaf.
    pub(crate) tokenize_content: bool,

    /// Whether to preserve whitespace as separate tokens.
    pub(crate) preserve_whitespace: bool,

    /// Whether to use code-aware tokenization.
    pub(crate) code_aware: bool,

    /// Whether to generate branch operations for empty lines.
    pub(crate) include_empty_lines: bool,

    /// Maximum content size to tokenize (larger content treated as binary).
    pub(crate) max_tokenize_size: usize,
}

impl ConversionOptions {
    /// Default maximum tokenize size (1MB).
    pub const DEFAULT_MAX_TOKENIZE_SIZE: usize = 1024 * 1024;

    /// Creates new options with default settings.
    pub fn new() -> Self {
        Self {
            tokenize_content: true,
            preserve_whitespace: true,
            code_aware: true,
            include_empty_lines: true,
            max_tokenize_size: Self::DEFAULT_MAX_TOKENIZE_SIZE,
        }
    }

    /// Sets whether to tokenize content into leaves.
    pub fn with_tokenize_content(mut self, tokenize: bool) -> Self {
        self.tokenize_content = tokenize;
        self
    }

    /// Sets whether to preserve whitespace as separate tokens.
    pub fn with_preserve_whitespace(mut self, preserve: bool) -> Self {
        self.preserve_whitespace = preserve;
        self
    }

    /// Sets whether to use code-aware tokenization.
    pub fn with_code_aware(mut self, aware: bool) -> Self {
        self.code_aware = aware;
        self
    }

    /// Sets whether to include empty lines.
    pub fn with_include_empty_lines(mut self, include: bool) -> Self {
        self.include_empty_lines = include;
        self
    }

    /// Sets the maximum content size to tokenize.
    pub fn with_max_tokenize_size(mut self, size: usize) -> Self {
        self.max_tokenize_size = size;
        self
    }

    /// Returns whether content tokenization is enabled.
    #[inline]
    pub fn tokenize_content(&self) -> bool {
        self.tokenize_content
    }

    /// Returns whether whitespace preservation is enabled.
    #[inline]
    pub fn preserve_whitespace(&self) -> bool {
        self.preserve_whitespace
    }

    /// Returns whether code-aware tokenization is enabled.
    #[inline]
    pub fn code_aware(&self) -> bool {
        self.code_aware
    }

    /// Returns whether empty lines are included.
    #[inline]
    pub fn include_empty_lines(&self) -> bool {
        self.include_empty_lines
    }

    /// Returns the maximum tokenize size.
    #[inline]
    pub fn max_tokenize_size(&self) -> usize {
        self.max_tokenize_size
    }

    /// Converts to tokenize options for the tokenizer.
    pub fn to_tokenize_options(&self) -> TokenizeOptions {
        TokenizeOptions::new()
            .with_merge_whitespace(!self.preserve_whitespace)
            .with_code_aware(self.code_aware)
    }
}

impl Default for ConversionOptions {
    fn default() -> Self {
        Self::new()
    }
}

// CONVERT ERROR

/// Errors that can occur during graph_op conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvertError {
    /// The graph_op type is not supported for conversion.
    UnsupportedHunk {
        /// Description of the unsupported graph_op.
        description: String,
    },

    /// Content is too large to tokenize.
    ContentTooLarge {
        /// Actual size in bytes.
        size: usize,
        /// Maximum allowed size.
        max_size: usize,
    },

    /// Required content is missing from the graph_op.
    MissingContent {
        /// Description of what content is missing.
        description: String,
    },

    /// Invalid state during conversion.
    InvalidState {
        /// Description of the invalid state.
        description: String,
    },

    /// Tokenization failed.
    TokenizationFailed {
        /// The error message.
        message: String,
    },
}

impl fmt::Display for ConvertError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConvertError::UnsupportedHunk { description } => {
                write!(f, "unsupported graph_op type: {}", description)
            }
            ConvertError::ContentTooLarge { size, max_size } => {
                write!(
                    f,
                    "content too large to tokenize: {} bytes (max {})",
                    size, max_size
                )
            }
            ConvertError::MissingContent { description } => {
                write!(f, "missing content: {}", description)
            }
            ConvertError::InvalidState { description } => {
                write!(f, "invalid conversion state: {}", description)
            }
            ConvertError::TokenizationFailed { message } => {
                write!(f, "tokenization failed: {}", message)
            }
        }
    }
}

impl std::error::Error for ConvertError {}

/// Result type for conversion operations.
pub type ConvertResult<T> = Result<T, ConvertError>;

// CONVERSION STATS

/// Statistics about the conversion process.
///
/// Tracks counts and metrics about converted operations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversionStats {
    /// Number of hunks converted.
    pub hunks_converted: usize,

    /// Number of trunk operations generated.
    pub trunk_ops: usize,

    /// Number of branch operations generated.
    pub branch_ops: usize,

    /// Number of leaf operations generated.
    pub leaf_ops: usize,

    /// Total content bytes processed.
    pub content_bytes: usize,

    /// Number of lines processed.
    pub lines_processed: usize,

    /// Number of tokens generated.
    pub tokens_generated: usize,

    /// Number of files added.
    pub files_added: usize,

    /// Number of files deleted.
    pub files_deleted: usize,

    /// Number of files moved.
    pub files_moved: usize,
}

impl ConversionStats {
    /// Creates new empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Merges another stats instance into this one.
    pub fn merge(&mut self, other: &ConversionStats) {
        self.hunks_converted += other.hunks_converted;
        self.trunk_ops += other.trunk_ops;
        self.branch_ops += other.branch_ops;
        self.leaf_ops += other.leaf_ops;
        self.content_bytes += other.content_bytes;
        self.lines_processed += other.lines_processed;
        self.tokens_generated += other.tokens_generated;
        self.files_added += other.files_added;
        self.files_deleted += other.files_deleted;
        self.files_moved += other.files_moved;
    }

    /// Returns the total number of operations generated.
    pub fn total_ops(&self) -> usize {
        self.trunk_ops + self.branch_ops + self.leaf_ops
    }
}

impl fmt::Display for ConversionStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} hunks → {} trunk + {} branch + {} leaf ops ({} bytes, {} lines)",
            self.hunks_converted,
            self.trunk_ops,
            self.branch_ops,
            self.leaf_ops,
            self.content_bytes,
            self.lines_processed
        )
    }
}

// CONVERTED OPS

/// The result of converting hunks to CRDT operations.
///
/// Contains all generated operations organized by level (trunk, branch, leaf)
/// along with the accumulated content buffer and conversion statistics.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::convert::ConvertedOps;
///
/// let ops = ConvertedOps::new();
/// assert!(ops.is_empty());
/// assert_eq!(ops.trunk_ops().len(), 0);
/// ```
#[derive(Debug, Clone)]
pub struct ConvertedOps {
    /// File-level operations.
    pub(crate) trunk_ops: Vec<TrunkOp>,

    /// Line-level operations.
    pub(crate) branch_ops: Vec<(BranchId, BranchOp)>,

    /// Token-level operations.
    pub(crate) leaf_ops: Vec<(LeafId, LeafOp)>,

    /// Accumulated content for leaves.
    pub(crate) content: Vec<u8>,

    /// Conversion statistics.
    pub(crate) stats: ConversionStats,
}

impl ConvertedOps {
    /// Creates a new empty result.
    pub fn new() -> Self {
        Self {
            trunk_ops: Vec::new(),
            branch_ops: Vec::new(),
            leaf_ops: Vec::new(),
            content: Vec::new(),
            stats: ConversionStats::new(),
        }
    }

    /// Returns true if no operations were generated.
    pub fn is_empty(&self) -> bool {
        self.trunk_ops.is_empty() && self.branch_ops.is_empty() && self.leaf_ops.is_empty()
    }

    /// Returns the trunk operations.
    #[inline]
    pub fn trunk_ops(&self) -> &[TrunkOp] {
        &self.trunk_ops
    }

    /// Returns the branch operations with their IDs.
    #[inline]
    pub fn branch_ops(&self) -> &[(BranchId, BranchOp)] {
        &self.branch_ops
    }

    /// Returns the leaf operations with their IDs.
    #[inline]
    pub fn leaf_ops(&self) -> &[(LeafId, LeafOp)] {
        &self.leaf_ops
    }

    /// Returns the accumulated content.
    #[inline]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Returns the conversion statistics.
    #[inline]
    pub fn stats(&self) -> &ConversionStats {
        &self.stats
    }

    /// Adds a trunk operation.
    pub fn add_trunk_op(&mut self, op: TrunkOp) {
        self.trunk_ops.push(op);
        self.stats.trunk_ops += 1;
    }

    /// Adds a branch operation with its ID.
    pub fn add_branch_op(&mut self, id: BranchId, op: BranchOp) {
        self.branch_ops.push((id, op));
        self.stats.branch_ops += 1;
    }

    /// Adds a leaf operation with its ID.
    pub fn add_leaf_op(&mut self, id: LeafId, op: LeafOp) {
        self.leaf_ops.push((id, op));
        self.stats.leaf_ops += 1;
    }

    /// Appends content and returns the byte range.
    pub fn append_content(&mut self, data: &[u8]) -> std::ops::Range<usize> {
        let start = self.content.len();
        self.content.extend_from_slice(data);
        let end = self.content.len();
        self.stats.content_bytes += data.len();
        start..end
    }

    /// Merges another ConvertedOps into this one.
    pub fn merge(&mut self, other: ConvertedOps) {
        // Adjust content ranges in other's leaf ops
        let _content_offset = self.content.len();

        self.trunk_ops.extend(other.trunk_ops);
        self.branch_ops.extend(other.branch_ops);

        // Note: Leaf ops with content references would need adjustment
        // For now, we just append them directly
        self.leaf_ops.extend(other.leaf_ops);
        self.content.extend(other.content);
        self.stats.merge(&other.stats);
    }

    /// Consumes the result and returns the operations.
    #[allow(clippy::type_complexity)]
    pub fn into_parts(
        self,
    ) -> (
        Vec<TrunkOp>,
        Vec<(BranchId, BranchOp)>,
        Vec<(LeafId, LeafOp)>,
        Vec<u8>,
    ) {
        (self.trunk_ops, self.branch_ops, self.leaf_ops, self.content)
    }
}

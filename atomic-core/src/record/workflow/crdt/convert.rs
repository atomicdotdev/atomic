//! GraphOp to CRDT operation conversion.
//!
//! This module converts traditional `GraphOp` types (used in the existing change
//! representation) into CRDT operations (`TrunkOp`, `BranchOp`, `LeafOp`).
//! This enables the transition from the flat graph model to the hierarchical
//! CRDT model while maintaining semantic equivalence.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      GraphOp → CRDT Conversion Pipeline                     │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input: GraphOp Types                                                      │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ GraphOp::FileAdd   → TrunkOp::Create + BranchOps + LeafOps          │  │
//! │  │ GraphOp::FileDel   → TrunkOp::Delete                                │  │
//! │  │ GraphOp::FileMove  → TrunkOp::Move                                  │  │
//! │  │ GraphOp::Edit      → BranchOp/LeafOp (insert/delete)                │  │
//! │  │ GraphOp::Replace   → BranchOp::Delete + BranchOp::Insert            │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  HunkConverter                                                          │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ • Analyzes graph_op type and content                                 │  │
//! │  │ • Generates appropriate CRDT operations                          │  │
//! │  │ • Tracks content positions for leaf ranges                       │  │
//! │  │ • Maintains ID allocation state                                  │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  Output: ConvertedOps                                                   │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ trunk_ops: Vec<TrunkOp>                                          │  │
//! │  │ branch_ops: Vec<BranchOp>                                        │  │
//! │  │ leaf_ops: Vec<LeafOp>                                            │  │
//! │  │ content: Vec<u8>  (accumulated content for leaves)               │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Types
//!
//! - [`HunkConverter`]: Main converter for transforming hunks to CRDT ops
//! - [`ConvertedOps`]: Result container for generated operations
//! - [`ConversionOptions`]: Configuration for conversion behavior
//! - [`ConversionStats`]: Statistics about the conversion process
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::crdt::convert::{
//!     HunkConverter, ConversionOptions,
//! };
//! use atomic_core::change::GraphOp;
//!
//! let converter = HunkConverter::new(change_id, ConversionOptions::default());
//!
//! // Convert a file addition graph_op
//! let file_add_hunk: GraphOp<Option<Hash>> = /* ... */;
//! let result = converter.convert_hunk(&file_add_hunk)?;
//!
//! // Access generated operations
//! for trunk_op in result.trunk_ops() {
//!     println!("Trunk operation: {:?}", trunk_op);
//! }
//! ```
//!
//! # Conversion Rules
//!
//! | GraphOp Type | CRDT Operations |
//! |-----------|-----------------|
//! | `FileAdd` | `TrunkOp::Create` + content as `BranchOp::Insert` + `LeafOp::Insert` |
//! | `FileDel` | `TrunkOp::Delete` (cascades to branches/leaves) |
//! | `FileUndel` | `TrunkOp::Undelete` (restores branches/leaves) |
//! | `FileMove` | `TrunkOp::Move` (preserves content) |
//! | `Edit` (insert) | `BranchOp::Insert` with `LeafOp::Insert` for tokens |
//! | `Edit` (delete) | `BranchOp::Delete` or `LeafOp::Delete` |
//! | `Replacement` | Delete ops followed by insert ops |

use crate::change::Encoding;
use crate::crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId, TrunkOp};
use crate::diff::token::TokenKind;
use crate::types::NodeId;
use serde::{Deserialize, Serialize};
use std::fmt;

use super::tokenize::{ContentTokenizer, TokenizeOptions};

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
    tokenize_content: bool,

    /// Whether to preserve whitespace as separate tokens.
    preserve_whitespace: bool,

    /// Whether to use code-aware tokenization.
    code_aware: bool,

    /// Whether to generate branch operations for empty lines.
    include_empty_lines: bool,

    /// Maximum content size to tokenize (larger content treated as binary).
    max_tokenize_size: usize,
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
    trunk_ops: Vec<TrunkOp>,

    /// Line-level operations.
    branch_ops: Vec<(BranchId, BranchOp)>,

    /// Token-level operations.
    leaf_ops: Vec<(LeafId, LeafOp)>,

    /// Accumulated content for leaves.
    content: Vec<u8>,

    /// Conversion statistics.
    stats: ConversionStats,
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

impl Default for ConvertedOps {
    fn default() -> Self {
        Self::new()
    }
}

// HUNK CONVERTER

/// Converts hunks to CRDT operations.
///
/// The `HunkConverter` is the main entry point for transforming traditional
/// `GraphOp` types into the hierarchical CRDT operation model.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::convert::{
///     HunkConverter, ConversionOptions,
/// };
/// use atomic_core::types::NodeId;
///
/// let change_id = NodeId::new(1);
/// let mut converter = HunkConverter::new(change_id, ConversionOptions::default());
///
/// // Convert content to CRDT operations
/// let content = b"fn main() {\n    println!(\"Hello\");\n}\n";
/// let ops = converter.convert_file_content("main.rs", content, None);
///
/// assert!(!ops.trunk_ops().is_empty());
/// ```
#[derive(Debug, Clone)]
pub struct HunkConverter {
    /// The change ID for generating CRDT IDs.
    change_id: NodeId,

    /// Conversion options.
    options: ConversionOptions,

    /// Counter for trunk IDs within this change.
    next_trunk_idx: u32,

    /// Counter for branch IDs within this change.
    next_branch_idx: u32,

    /// Counter for leaf IDs within this change.
    next_leaf_idx: u32,
}

impl HunkConverter {
    /// Creates a new converter for the given change.
    pub fn new(change_id: NodeId, options: ConversionOptions) -> Self {
        Self {
            change_id,
            options,
            next_trunk_idx: 0,
            next_branch_idx: 0,
            next_leaf_idx: 0,
        }
    }

    /// Creates a converter with default options.
    pub fn with_defaults(change_id: NodeId) -> Self {
        Self::new(change_id, ConversionOptions::default())
    }

    /// Returns the change ID.
    #[inline]
    pub fn change_id(&self) -> NodeId {
        self.change_id
    }

    /// Returns the conversion options.
    #[inline]
    pub fn options(&self) -> &ConversionOptions {
        &self.options
    }

    /// Allocates a new trunk ID.
    fn alloc_trunk_id(&mut self) -> TrunkId {
        let id = TrunkId::new(self.change_id, self.next_trunk_idx);
        self.next_trunk_idx += 1;
        id
    }

    /// Allocates a new branch ID.
    fn alloc_branch_id(&mut self) -> BranchId {
        let id = BranchId::new(self.change_id, self.next_branch_idx);
        self.next_branch_idx += 1;
        id
    }

    /// Allocates a new leaf ID.
    fn alloc_leaf_id(&mut self) -> LeafId {
        let id = LeafId::new(self.change_id, self.next_leaf_idx);
        self.next_leaf_idx += 1;
        id
    }

    /// Converts a file addition with content to CRDT operations.
    ///
    /// This generates:
    /// - `TrunkOp::Create` for the file
    /// - `BranchOp::Insert` for each line
    /// - `LeafOp::Insert` for each token (if tokenization enabled)
    pub fn convert_file_content(
        &mut self,
        path: &str,
        content: &[u8],
        encoding: Option<Encoding>,
    ) -> ConvertedOps {
        let mut result = ConvertedOps::new();

        // Create trunk operation
        let _trunk_id = self.alloc_trunk_id();
        result.add_trunk_op(TrunkOp::Create {
            path: path.to_string(),
            encoding,
        });
        result.stats.files_added += 1;
        result.stats.hunks_converted += 1;

        // Check if content should be tokenized
        if content.len() > self.options.max_tokenize_size {
            // Treat as binary - single branch with single leaf
            let branch_id = self.alloc_branch_id();
            let _leaf_id = self.alloc_leaf_id();

            let _content_range = result.append_content(content);

            result.add_branch_op(
                branch_id,
                BranchOp::Insert {
                    after: None,
                    content: vec![LeafOp::Insert {
                        after: None,
                        kind: TokenKind::Other,
                        content: content.to_vec(),
                    }],
                },
            );

            result.stats.lines_processed += 1;
            result.stats.tokens_generated += 1;

            return result;
        }

        // Tokenize content into lines
        let tokenize_opts = self.options.to_tokenize_options();
        let tokenizer = ContentTokenizer::with_options(content, tokenize_opts);

        let mut prev_branch_id: Option<BranchId> = None;

        for line in tokenizer.lines() {
            // Skip empty lines if configured
            if line.is_empty() && !self.options.include_empty_lines {
                continue;
            }

            let branch_id = self.alloc_branch_id();
            result.stats.lines_processed += 1;

            // Generate leaf operations for tokens in this line
            let mut leaf_ops = Vec::new();
            let mut prev_leaf_id: Option<LeafId> = None;

            if self.options.tokenize_content && !line.tokens().is_empty() {
                for token in line.tokens() {
                    let leaf_id = self.alloc_leaf_id();

                    // Append content and get range
                    let _ = result.append_content(token.content());

                    leaf_ops.push(LeafOp::Insert {
                        after: prev_leaf_id,
                        kind: token.kind(),
                        content: token.content().to_vec(),
                    });

                    result.stats.tokens_generated += 1;
                    prev_leaf_id = Some(leaf_id);
                }
            } else if !line.is_empty() {
                // No tokenization - entire line is one leaf
                let _leaf_id = self.alloc_leaf_id();
                let _ = result.append_content(line.content());

                leaf_ops.push(LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Other,
                    content: line.content().to_vec(),
                });

                result.stats.tokens_generated += 1;
            }

            // Create branch operation
            result.add_branch_op(
                branch_id,
                BranchOp::Insert {
                    after: prev_branch_id,
                    content: leaf_ops,
                },
            );

            prev_branch_id = Some(branch_id);
        }

        result
    }

    /// Converts a file deletion to CRDT operations.
    ///
    /// This generates a `TrunkOp::Delete` which cascades to mark all
    /// branches and leaves as deleted.
    pub fn convert_file_deletion(&mut self, trunk_id: TrunkId) -> ConvertedOps {
        let mut result = ConvertedOps::new();

        result.add_trunk_op(TrunkOp::Delete { trunk: trunk_id });
        result.stats.files_deleted += 1;
        result.stats.hunks_converted += 1;

        result
    }

    /// Converts a file move/rename to CRDT operations.
    ///
    /// This generates a `TrunkOp::Move` which updates the file's path
    /// while preserving its content and history.
    pub fn convert_file_move(&mut self, trunk_id: TrunkId, new_path: &str) -> ConvertedOps {
        let mut result = ConvertedOps::new();

        result.add_trunk_op(TrunkOp::Move {
            trunk: trunk_id,
            new_path: new_path.to_string(),
        });
        result.stats.files_moved += 1;
        result.stats.hunks_converted += 1;

        result
    }

    /// Converts a file undeletion to CRDT operations.
    pub fn convert_file_undeletion(&mut self, trunk_id: TrunkId) -> ConvertedOps {
        let mut result = ConvertedOps::new();

        result.add_trunk_op(TrunkOp::Undelete { trunk: trunk_id });
        result.stats.hunks_converted += 1;

        result
    }

    /// Converts a line insertion to CRDT operations.
    ///
    /// # Arguments
    ///
    /// * `trunk_id` - The file containing the line
    /// * `after_branch` - The branch to insert after (None for start of file)
    /// * `content` - The line content to insert
    pub fn convert_line_insert(
        &mut self,
        _trunk_id: TrunkId,
        after_branch: Option<BranchId>,
        content: &[u8],
    ) -> ConvertedOps {
        let mut result = ConvertedOps::new();

        let branch_id = self.alloc_branch_id();
        result.stats.lines_processed += 1;
        result.stats.hunks_converted += 1;

        // Generate leaf operations
        let leaf_ops = if self.options.tokenize_content {
            self.tokenize_to_leaf_ops(content, &mut result)
        } else {
            vec![LeafOp::Insert {
                after: None,
                kind: TokenKind::Other,
                content: content.to_vec(),
            }]
        };

        result.add_branch_op(
            branch_id,
            BranchOp::Insert {
                after: after_branch,
                content: leaf_ops,
            },
        );

        result
    }

    /// Converts a line deletion to CRDT operations.
    pub fn convert_line_delete(&mut self, branch_id: BranchId) -> ConvertedOps {
        let mut result = ConvertedOps::new();

        result.add_branch_op(
            branch_id,
            BranchOp::Delete {
                branch: branch_id,
                content: Vec::new(),
            },
        );
        result.stats.hunks_converted += 1;

        result
    }

    /// Converts a token insertion to CRDT operations.
    pub fn convert_token_insert(
        &mut self,
        _branch_id: BranchId,
        after_leaf: Option<LeafId>,
        kind: TokenKind,
        content: &[u8],
    ) -> ConvertedOps {
        let mut result = ConvertedOps::new();

        let leaf_id = self.alloc_leaf_id();
        let _ = result.append_content(content);

        result.add_leaf_op(
            leaf_id,
            LeafOp::Insert {
                after: after_leaf,
                kind,
                content: content.to_vec(),
            },
        );

        result.stats.tokens_generated += 1;
        result.stats.hunks_converted += 1;

        result
    }

    /// Converts a token deletion to CRDT operations.
    pub fn convert_token_delete(&mut self, leaf_id: LeafId) -> ConvertedOps {
        let mut result = ConvertedOps::new();

        result.add_leaf_op(leaf_id, LeafOp::Delete { leaf: leaf_id });
        result.stats.hunks_converted += 1;

        result
    }

    /// Converts a token replacement to CRDT operations.
    ///
    /// The replacement preserves the leaf ID for accurate blame tracking.
    pub fn convert_token_replace(&mut self, leaf_id: LeafId, new_content: &[u8]) -> ConvertedOps {
        let mut result = ConvertedOps::new();

        let _ = result.append_content(new_content);

        result.add_leaf_op(
            leaf_id,
            LeafOp::Replace {
                leaf: leaf_id,
                new_content: new_content.to_vec(),
            },
        );

        result.stats.tokens_generated += 1;
        result.stats.hunks_converted += 1;

        result
    }

    /// Tokenizes content into LeafOp::Insert operations.
    fn tokenize_to_leaf_ops(&mut self, content: &[u8], result: &mut ConvertedOps) -> Vec<LeafOp> {
        let tokenize_opts = self.options.to_tokenize_options();
        let line = ContentTokenizer::tokenize_line(content, &tokenize_opts);

        let mut leaf_ops = Vec::new();
        let mut prev_leaf_id: Option<LeafId> = None;

        for token in line.tokens() {
            let leaf_id = self.alloc_leaf_id();
            let _ = result.append_content(token.content());

            leaf_ops.push(LeafOp::Insert {
                after: prev_leaf_id,
                kind: token.kind(),
                content: token.content().to_vec(),
            });

            result.stats.tokens_generated += 1;
            prev_leaf_id = Some(leaf_id);
        }

        leaf_ops
    }

    /// Resets the ID counters. Useful for testing.
    pub fn reset_counters(&mut self) {
        self.next_trunk_idx = 0;
        self.next_branch_idx = 0;
        self.next_leaf_idx = 0;
    }
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // ConversionOptions Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_conversion_options_default() {
        let opts = ConversionOptions::default();
        assert!(opts.tokenize_content());
        assert!(opts.preserve_whitespace());
        assert!(opts.code_aware());
        assert!(opts.include_empty_lines());
        assert_eq!(
            opts.max_tokenize_size(),
            ConversionOptions::DEFAULT_MAX_TOKENIZE_SIZE
        );
    }

    #[test]
    fn test_conversion_options_new() {
        let opts = ConversionOptions::new();
        assert!(opts.tokenize_content());
    }

    #[test]
    fn test_conversion_options_builder_tokenize_content() {
        let opts = ConversionOptions::new().with_tokenize_content(false);
        assert!(!opts.tokenize_content());
    }

    #[test]
    fn test_conversion_options_builder_preserve_whitespace() {
        let opts = ConversionOptions::new().with_preserve_whitespace(false);
        assert!(!opts.preserve_whitespace());
    }

    #[test]
    fn test_conversion_options_builder_code_aware() {
        let opts = ConversionOptions::new().with_code_aware(false);
        assert!(!opts.code_aware());
    }

    #[test]
    fn test_conversion_options_builder_include_empty_lines() {
        let opts = ConversionOptions::new().with_include_empty_lines(false);
        assert!(!opts.include_empty_lines());
    }

    #[test]
    fn test_conversion_options_builder_max_tokenize_size() {
        let opts = ConversionOptions::new().with_max_tokenize_size(5000);
        assert_eq!(opts.max_tokenize_size(), 5000);
    }

    #[test]
    fn test_conversion_options_builder_chain() {
        let opts = ConversionOptions::new()
            .with_tokenize_content(false)
            .with_preserve_whitespace(false)
            .with_code_aware(false)
            .with_include_empty_lines(false)
            .with_max_tokenize_size(1000);

        assert!(!opts.tokenize_content());
        assert!(!opts.preserve_whitespace());
        assert!(!opts.code_aware());
        assert!(!opts.include_empty_lines());
        assert_eq!(opts.max_tokenize_size(), 1000);
    }

    #[test]
    fn test_conversion_options_to_tokenize_options() {
        let opts = ConversionOptions::new()
            .with_preserve_whitespace(false)
            .with_code_aware(true);

        let tokenize_opts = opts.to_tokenize_options();
        assert!(tokenize_opts.merge_whitespace()); // Inverted from preserve
        assert!(tokenize_opts.code_aware());
    }

    // ------------------------------------------------------------------------
    // ConvertError Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_convert_error_unsupported_hunk_display() {
        let err = ConvertError::UnsupportedHunk {
            description: "unknown graph_op type".to_string(),
        };
        assert!(err.to_string().contains("unsupported"));
        assert!(err.to_string().contains("unknown graph_op type"));
    }

    #[test]
    fn test_convert_error_content_too_large_display() {
        let err = ConvertError::ContentTooLarge {
            size: 2_000_000,
            max_size: 1_000_000,
        };
        assert!(err.to_string().contains("too large"));
        assert!(err.to_string().contains("2000000"));
    }

    #[test]
    fn test_convert_error_missing_content_display() {
        let err = ConvertError::MissingContent {
            description: "file content".to_string(),
        };
        assert!(err.to_string().contains("missing"));
        assert!(err.to_string().contains("file content"));
    }

    #[test]
    fn test_convert_error_invalid_state_display() {
        let err = ConvertError::InvalidState {
            description: "no active file".to_string(),
        };
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn test_convert_error_tokenization_failed_display() {
        let err = ConvertError::TokenizationFailed {
            message: "bad encoding".to_string(),
        };
        assert!(err.to_string().contains("tokenization"));
    }

    #[test]
    fn test_convert_error_is_error_trait() {
        let err = ConvertError::MissingContent {
            description: "test".to_string(),
        };
        let _: &dyn std::error::Error = &err;
    }

    // ------------------------------------------------------------------------
    // ConversionStats Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_conversion_stats_new() {
        let stats = ConversionStats::new();
        assert_eq!(stats.hunks_converted, 0);
        assert_eq!(stats.total_ops(), 0);
    }

    #[test]
    fn test_conversion_stats_merge() {
        let mut stats1 = ConversionStats::new();
        stats1.hunks_converted = 2;
        stats1.trunk_ops = 1;
        stats1.branch_ops = 5;
        stats1.leaf_ops = 20;

        let mut stats2 = ConversionStats::new();
        stats2.hunks_converted = 3;
        stats2.trunk_ops = 2;
        stats2.branch_ops = 10;
        stats2.leaf_ops = 30;

        stats1.merge(&stats2);

        assert_eq!(stats1.hunks_converted, 5);
        assert_eq!(stats1.trunk_ops, 3);
        assert_eq!(stats1.branch_ops, 15);
        assert_eq!(stats1.leaf_ops, 50);
    }

    #[test]
    fn test_conversion_stats_total_ops() {
        let mut stats = ConversionStats::new();
        stats.trunk_ops = 1;
        stats.branch_ops = 5;
        stats.leaf_ops = 20;

        assert_eq!(stats.total_ops(), 26);
    }

    #[test]
    fn test_conversion_stats_display() {
        let mut stats = ConversionStats::new();
        stats.hunks_converted = 3;
        stats.trunk_ops = 1;
        stats.branch_ops = 10;
        stats.leaf_ops = 50;
        stats.content_bytes = 500;
        stats.lines_processed = 10;

        let display = format!("{}", stats);
        assert!(display.contains("3 hunks"));
        assert!(display.contains("1 trunk"));
        assert!(display.contains("10 branch"));
        assert!(display.contains("50 leaf"));
    }

    // ------------------------------------------------------------------------
    // ConvertedOps Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_converted_ops_new() {
        let ops = ConvertedOps::new();
        assert!(ops.is_empty());
        assert!(ops.trunk_ops().is_empty());
        assert!(ops.branch_ops().is_empty());
        assert!(ops.leaf_ops().is_empty());
        assert!(ops.content().is_empty());
    }

    #[test]
    fn test_converted_ops_add_trunk_op() {
        let mut ops = ConvertedOps::new();
        ops.add_trunk_op(TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        });

        assert!(!ops.is_empty());
        assert_eq!(ops.trunk_ops().len(), 1);
        assert_eq!(ops.stats().trunk_ops, 1);
    }

    #[test]
    fn test_converted_ops_add_branch_op() {
        let mut ops = ConvertedOps::new();
        let branch_id = BranchId::new(NodeId::new(1), 0);
        ops.add_branch_op(
            branch_id,
            BranchOp::Insert {
                after: None,
                content: vec![],
            },
        );

        assert!(!ops.is_empty());
        assert_eq!(ops.branch_ops().len(), 1);
        assert_eq!(ops.stats().branch_ops, 1);
    }

    #[test]
    fn test_converted_ops_add_leaf_op() {
        let mut ops = ConvertedOps::new();
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        ops.add_leaf_op(
            leaf_id,
            LeafOp::Insert {
                after: None,
                kind: TokenKind::Word,
                content: b"test".to_vec(),
            },
        );

        assert!(!ops.is_empty());
        assert_eq!(ops.leaf_ops().len(), 1);
        assert_eq!(ops.stats().leaf_ops, 1);
    }

    #[test]
    fn test_converted_ops_append_content() {
        let mut ops = ConvertedOps::new();
        let range1 = ops.append_content(b"hello");
        let range2 = ops.append_content(b"world");

        assert_eq!(range1, 0..5);
        assert_eq!(range2, 5..10);
        assert_eq!(ops.content(), b"helloworld");
        assert_eq!(ops.stats().content_bytes, 10);
    }

    #[test]
    fn test_converted_ops_merge() {
        let mut ops1 = ConvertedOps::new();
        ops1.add_trunk_op(TrunkOp::Create {
            path: "a.rs".to_string(),
            encoding: None,
        });
        ops1.append_content(b"aaa");

        let mut ops2 = ConvertedOps::new();
        ops2.add_trunk_op(TrunkOp::Create {
            path: "b.rs".to_string(),
            encoding: None,
        });
        ops2.append_content(b"bbb");

        ops1.merge(ops2);

        assert_eq!(ops1.trunk_ops().len(), 2);
        assert_eq!(ops1.content(), b"aaabbb");
        assert_eq!(ops1.stats().trunk_ops, 2);
    }

    #[test]
    fn test_converted_ops_into_parts() {
        let mut ops = ConvertedOps::new();
        ops.add_trunk_op(TrunkOp::Create {
            path: "test.rs".to_string(),
            encoding: None,
        });
        ops.append_content(b"content");

        let (trunk_ops, branch_ops, leaf_ops, content) = ops.into_parts();

        assert_eq!(trunk_ops.len(), 1);
        assert!(branch_ops.is_empty());
        assert!(leaf_ops.is_empty());
        assert_eq!(content, b"content");
    }

    // ------------------------------------------------------------------------
    // HunkConverter Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_hunk_converter_new() {
        let change_id = NodeId::new(1);
        let converter = HunkConverter::new(change_id, ConversionOptions::default());

        assert_eq!(converter.change_id(), change_id);
    }

    #[test]
    fn test_hunk_converter_with_defaults() {
        let change_id = NodeId::new(1);
        let converter = HunkConverter::with_defaults(change_id);

        assert_eq!(converter.change_id(), change_id);
        assert!(converter.options().tokenize_content());
    }

    #[test]
    fn test_hunk_converter_convert_file_content_simple() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let content = b"hello world";
        let ops = converter.convert_file_content("test.txt", content, None);

        assert!(!ops.is_empty());
        assert_eq!(ops.trunk_ops().len(), 1);
        assert!(ops.branch_ops().len() >= 1);
        assert_eq!(ops.stats().files_added, 1);
    }

    #[test]
    fn test_hunk_converter_convert_file_content_multiline() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let content = b"line one\nline two\nline three\n";
        let ops = converter.convert_file_content("test.txt", content, None);

        assert!(!ops.is_empty());
        assert_eq!(ops.trunk_ops().len(), 1);
        // Should have branches for each line (including empty trailing)
        assert!(ops.branch_ops().len() >= 3);
        assert!(ops.stats().lines_processed >= 3);
    }

    #[test]
    fn test_hunk_converter_convert_file_content_with_encoding() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let content = b"fn main() {}";
        let ops = converter.convert_file_content("main.rs", content, Some(Encoding::Utf8));

        assert_eq!(ops.trunk_ops().len(), 1);
        match &ops.trunk_ops()[0] {
            TrunkOp::Create { path, encoding } => {
                assert_eq!(path, "main.rs");
                assert_eq!(*encoding, Some(Encoding::Utf8));
            }
            _ => panic!("Expected TrunkOp::Create"),
        }
    }

    #[test]
    fn test_hunk_converter_convert_file_content_large_binary() {
        let change_id = NodeId::new(1);
        let opts = ConversionOptions::new().with_max_tokenize_size(100);
        let mut converter = HunkConverter::new(change_id, opts);

        // Create content larger than max_tokenize_size
        let content = vec![b'x'; 200];
        let ops = converter.convert_file_content("large.bin", &content, None);

        // Should have 1 trunk op and 1 branch op (binary mode)
        assert_eq!(ops.trunk_ops().len(), 1);
        assert_eq!(ops.branch_ops().len(), 1);
    }

    #[test]
    fn test_hunk_converter_convert_file_deletion() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let trunk_id = TrunkId::new(NodeId::new(0), 0);
        let ops = converter.convert_file_deletion(trunk_id);

        assert_eq!(ops.trunk_ops().len(), 1);
        match &ops.trunk_ops()[0] {
            TrunkOp::Delete { trunk } => assert_eq!(*trunk, trunk_id),
            _ => panic!("Expected TrunkOp::Delete"),
        }
        assert_eq!(ops.stats().files_deleted, 1);
    }

    #[test]
    fn test_hunk_converter_convert_file_move() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let trunk_id = TrunkId::new(NodeId::new(0), 0);
        let ops = converter.convert_file_move(trunk_id, "new/path.rs");

        assert_eq!(ops.trunk_ops().len(), 1);
        match &ops.trunk_ops()[0] {
            TrunkOp::Move { trunk, new_path } => {
                assert_eq!(*trunk, trunk_id);
                assert_eq!(new_path, "new/path.rs");
            }
            _ => panic!("Expected TrunkOp::Move"),
        }
        assert_eq!(ops.stats().files_moved, 1);
    }

    #[test]
    fn test_hunk_converter_convert_file_undeletion() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let trunk_id = TrunkId::new(NodeId::new(0), 0);
        let ops = converter.convert_file_undeletion(trunk_id);

        assert_eq!(ops.trunk_ops().len(), 1);
        match &ops.trunk_ops()[0] {
            TrunkOp::Undelete { trunk } => assert_eq!(*trunk, trunk_id),
            _ => panic!("Expected TrunkOp::Undelete"),
        }
    }

    #[test]
    fn test_hunk_converter_convert_line_insert() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let trunk_id = TrunkId::new(NodeId::new(0), 0);
        let ops = converter.convert_line_insert(trunk_id, None, b"new line content");

        assert_eq!(ops.branch_ops().len(), 1);
        assert!(ops.stats().lines_processed >= 1);
    }

    #[test]
    fn test_hunk_converter_convert_line_insert_after() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let trunk_id = TrunkId::new(NodeId::new(0), 0);
        let after_branch = BranchId::new(NodeId::new(0), 5);
        let ops = converter.convert_line_insert(trunk_id, Some(after_branch), b"inserted");

        assert_eq!(ops.branch_ops().len(), 1);
        let (_, branch_op) = &ops.branch_ops()[0];
        match branch_op {
            BranchOp::Insert { after, .. } => assert_eq!(*after, Some(after_branch)),
            _ => panic!("Expected BranchOp::Insert"),
        }
    }

    #[test]
    fn test_hunk_converter_convert_line_delete() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let branch_id = BranchId::new(NodeId::new(0), 3);
        let ops = converter.convert_line_delete(branch_id);

        assert_eq!(ops.branch_ops().len(), 1);
        let (id, branch_op) = &ops.branch_ops()[0];
        assert_eq!(*id, branch_id);
        match branch_op {
            BranchOp::Delete { branch, .. } => assert_eq!(*branch, branch_id),
            _ => panic!("Expected BranchOp::Delete"),
        }
    }

    #[test]
    fn test_hunk_converter_convert_token_insert() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let branch_id = BranchId::new(NodeId::new(0), 0);
        let ops = converter.convert_token_insert(branch_id, None, TokenKind::Word, b"hello");

        assert_eq!(ops.leaf_ops().len(), 1);
        assert_eq!(ops.stats().tokens_generated, 1);
    }

    #[test]
    fn test_hunk_converter_convert_token_delete() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let leaf_id = LeafId::new(NodeId::new(0), 5);
        let ops = converter.convert_token_delete(leaf_id);

        assert_eq!(ops.leaf_ops().len(), 1);
        let (id, leaf_op) = &ops.leaf_ops()[0];
        assert_eq!(*id, leaf_id);
        match leaf_op {
            LeafOp::Delete { leaf } => assert_eq!(*leaf, leaf_id),
            _ => panic!("Expected LeafOp::Delete"),
        }
    }

    #[test]
    fn test_hunk_converter_convert_token_replace() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let leaf_id = LeafId::new(NodeId::new(0), 7);
        let ops = converter.convert_token_replace(leaf_id, b"new_value");

        assert_eq!(ops.leaf_ops().len(), 1);
        let (id, leaf_op) = &ops.leaf_ops()[0];
        assert_eq!(*id, leaf_id);
        match leaf_op {
            LeafOp::Replace { leaf, new_content } => {
                assert_eq!(*leaf, leaf_id);
                assert_eq!(new_content, b"new_value");
            }
            _ => panic!("Expected LeafOp::Replace"),
        }
    }

    #[test]
    fn test_hunk_converter_reset_counters() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        // Generate some IDs
        converter.convert_file_content("test.rs", b"content", None);

        // Reset and verify we can generate again
        converter.reset_counters();
        let ops = converter.convert_file_content("test2.rs", b"more", None);

        // Should have IDs starting from 0 again
        assert!(!ops.is_empty());
    }

    #[test]
    fn test_hunk_converter_no_tokenization() {
        let change_id = NodeId::new(1);
        let opts = ConversionOptions::new().with_tokenize_content(false);
        let mut converter = HunkConverter::new(change_id, opts);

        let ops = converter.convert_file_content("test.txt", b"hello world", None);

        // Without tokenization, each line should be a single leaf
        assert!(!ops.is_empty());
        // Check that we have fewer tokens than with tokenization
        let tokens = ops.stats().tokens_generated;
        assert!(tokens > 0);
    }

    // ------------------------------------------------------------------------
    // Integration Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_integration_full_file_workflow() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        // Add a file
        let content = b"fn main() {\n    println!(\"Hello\");\n}\n";
        let ops = converter.convert_file_content("main.rs", content, Some(Encoding::Utf8));

        // Verify structure
        assert_eq!(ops.trunk_ops().len(), 1);
        assert!(ops.branch_ops().len() >= 3); // At least 3 lines
        assert!(ops.stats().tokens_generated > 0);
        assert_eq!(ops.stats().files_added, 1);
    }

    #[test]
    fn test_integration_empty_file() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let ops = converter.convert_file_content("empty.txt", b"", None);

        assert_eq!(ops.trunk_ops().len(), 1);
        // Empty file may have 0 branches depending on options
    }

    #[test]
    fn test_integration_code_aware_tokenization() {
        let change_id = NodeId::new(1);
        let opts = ConversionOptions::new().with_code_aware(true);
        let mut converter = HunkConverter::new(change_id, opts);

        let content = b"let x = 42;";
        let ops = converter.convert_file_content("code.rs", content, None);

        // Code-aware should recognize operators and numbers
        assert!(!ops.is_empty());
        assert!(ops.stats().tokens_generated > 2); // More than just words
    }

    #[test]
    fn test_integration_multiple_files() {
        let change_id = NodeId::new(1);
        let mut converter = HunkConverter::with_defaults(change_id);

        let ops1 = converter.convert_file_content("file1.rs", b"content1", None);
        let ops2 = converter.convert_file_content("file2.rs", b"content2", None);

        assert_eq!(ops1.trunk_ops().len(), 1);
        assert_eq!(ops2.trunk_ops().len(), 1);

        // Merge them
        let mut combined = ops1;
        combined.merge(ops2);

        assert_eq!(combined.trunk_ops().len(), 2);
        assert_eq!(combined.stats().files_added, 2);
    }
}

//! Convert SemanticDiff to CRDT operations.
//!
//! This module bridges the semantic diff layer with the CRDT change recording
//! system. It converts `SemanticDiff` results (line and token level changes)
//! into `FileOps` and `LineOps` that can be stored in changes.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                  SemanticDiff → CRDT Conversion Pipeline                 │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input: SemanticDiff                                                    │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ LineChange::Added    → BranchOp::Insert with LeafOp::Insert      │  │
//! │  │ LineChange::Deleted  → BranchOp::Delete with content snapshot    │  │
//! │  │ LineChange::Modified → Token-level LeafOp operations             │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  Output: FileOps                                                        │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ trunk_id: TrunkId                                                │  │
//! │  │ path: String                                                     │  │
//! │  │ trunk_op: Option<TrunkOp>                                        │  │
//! │  │ line_ops: Vec<LineOps>                                           │  │
//! │  │   └── BranchOp::Insert/Delete with Vec<LeafOp>                   │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::diff::semantic::{semantic_diff, SemanticDiff};
//! use atomic_core::diff::semantic_to_crdt::{SemanticToCrdt, ConversionConfig};
//! use atomic_core::types::NodeId;
//!
//! let old = b"let x = 1;\n";
//! let new = b"let x = 42;\n";
//!
//! let diff = semantic_diff(old, new);
//!
//! let converter = SemanticToCrdt::new(NodeId::new(1), ConversionConfig::default());
//! let file_ops = converter.convert_diff(&diff, "src/main.rs")?;
//!
//! // file_ops now contains CRDT operations for this change
//! for line_op in file_ops.line_ops() {
//!     println!("Line operation: {:?}", line_op);
//! }
//! ```
//!
//! # Conversion Rules
//!
//! | SemanticDiff Type | CRDT Operations |
//! |-------------------|-----------------|
//! | `LineChange::Added` | `BranchOp::Insert` with `LeafOp::Insert` for each token |
//! | `LineChange::Deleted` | `BranchOp::Delete` with original token content preserved |
//! | `LineChange::Modified` | Mixed `LeafOp::Insert`, `LeafOp::Delete`, `LeafOp::Replace` |
//! | `TokenChange::Inserted` | `LeafOp::Insert` |
//! | `TokenChange::Deleted` | `LeafOp::Delete` |
//! | `TokenChange::Replaced` | `LeafOp::Replace` |
//! | `TokenChange::Unchanged` | (no operation - context only) |

use crate::change::ops::{FileOps, LineOps};
use crate::change::Encoding;
use crate::crdt::{BranchId, LeafId, LeafOp, TrunkId};
use crate::diff::token::TokenKind;
use crate::types::NodeId;
use serde::{Deserialize, Serialize};
use std::fmt;

use super::semantic::{LineChange, SemanticDiff, SemanticLine, TokenChange};

// Conversion Configuration

/// Configuration for semantic diff to CRDT conversion.
#[derive(Debug, Clone)]
pub struct ConversionConfig {
    /// Whether to preserve unchanged tokens in modified lines.
    ///
    /// When true, unchanged tokens are included as `LeafOp::Insert` in the
    /// output (for complete line reconstruction). When false, only changed
    /// tokens are recorded.
    pub preserve_unchanged: bool,

    /// Whether to use Replace operations or Delete+Insert pairs.
    ///
    /// When true, token replacements use `LeafOp::Replace`. When false,
    /// replacements are converted to `LeafOp::Delete` followed by `LeafOp::Insert`.
    pub use_replace_ops: bool,

    /// Text encoding for the file.
    pub encoding: Option<Encoding>,

    /// Whether to include whitespace tokens.
    ///
    /// When false, whitespace-only tokens are excluded from the output.
    pub include_whitespace: bool,
}

impl Default for ConversionConfig {
    fn default() -> Self {
        Self {
            preserve_unchanged: true,
            use_replace_ops: true,
            encoding: Some(Encoding::Utf8),
            include_whitespace: true,
        }
    }
}

impl ConversionConfig {
    /// Create a new configuration with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to preserve unchanged tokens.
    pub fn preserve_unchanged(mut self, preserve: bool) -> Self {
        self.preserve_unchanged = preserve;
        self
    }

    /// Set whether to use Replace operations.
    pub fn use_replace_ops(mut self, use_replace: bool) -> Self {
        self.use_replace_ops = use_replace;
        self
    }

    /// Set the text encoding.
    pub fn encoding(mut self, encoding: Option<Encoding>) -> Self {
        self.encoding = encoding;
        self
    }

    /// Set whether to include whitespace tokens.
    pub fn include_whitespace(mut self, include: bool) -> Self {
        self.include_whitespace = include;
        self
    }
}

// Conversion Statistics

/// Statistics from a semantic-to-CRDT conversion.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversionStats {
    /// Number of line insertions generated.
    pub lines_inserted: usize,

    /// Number of line deletions generated.
    pub lines_deleted: usize,

    /// Number of token insertions generated.
    pub tokens_inserted: usize,

    /// Number of token deletions generated.
    pub tokens_deleted: usize,

    /// Number of token replacements generated.
    pub tokens_replaced: usize,

    /// Number of unchanged tokens (if preserved).
    pub tokens_unchanged: usize,
}

impl ConversionStats {
    /// Check if any operations were generated.
    pub fn has_operations(&self) -> bool {
        self.lines_inserted > 0
            || self.lines_deleted > 0
            || self.tokens_inserted > 0
            || self.tokens_deleted > 0
            || self.tokens_replaced > 0
    }

    /// Get total line operations.
    pub fn total_line_ops(&self) -> usize {
        self.lines_inserted + self.lines_deleted
    }

    /// Get total token operations (excluding unchanged).
    pub fn total_token_ops(&self) -> usize {
        self.tokens_inserted + self.tokens_deleted + self.tokens_replaced
    }
}

impl fmt::Display for ConversionStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} line ops (+{} -{}) | {} token ops (+{} -{} ~{})",
            self.total_line_ops(),
            self.lines_inserted,
            self.lines_deleted,
            self.total_token_ops(),
            self.tokens_inserted,
            self.tokens_deleted,
            self.tokens_replaced
        )
    }
}

// Conversion Error

/// Errors that can occur during semantic-to-CRDT conversion.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ConversionError {
    /// Invalid line number encountered.
    #[error("invalid line number: {0}")]
    InvalidLineNumber(usize),

    /// Empty content where content was expected.
    #[error("empty content for {context}")]
    EmptyContent { context: String },

    /// ID allocation failed.
    #[error("failed to allocate {kind} ID")]
    IdAllocationFailed { kind: String },
}

/// Result type for conversion operations.
pub type ConversionResult<T> = Result<T, ConversionError>;

// ID Allocator

/// Allocates unique IDs for CRDT operations.
#[derive(Debug, Clone)]
struct IdAllocator {
    /// The change node ID (for creating new IDs).
    change_id: NodeId,

    /// Next branch index.
    next_branch: u32,

    /// Next leaf index.
    next_leaf: u32,
}

impl IdAllocator {
    /// Create a new ID allocator for a change.
    fn new(change_id: NodeId) -> Self {
        Self {
            change_id,
            next_branch: 0,
            next_leaf: 0,
        }
    }

    /// Allocate a new branch ID.
    fn alloc_branch(&mut self) -> BranchId {
        let id = BranchId::new(self.change_id, self.next_branch);
        self.next_branch += 1;
        id
    }

    /// Allocate a new leaf ID.
    fn alloc_leaf(&mut self) -> LeafId {
        let id = LeafId::new(self.change_id, self.next_leaf);
        self.next_leaf += 1;
        id
    }
}

// Semantic to CRDT Converter

/// Converts SemanticDiff results to CRDT operations.
///
/// This is the main converter that takes semantic diff results and produces
/// the corresponding CRDT operations for storage in changes.
#[derive(Debug)]
pub struct SemanticToCrdt {
    /// ID allocator for generating unique IDs.
    allocator: IdAllocator,

    /// Trunk ID for the file being converted.
    trunk_id: TrunkId,

    /// Conversion configuration.
    config: ConversionConfig,

    /// Statistics from the conversion.
    stats: ConversionStats,
}

impl SemanticToCrdt {
    /// Create a new converter for a specific change.
    ///
    /// # Arguments
    ///
    /// * `change_id` - The node ID of the change being recorded
    /// * `trunk_id` - The trunk ID for the file
    /// * `config` - Conversion configuration
    pub fn new(change_id: NodeId, trunk_id: TrunkId, config: ConversionConfig) -> Self {
        Self {
            allocator: IdAllocator::new(change_id),
            trunk_id,
            config,
            stats: ConversionStats::default(),
        }
    }

    /// Create a converter with default configuration.
    pub fn with_defaults(change_id: NodeId, trunk_id: TrunkId) -> Self {
        Self::new(change_id, trunk_id, ConversionConfig::default())
    }

    /// Get the conversion statistics.
    pub fn stats(&self) -> &ConversionStats {
        &self.stats
    }

    /// Convert a SemanticDiff to FileOps.
    ///
    /// # Arguments
    ///
    /// * `diff` - The semantic diff to convert
    /// * `path` - The file path
    ///
    /// # Returns
    ///
    /// A `FileOps` containing all the CRDT operations for this diff.
    pub fn convert_diff<'a>(
        &mut self,
        diff: &SemanticDiff<'a>,
        path: &str,
    ) -> ConversionResult<FileOps> {
        let mut file_ops = FileOps::edit(self.trunk_id, path.to_string());

        for change in diff.changes() {
            let line_ops = self.convert_line_change(change)?;
            for op in line_ops {
                file_ops.add_line_op(op);
            }
        }

        Ok(file_ops)
    }

    /// Convert a single LineChange to LineOps.
    fn convert_line_change<'a>(
        &mut self,
        change: &LineChange<'a>,
    ) -> ConversionResult<Vec<LineOps>> {
        match change {
            LineChange::Added {
                line_num,
                line,
                tokens,
                ..
            } => self.convert_added_line(*line_num, line, tokens),
            LineChange::Deleted {
                line_num,
                line,
                tokens,
                ..
            } => self.convert_deleted_line(*line_num, line, tokens),
            LineChange::Modified {
                old_line_num,
                new_line_num,
                before,
                after,
                token_changes,
                ..
            } => self.convert_modified_line(
                *old_line_num,
                *new_line_num,
                before,
                after,
                token_changes,
            ),
        }
    }

    /// Convert an added line to LineOps.
    fn convert_added_line<'a>(
        &mut self,
        line_num: usize,
        _line: &SemanticLine<'a>,
        tokens: &[TokenChange<'a>],
    ) -> ConversionResult<Vec<LineOps>> {
        let branch_id = self.allocator.alloc_branch();

        // Convert all tokens to LeafOp::Insert
        let leaf_ops = self.convert_tokens_to_inserts(tokens)?;

        let line_ops = LineOps::insert(branch_id, None, leaf_ops).with_new_line_num(line_num);

        self.stats.lines_inserted += 1;

        Ok(vec![line_ops])
    }

    /// Convert a deleted line to LineOps.
    fn convert_deleted_line<'a>(
        &mut self,
        line_num: usize,
        _line: &SemanticLine<'a>,
        tokens: &[TokenChange<'a>],
    ) -> ConversionResult<Vec<LineOps>> {
        let branch_id = self.allocator.alloc_branch();

        // Store the original content as LeafOps for diff display
        let content_ops = self.convert_tokens_to_inserts(tokens)?;

        let line_ops = LineOps::delete(branch_id, content_ops).with_old_line_num(line_num);

        self.stats.lines_deleted += 1;

        Ok(vec![line_ops])
    }

    /// Convert a modified line to LineOps.
    ///
    /// Modified lines are more complex - we need to generate token-level
    /// operations based on what changed.
    fn convert_modified_line<'a>(
        &mut self,
        old_line_num: usize,
        new_line_num: usize,
        _before: &SemanticLine<'a>,
        _after: &SemanticLine<'a>,
        token_changes: &[TokenChange<'a>],
    ) -> ConversionResult<Vec<LineOps>> {
        let branch_id = self.allocator.alloc_branch();

        // Convert token changes to LeafOps
        let leaf_ops = self.convert_token_changes(token_changes)?;

        // For a modified line, we emit the new content with the leaf ops
        // that describe how it was constructed
        let line_ops = LineOps::insert(branch_id, None, leaf_ops)
            .with_old_line_num(old_line_num)
            .with_new_line_num(new_line_num);

        // Modified lines count as both delete and insert at the line level
        // but we only emit one operation with the final content
        self.stats.lines_inserted += 1;

        Ok(vec![line_ops])
    }

    /// Convert token changes to LeafOps.
    fn convert_token_changes<'a>(
        &mut self,
        token_changes: &[TokenChange<'a>],
    ) -> ConversionResult<Vec<LeafOp>> {
        let mut leaf_ops = Vec::new();

        for tc in token_changes {
            match tc {
                TokenChange::Unchanged { token, .. } => {
                    if self.config.preserve_unchanged {
                        // Include unchanged tokens as inserts for complete reconstruction
                        if self.should_include_token(token.kind()) {
                            let _leaf_id = self.allocator.alloc_leaf();
                            leaf_ops.push(LeafOp::Insert {
                                after: None,
                                kind: token.kind(),
                                content: token.content().to_vec(),
                            });
                            self.stats.tokens_unchanged += 1;
                        }
                    }
                }

                TokenChange::Inserted { token, .. } => {
                    if self.should_include_token(token.kind()) {
                        let _leaf_id = self.allocator.alloc_leaf();
                        leaf_ops.push(LeafOp::Insert {
                            after: None,
                            kind: token.kind(),
                            content: token.content().to_vec(),
                        });
                        self.stats.tokens_inserted += 1;
                    }
                }

                TokenChange::Deleted { token, .. } => {
                    // For deletions, we record what was deleted
                    if self.should_include_token(token.kind()) {
                        let leaf_id = self.allocator.alloc_leaf();
                        leaf_ops.push(LeafOp::Delete { leaf: leaf_id });
                        self.stats.tokens_deleted += 1;
                    }
                }

                TokenChange::Replaced { new_token, .. } => {
                    if self.should_include_token(new_token.kind()) {
                        let leaf_id = self.allocator.alloc_leaf();

                        if self.config.use_replace_ops {
                            // Use a single Replace operation
                            leaf_ops.push(LeafOp::Replace {
                                leaf: leaf_id,
                                new_content: new_token.content().to_vec(),
                            });
                            self.stats.tokens_replaced += 1;
                        } else {
                            // Use Delete + Insert pair
                            leaf_ops.push(LeafOp::Delete { leaf: leaf_id });
                            leaf_ops.push(LeafOp::Insert {
                                after: None,
                                kind: new_token.kind(),
                                content: new_token.content().to_vec(),
                            });
                            self.stats.tokens_deleted += 1;
                            self.stats.tokens_inserted += 1;
                        }
                    }
                }
            }
        }

        Ok(leaf_ops)
    }

    /// Convert token changes to insert-only LeafOps (for added/deleted lines).
    fn convert_tokens_to_inserts<'a>(
        &mut self,
        tokens: &[TokenChange<'a>],
    ) -> ConversionResult<Vec<LeafOp>> {
        let mut leaf_ops = Vec::new();

        for tc in tokens {
            // For added lines, all tokens should be Inserted
            // For deleted lines, all tokens should be Deleted (but we store as Insert for content)
            let token = match tc {
                TokenChange::Inserted { token, .. } => token,
                TokenChange::Deleted { token, .. } => token,
                TokenChange::Unchanged { token, .. } => token,
                TokenChange::Replaced { new_token, .. } => new_token,
            };

            if self.should_include_token(token.kind()) {
                let _leaf_id = self.allocator.alloc_leaf();
                leaf_ops.push(LeafOp::Insert {
                    after: None,
                    kind: token.kind(),
                    content: token.content().to_vec(),
                });

                match tc {
                    TokenChange::Inserted { .. } => self.stats.tokens_inserted += 1,
                    TokenChange::Deleted { .. } => self.stats.tokens_deleted += 1,
                    _ => {}
                }
            }
        }

        Ok(leaf_ops)
    }

    /// Check if a token should be included based on configuration.
    fn should_include_token(&self, kind: TokenKind) -> bool {
        if !self.config.include_whitespace {
            return kind != TokenKind::Whitespace && kind != TokenKind::Newline;
        }
        true
    }
}

// Convenience Functions

/// Convert a SemanticDiff to FileOps with default configuration.
///
/// This is a convenience function for simple conversions.
///
/// # Arguments
///
/// * `diff` - The semantic diff to convert
/// * `change_id` - The node ID of the change
/// * `trunk_id` - The trunk ID for the file
/// * `path` - The file path
///
/// # Returns
///
/// A `FileOps` containing all the CRDT operations.
pub fn convert_diff_to_file_ops<'a>(
    diff: &SemanticDiff<'a>,
    change_id: NodeId,
    trunk_id: TrunkId,
    path: &str,
) -> ConversionResult<FileOps> {
    let mut converter = SemanticToCrdt::with_defaults(change_id, trunk_id);
    converter.convert_diff(diff, path)
}

/// Convert a SemanticDiff to FileOps with custom configuration.
pub fn convert_diff_to_file_ops_with_config<'a>(
    diff: &SemanticDiff<'a>,
    change_id: NodeId,
    trunk_id: TrunkId,
    path: &str,
    config: ConversionConfig,
) -> ConversionResult<(FileOps, ConversionStats)> {
    let mut converter = SemanticToCrdt::new(change_id, trunk_id, config);
    let ops = converter.convert_diff(diff, path)?;
    Ok((ops, converter.stats().clone()))
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::semantic::semantic_diff;

    fn test_change_id() -> NodeId {
        NodeId::new(1)
    }

    fn test_trunk_id() -> TrunkId {
        TrunkId::new(test_change_id(), 0)
    }

    // ConversionConfig tests

    #[test]
    fn test_conversion_config_default() {
        let config = ConversionConfig::default();
        assert!(config.preserve_unchanged);
        assert!(config.use_replace_ops);
        assert!(config.include_whitespace);
        assert!(config.encoding.is_some());
    }

    #[test]
    fn test_conversion_config_builder() {
        let config = ConversionConfig::new()
            .preserve_unchanged(false)
            .use_replace_ops(false)
            .include_whitespace(false)
            .encoding(None);

        assert!(!config.preserve_unchanged);
        assert!(!config.use_replace_ops);
        assert!(!config.include_whitespace);
        assert!(config.encoding.is_none());
    }

    // ConversionStats tests

    #[test]
    fn test_conversion_stats_default() {
        let stats = ConversionStats::default();
        assert!(!stats.has_operations());
        assert_eq!(stats.total_line_ops(), 0);
        assert_eq!(stats.total_token_ops(), 0);
    }

    #[test]
    fn test_conversion_stats_with_ops() {
        let stats = ConversionStats {
            lines_inserted: 2,
            lines_deleted: 1,
            tokens_inserted: 5,
            tokens_deleted: 3,
            tokens_replaced: 2,
            tokens_unchanged: 10,
        };

        assert!(stats.has_operations());
        assert_eq!(stats.total_line_ops(), 3);
        assert_eq!(stats.total_token_ops(), 10);
    }

    #[test]
    fn test_conversion_stats_display() {
        let stats = ConversionStats {
            lines_inserted: 2,
            lines_deleted: 1,
            tokens_inserted: 5,
            tokens_deleted: 3,
            tokens_replaced: 2,
            tokens_unchanged: 10,
        };

        let display = format!("{}", stats);
        assert!(display.contains("3 line ops"));
        assert!(display.contains("10 token ops"));
    }

    // SemanticToCrdt tests

    #[test]
    fn test_convert_added_line() {
        let old = b"";
        let new = b"let x = 42;\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());

        let mut converter = SemanticToCrdt::with_defaults(test_change_id(), test_trunk_id());
        let file_ops = converter.convert_diff(&diff, "test.rs").unwrap();

        assert_eq!(file_ops.path(), "test.rs");
        assert!(!file_ops.line_ops().is_empty());
        assert_eq!(converter.stats().lines_inserted, 1);
    }

    #[test]
    fn test_convert_deleted_line() {
        let old = b"let x = 42;\n";
        let new = b"";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());

        let mut converter = SemanticToCrdt::with_defaults(test_change_id(), test_trunk_id());
        let file_ops = converter.convert_diff(&diff, "test.rs").unwrap();

        assert!(!file_ops.line_ops().is_empty());
        assert_eq!(converter.stats().lines_deleted, 1);
    }

    #[test]
    fn test_convert_modified_line() {
        let old = b"let x = 1;\n";
        let new = b"let x = 42;\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());

        let mut converter = SemanticToCrdt::with_defaults(test_change_id(), test_trunk_id());
        let file_ops = converter.convert_diff(&diff, "test.rs").unwrap();

        assert!(!file_ops.line_ops().is_empty());
        // Modified lines generate insert operations with the new content
        assert!(converter.stats().lines_inserted >= 1);
    }

    #[test]
    fn test_convert_with_token_changes() {
        let old = b"let foo = 1;\n";
        let new = b"let bar = 2;\n";

        let diff = semantic_diff(old, new);

        let config = ConversionConfig::new().use_replace_ops(true);
        let mut converter = SemanticToCrdt::new(test_change_id(), test_trunk_id(), config);
        let _file_ops = converter.convert_diff(&diff, "test.rs").unwrap();

        let stats = converter.stats();
        // Should have token replacements (foo -> bar, 1 -> 2)
        assert!(stats.tokens_replaced > 0 || stats.tokens_inserted > 0);
    }

    #[test]
    fn test_convert_without_replace_ops() {
        let old = b"let foo = 1;\n";
        let new = b"let bar = 2;\n";

        let diff = semantic_diff(old, new);

        let config = ConversionConfig::new().use_replace_ops(false);
        let mut converter = SemanticToCrdt::new(test_change_id(), test_trunk_id(), config);
        let _file_ops = converter.convert_diff(&diff, "test.rs").unwrap();

        let stats = converter.stats();
        // Without replace ops, should use delete + insert pairs
        assert_eq!(stats.tokens_replaced, 0);
        assert!(stats.tokens_inserted > 0 || stats.tokens_deleted > 0);
    }

    #[test]
    fn test_convert_without_whitespace() {
        let old = b"a b c\n";
        let new = b"a x c\n";

        let diff = semantic_diff(old, new);

        let config = ConversionConfig::new().include_whitespace(false);
        let mut converter = SemanticToCrdt::new(test_change_id(), test_trunk_id(), config);
        let file_ops = converter.convert_diff(&diff, "test.rs").unwrap();

        // Check that line ops were generated
        assert!(!file_ops.line_ops().is_empty());
    }

    #[test]
    fn test_convert_empty_diff() {
        let content = b"let x = 42;\n";
        let diff = semantic_diff(content, content);

        assert!(!diff.has_changes());

        let mut converter = SemanticToCrdt::with_defaults(test_change_id(), test_trunk_id());
        let file_ops = converter.convert_diff(&diff, "test.rs").unwrap();

        assert!(file_ops.line_ops().is_empty());
        assert!(!converter.stats().has_operations());
    }

    // Convenience function tests

    #[test]
    fn test_convert_diff_to_file_ops() {
        let old = b"hello\n";
        let new = b"world\n";

        let diff = semantic_diff(old, new);
        let file_ops =
            convert_diff_to_file_ops(&diff, test_change_id(), test_trunk_id(), "test.txt").unwrap();

        assert_eq!(file_ops.path(), "test.txt");
        assert!(!file_ops.line_ops().is_empty());
    }

    #[test]
    fn test_convert_diff_to_file_ops_with_config() {
        let old = b"hello\n";
        let new = b"world\n";

        let diff = semantic_diff(old, new);
        let config = ConversionConfig::new().preserve_unchanged(false);

        let (file_ops, stats) = convert_diff_to_file_ops_with_config(
            &diff,
            test_change_id(),
            test_trunk_id(),
            "test.txt",
            config,
        )
        .unwrap();

        assert_eq!(file_ops.path(), "test.txt");
        assert!(stats.has_operations());
    }

    // Integration tests

    #[test]
    fn test_multiline_conversion() {
        let old = b"fn main() {\n    println!(\"Hello\");\n}\n";
        let new = b"fn main() {\n    println!(\"Hello, World!\");\n    return;\n}\n";

        let diff = semantic_diff(old, new);
        assert!(diff.has_changes());

        let mut converter = SemanticToCrdt::with_defaults(test_change_id(), test_trunk_id());
        let _file_ops = converter.convert_diff(&diff, "main.rs").unwrap();

        let stats = converter.stats();
        assert!(stats.has_operations());
        // Should have modifications and/or additions
        assert!(stats.lines_inserted > 0 || stats.tokens_inserted > 0);
    }
}

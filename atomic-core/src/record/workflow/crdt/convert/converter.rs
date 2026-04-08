//! HunkConverter for transforming hunks to CRDT operations.

use crate::change::Encoding;
use crate::crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId, TrunkOp};
use crate::diff::token::TokenKind;
use crate::types::NodeId;

use super::super::tokenize::ContentTokenizer;
use super::types::{ConversionOptions, ConvertedOps};

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

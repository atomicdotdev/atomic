//! Branch (line) operations for the CRDT change builder.
//!
//! This module contains [`LineOps`] which represents operations on a single
//! line, and the branch-related methods on [`CrdtChangeBuilder`].

use crate::crdt::{BranchId, BranchOp, LeafId, LeafOp};

use super::super::tokenize::{ContentTokenizer, TokenizeOptions};
use super::leaf::TokenOps;
use super::CrdtChangeBuilder;

use super::super::line_ops::{LineChange, LineChangeKind};

// ============================================================================
// LINE OPS
// ============================================================================

/// Operations for a single line.
///
/// Contains the branch operation and any associated leaf operations.
#[derive(Debug, Clone)]
pub struct LineOps {
    /// The branch ID for this line.
    branch_id: BranchId,

    /// The operation to perform on the branch.
    operation: BranchOp,

    /// Token operations within this line (for inserts/modifications).
    token_ops: Vec<TokenOps>,

    /// The line number in the old file (for deletes/modifies).
    old_line_num: Option<usize>,

    /// The line number in the new file (for inserts/modifies).
    new_line_num: Option<usize>,
}

impl LineOps {
    /// Creates a new line operation.
    pub fn new(branch_id: BranchId, operation: BranchOp) -> Self {
        Self {
            branch_id,
            operation,
            token_ops: Vec::new(),
            old_line_num: None,
            new_line_num: None,
        }
    }

    /// Creates a line insert operation with leaf operations.
    pub fn insert(branch_id: BranchId, after: Option<BranchId>, leaf_ops: Vec<LeafOp>) -> Self {
        Self {
            branch_id,
            operation: BranchOp::Insert {
                after,
                content: leaf_ops,
            },
            token_ops: Vec::new(),
            old_line_num: None,
            new_line_num: None,
        }
    }

    /// Creates a line delete operation with original content.
    ///
    /// # Arguments
    ///
    /// * `branch_id` - The branch (line) being deleted
    /// * `content` - The original content of the line (for diff display)
    pub fn delete(branch_id: BranchId, content: Vec<LeafOp>) -> Self {
        Self {
            branch_id,
            operation: BranchOp::Delete {
                branch: branch_id,
                content,
            },
            token_ops: Vec::new(),
            old_line_num: None,
            new_line_num: None,
        }
    }

    /// Creates a line delete operation without content.
    ///
    /// Use this when the original content is not available.
    pub fn delete_empty(branch_id: BranchId) -> Self {
        Self {
            branch_id,
            operation: BranchOp::Delete {
                branch: branch_id,
                content: Vec::new(),
            },
            token_ops: Vec::new(),
            old_line_num: None,
            new_line_num: None,
        }
    }

    /// Set the old line number.
    pub fn with_old_line_num(mut self, line_num: usize) -> Self {
        self.old_line_num = Some(line_num);
        self
    }

    /// Set the new line number.
    pub fn with_new_line_num(mut self, line_num: usize) -> Self {
        self.new_line_num = Some(line_num);
        self
    }

    /// Creates a line modify operation (old content → new content).
    ///
    /// This is the canonical representation for a modified line. Carries
    /// both old and new content so every consumer can render word-level
    /// diffs without heuristic re-pairing.
    pub fn modify(branch_id: BranchId, old_content: Vec<LeafOp>, new_content: Vec<LeafOp>) -> Self {
        Self {
            branch_id,
            operation: BranchOp::Modify {
                branch: branch_id,
                old_content,
                new_content,
            },
            token_ops: Vec::new(),
            old_line_num: None,
            new_line_num: None,
        }
    }

    /// Returns `true` if this is a delete operation.
    #[inline]
    pub fn is_delete(&self) -> bool {
        matches!(self.operation, BranchOp::Delete { .. })
    }

    /// Returns `true` if this is an insert operation.
    #[inline]
    pub fn is_insert(&self) -> bool {
        matches!(self.operation, BranchOp::Insert { .. })
    }

    /// Returns `true` if this is a modify operation.
    #[inline]
    pub fn is_modify(&self) -> bool {
        matches!(self.operation, BranchOp::Modify { .. })
    }

    /// Get the old line number.
    #[inline]
    pub fn old_line_num(&self) -> Option<usize> {
        self.old_line_num
    }

    /// Get the new line number.
    #[inline]
    pub fn new_line_num(&self) -> Option<usize> {
        self.new_line_num
    }

    /// Returns the branch ID.
    #[inline]
    pub fn branch_id(&self) -> BranchId {
        self.branch_id
    }

    /// Returns the branch operation.
    #[inline]
    pub fn operation(&self) -> &BranchOp {
        &self.operation
    }

    /// Returns the branch operation for in-place mutation.
    ///
    /// Used by the consolidation pass in `build_crdt_ops_for_modified_file`
    /// to rewrite an `Insert`'s `after` reference when its target placeholder
    /// gets promoted to a `Modify` on an existing branch.
    #[inline]
    pub fn operation_mut(&mut self) -> &mut BranchOp {
        &mut self.operation
    }

    /// Returns the token operations.
    #[inline]
    pub fn token_ops(&self) -> &[TokenOps] {
        &self.token_ops
    }

    /// Adds a token operation.
    pub fn add_token_op(&mut self, op: TokenOps) {
        self.token_ops.push(op);
    }

    /// Consumes and returns the branch operation.
    pub fn into_operation(self) -> BranchOp {
        self.operation
    }
}

// ============================================================================
// BUILDER BRANCH METHODS
// ============================================================================

impl CrdtChangeBuilder {
    /// Adds a new line to a file and returns its branch ID.
    ///
    /// # Arguments
    ///
    /// * `trunk_id` - The file to add the line to
    /// * `after` - The branch to insert after (None for start of file)
    pub fn add_line(
        &mut self,
        trunk_id: crate::crdt::TrunkId,
        after: Option<BranchId>,
    ) -> BranchId {
        let branch_id = self.alloc_branch_id();

        let line_op = LineOps::insert(branch_id, after, Vec::new());

        if let Some(&file_idx) = self.trunk_index.get(&trunk_id) {
            let line_idx = self.file_ops[file_idx].line_ops.len();
            self.branch_index.insert(branch_id, (file_idx, line_idx));
            self.file_ops[file_idx].add_line_op(line_op);
        }

        self.stats.lines_added += 1;
        branch_id
    }

    /// Adds a line with content, tokenizing into leaves.
    pub fn add_line_with_content(
        &mut self,
        trunk_id: crate::crdt::TrunkId,
        after: Option<BranchId>,
        content: &[u8],
    ) -> BranchId {
        let branch_id = self.alloc_branch_id();

        // Tokenize the line
        let opts = TokenizeOptions::default();
        let line = ContentTokenizer::tokenize_line(content, &opts);

        // Generate leaf operations
        let mut leaf_ops = Vec::new();
        let mut prev_leaf: Option<LeafId> = None;

        for token in line.tokens() {
            let leaf_id = self.alloc_leaf_id();
            let _ = self.append_content(token.content());

            leaf_ops.push(LeafOp::Insert {
                after: prev_leaf,
                kind: token.kind(),
                content: token.content().to_vec(),
            });

            self.stats.tokens_added += 1;
            prev_leaf = Some(leaf_id);
        }

        let line_op = LineOps::insert(branch_id, after, leaf_ops);

        if let Some(&file_idx) = self.trunk_index.get(&trunk_id) {
            let line_idx = self.file_ops[file_idx].line_ops.len();
            self.branch_index.insert(branch_id, (file_idx, line_idx));
            self.file_ops[file_idx].add_line_op(line_op);
        }

        self.stats.lines_added += 1;
        branch_id
    }

    /// Marks a line for deletion.
    ///
    /// Note: This creates a delete without content. For deletes with content
    /// (for diff display), use the `LineOps::delete()` constructor directly.
    pub fn delete_line(&mut self, branch_id: BranchId) {
        let line_op = LineOps::delete_empty(branch_id);

        // Find which file this branch belongs to
        if let Some(&(file_idx, _)) = self.branch_index.get(&branch_id) {
            self.file_ops[file_idx].add_line_op(line_op);
        } else {
            // Branch not in index - create a placeholder file op
            use super::trunk::FileOps;
            use crate::crdt::TrunkId;
            use crate::types::NodeId;

            let file_op = FileOps::new(TrunkId::new(NodeId::new(0), 0), String::new(), None);
            let mut file_op = file_op;
            file_op.add_line_op(line_op);
            self.file_ops.push(file_op);
        }

        self.stats.lines_deleted += 1;
    }

    /// Applies a line change from the analyzer.
    ///
    /// This is the integration point between the `LineAnalyzer` and the builder.
    pub fn apply_line_change(&mut self, trunk_id: crate::crdt::TrunkId, change: &LineChange) {
        match change.kind() {
            LineChangeKind::Equal => {
                // No operation needed for unchanged lines
            }
            LineChangeKind::Insert => {
                if let Some(content) = change.new_content() {
                    self.add_line_with_content(trunk_id, self.last_branch_id, content);
                }
            }
            LineChangeKind::Delete => {
                if let Some(branch_id) = change.existing_branch() {
                    self.delete_line(branch_id);
                }
                // Note: If no existing_branch, we'd need to look it up
                self.stats.lines_deleted += 1;
            }
            LineChangeKind::Modify => {
                // For modifications, we delete the old and insert the new
                if let Some(branch_id) = change.existing_branch() {
                    self.delete_line(branch_id);
                }
                if let Some(content) = change.new_content() {
                    self.add_line_with_content(trunk_id, self.last_branch_id, content);
                }
                self.stats.lines_modified += 1;
            }
            LineChangeKind::Move => {
                // Moves are handled as delete + insert with the same content
                // The CRDT model would track this differently in a full implementation
            }
        }
    }
}

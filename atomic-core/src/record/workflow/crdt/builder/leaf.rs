//! Token (leaf) operations for the CRDT change builder.
//!
//! This module contains the [`TokenOps`] type representing leaf-level
//! operations, and the leaf-related methods on [`CrdtChangeBuilder`].

use crate::crdt::LeafId;
use crate::crdt::LeafOp;
use crate::diff::token::TokenKind;

use super::CrdtChangeBuilder;

// ============================================================================
// TOKEN OPS
// ============================================================================

/// Operations for tokens within a line.
///
/// Represents the leaf-level operations that modify tokens.
#[derive(Debug, Clone)]
pub struct TokenOps {
    /// The leaf ID for this token.
    leaf_id: LeafId,

    /// The operation to perform.
    operation: LeafOp,
}

impl TokenOps {
    /// Creates a new token operation.
    pub fn new(leaf_id: LeafId, operation: LeafOp) -> Self {
        Self { leaf_id, operation }
    }

    /// Returns the leaf ID.
    #[inline]
    pub fn leaf_id(&self) -> LeafId {
        self.leaf_id
    }

    /// Returns the operation.
    #[inline]
    pub fn operation(&self) -> &LeafOp {
        &self.operation
    }

    /// Consumes and returns the operation.
    pub fn into_operation(self) -> LeafOp {
        self.operation
    }
}

// ============================================================================
// BUILDER LEAF METHODS
// ============================================================================

impl CrdtChangeBuilder {
    /// Adds a token to a line.
    ///
    /// # Arguments
    ///
    /// * `branch_id` - The line to add the token to
    /// * `after` - The leaf to insert after (None for start of line)
    /// * `kind` - The token kind
    /// * `content` - The token content
    pub fn add_token(
        &mut self,
        _branch_id: crate::crdt::BranchId,
        _after: Option<LeafId>,
        _kind: TokenKind,
        content: &[u8],
    ) -> LeafId {
        let leaf_id = self.alloc_leaf_id();
        let _ = self.append_content(content);

        // Note: In a full implementation, we would add this to the branch's leaf ops
        // For now, we just track the allocation and stats
        self.stats.tokens_added += 1;

        leaf_id
    }

    /// Marks a token for deletion.
    pub fn delete_token(&mut self, _leaf_id: LeafId) {
        // Track the deletion
        self.stats.tokens_deleted += 1;
    }

    /// Replaces a token's content (preserving its ID for blame).
    pub fn replace_token(&mut self, _leaf_id: LeafId, new_content: &[u8]) {
        let _ = self.append_content(new_content);
        self.stats.tokens_replaced += 1;
    }
}

//! Builder for accumulating CRDT operations during change recording.
//!
//! This module provides the [`CrdtChangeBuilder`] which accumulates CRDT
//! operations (TrunkOp, BranchOp, LeafOp) as files are recorded.

mod branch;
mod leaf;
mod trunk;

#[cfg(test)]
mod tests;

use crate::crdt::{BranchId, BranchOp, LeafId, TrunkId, TrunkOp};
use crate::types::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

pub use branch::LineOps;
pub use leaf::TokenOps;
pub use trunk::FileOps;

// ============================================================================
// BUILD ERROR
// ============================================================================

/// Errors that can occur during CRDT change building.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrdtBuildError {
    /// Attempted to add content to an unknown trunk.
    UnknownTrunk { trunk_id: TrunkId },

    /// Attempted to add content to an unknown branch.
    UnknownBranch { branch_id: BranchId },

    /// The builder is in an invalid state for the requested operation.
    InvalidState { description: String },

    /// A referenced ID does not exist.
    InvalidReference { description: String },

    /// Content validation failed.
    ValidationFailed { description: String },
}

impl fmt::Display for CrdtBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrdtBuildError::UnknownTrunk { trunk_id } => {
                write!(f, "unknown trunk: {:?}", trunk_id)
            }
            CrdtBuildError::UnknownBranch { branch_id } => {
                write!(f, "unknown branch: {:?}", branch_id)
            }
            CrdtBuildError::InvalidState { description } => {
                write!(f, "invalid builder state: {}", description)
            }
            CrdtBuildError::InvalidReference { description } => {
                write!(f, "invalid reference: {}", description)
            }
            CrdtBuildError::ValidationFailed { description } => {
                write!(f, "validation failed: {}", description)
            }
        }
    }
}

impl std::error::Error for CrdtBuildError {}

/// Result type for build operations.
pub type CrdtBuildResult<T> = Result<T, CrdtBuildError>;

// ============================================================================
// BUILD STATS
// ============================================================================

/// Statistics about the CRDT change building process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrdtBuildStats {
    /// Number of files added.
    pub files_added: usize,
    /// Number of files deleted.
    pub files_deleted: usize,
    /// Number of files moved.
    pub files_moved: usize,
    /// Number of files undeleted.
    pub files_undeleted: usize,
    /// Number of lines added.
    pub lines_added: usize,
    /// Number of lines deleted.
    pub lines_deleted: usize,
    /// Number of lines modified.
    pub lines_modified: usize,
    /// Number of tokens added.
    pub tokens_added: usize,
    /// Number of tokens deleted.
    pub tokens_deleted: usize,
    /// Number of tokens replaced.
    pub tokens_replaced: usize,
    /// Total content bytes accumulated.
    pub content_bytes: usize,
}

impl CrdtBuildStats {
    /// Creates new empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the total number of file operations.
    pub fn total_file_ops(&self) -> usize {
        self.files_added + self.files_deleted + self.files_moved + self.files_undeleted
    }

    /// Returns the total number of line operations.
    pub fn total_line_ops(&self) -> usize {
        self.lines_added + self.lines_deleted + self.lines_modified
    }

    /// Returns the total number of token operations.
    pub fn total_token_ops(&self) -> usize {
        self.tokens_added + self.tokens_deleted + self.tokens_replaced
    }

    /// Returns the total number of all operations.
    pub fn total_ops(&self) -> usize {
        self.total_file_ops() + self.total_line_ops() + self.total_token_ops()
    }

    /// Returns true if any changes were recorded.
    pub fn has_changes(&self) -> bool {
        self.total_ops() > 0
    }

    /// Merges another stats instance into this one.
    pub fn merge(&mut self, other: &CrdtBuildStats) {
        self.files_added += other.files_added;
        self.files_deleted += other.files_deleted;
        self.files_moved += other.files_moved;
        self.files_undeleted += other.files_undeleted;
        self.lines_added += other.lines_added;
        self.lines_deleted += other.lines_deleted;
        self.lines_modified += other.lines_modified;
        self.tokens_added += other.tokens_added;
        self.tokens_deleted += other.tokens_deleted;
        self.tokens_replaced += other.tokens_replaced;
        self.content_bytes += other.content_bytes;
    }
}

impl fmt::Display for CrdtBuildStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "files: +{} -{} ~{}, lines: +{} -{} ~{}, tokens: +{} -{} ~{}, {} bytes",
            self.files_added,
            self.files_deleted,
            self.files_moved,
            self.lines_added,
            self.lines_deleted,
            self.lines_modified,
            self.tokens_added,
            self.tokens_deleted,
            self.tokens_replaced,
            self.content_bytes
        )
    }
}

// ============================================================================
// CRDT CHANGE RESULT
// ============================================================================

/// The result of building CRDT operations for a change.
#[derive(Debug, Clone)]
pub struct CrdtChangeResult {
    /// Operations organized by file.
    file_ops: Vec<FileOps>,
    /// Accumulated content for all leaves.
    content: Vec<u8>,
    /// Build statistics.
    stats: CrdtBuildStats,
}

impl CrdtChangeResult {
    /// Creates an empty result.
    pub fn new() -> Self {
        Self {
            file_ops: Vec::new(),
            content: Vec::new(),
            stats: CrdtBuildStats::new(),
        }
    }

    #[inline]
    pub fn file_ops(&self) -> &[FileOps] {
        &self.file_ops
    }

    #[inline]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    #[inline]
    pub fn stats(&self) -> &CrdtBuildStats {
        &self.stats
    }

    pub fn is_empty(&self) -> bool {
        self.file_ops.is_empty() && self.content.is_empty()
    }

    pub fn file_count(&self) -> usize {
        self.file_ops.len()
    }

    pub fn trunk_ops(&self) -> Vec<&TrunkOp> {
        self.file_ops.iter().filter_map(|f| f.trunk_op()).collect()
    }

    pub fn branch_ops(&self) -> Vec<(BranchId, &BranchOp)> {
        self.file_ops
            .iter()
            .flat_map(|f| f.line_ops().iter().map(|l| (l.branch_id(), l.operation())))
            .collect()
    }

    /// Consumes the result and returns its parts.
    pub fn into_parts(self) -> (Vec<FileOps>, Vec<u8>, CrdtBuildStats) {
        (self.file_ops, self.content, self.stats)
    }
}

impl Default for CrdtChangeResult {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// CRDT CHANGE BUILDER
// ============================================================================

/// Builder for accumulating CRDT operations during change recording.
///
/// Provides a high-level API for adding files, lines, and tokens while
/// managing ID allocation and content accumulation internally.
#[derive(Debug)]
pub struct CrdtChangeBuilder {
    /// The change ID for generating CRDT IDs.
    pub(crate) change_id: NodeId,
    /// Counter for trunk IDs.
    pub(crate) next_trunk_idx: u32,
    /// Counter for branch IDs.
    pub(crate) next_branch_idx: u32,
    /// Counter for leaf IDs.
    pub(crate) next_leaf_idx: u32,
    /// File operations being built.
    pub(crate) file_ops: Vec<FileOps>,
    /// Map from trunk ID to index in file_ops.
    pub(crate) trunk_index: HashMap<TrunkId, usize>,
    /// Map from branch ID to (file_index, line_index).
    pub(crate) branch_index: HashMap<BranchId, (usize, usize)>,
    /// Accumulated content.
    pub(crate) content: Vec<u8>,
    /// Build statistics.
    pub(crate) stats: CrdtBuildStats,
    /// The last allocated branch ID (for chaining).
    pub(crate) last_branch_id: Option<BranchId>,
    /// The last allocated leaf ID (for chaining).
    pub(crate) last_leaf_id: Option<LeafId>,
}

impl CrdtChangeBuilder {
    /// Creates a new builder for the given change.
    pub fn new(change_id: NodeId) -> Self {
        Self {
            change_id,
            next_trunk_idx: 0,
            next_branch_idx: 0,
            next_leaf_idx: 0,
            file_ops: Vec::new(),
            trunk_index: HashMap::new(),
            branch_index: HashMap::new(),
            content: Vec::new(),
            stats: CrdtBuildStats::new(),
            last_branch_id: None,
            last_leaf_id: None,
        }
    }

    /// Returns the change ID.
    #[inline]
    pub fn change_id(&self) -> NodeId {
        self.change_id
    }

    /// Allocates a new trunk ID.
    pub(crate) fn alloc_trunk_id(&mut self) -> TrunkId {
        let id = TrunkId::new(self.change_id, self.next_trunk_idx);
        self.next_trunk_idx += 1;
        id
    }

    /// Allocates a new branch ID.
    pub(crate) fn alloc_branch_id(&mut self) -> BranchId {
        let id = BranchId::new(self.change_id, self.next_branch_idx);
        self.next_branch_idx += 1;
        self.last_branch_id = Some(id);
        id
    }

    /// Allocates a new leaf ID.
    pub(crate) fn alloc_leaf_id(&mut self) -> LeafId {
        let id = LeafId::new(self.change_id, self.next_leaf_idx);
        self.next_leaf_idx += 1;
        self.last_leaf_id = Some(id);
        id
    }

    /// Appends content to the buffer and returns the byte range.
    pub(crate) fn append_content(&mut self, data: &[u8]) -> std::ops::Range<usize> {
        let start = self.content.len();
        self.content.extend_from_slice(data);
        let end = self.content.len();
        self.stats.content_bytes += data.len();
        start..end
    }

    /// Merges another builder's results into this one.
    pub fn merge(&mut self, other: CrdtChangeBuilder) {
        for file_op in other.file_ops {
            self.file_ops.push(file_op);
        }
        self.content.extend(other.content);
        self.stats.merge(&other.stats);
    }

    /// Finishes building and returns the result.
    pub fn finish(self) -> CrdtChangeResult {
        CrdtChangeResult {
            file_ops: self.file_ops,
            content: self.content,
            stats: self.stats,
        }
    }

    /// Returns the current statistics.
    pub fn current_stats(&self) -> &CrdtBuildStats {
        &self.stats
    }

    /// Returns true if any operations have been recorded.
    pub fn has_operations(&self) -> bool {
        !self.file_ops.is_empty() || self.stats.has_changes()
    }

    /// Returns the last allocated branch ID.
    pub fn last_branch(&self) -> Option<BranchId> {
        self.last_branch_id
    }

    /// Returns the last allocated leaf ID.
    pub fn last_leaf(&self) -> Option<LeafId> {
        self.last_leaf_id
    }
}

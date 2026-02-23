//! Builder for accumulating CRDT operations during change recording.
//!
//! This module provides types for building CRDT operations during recording,
//! and functions to convert them to the serializable `change::ops` types.
//!
//! This module provides the [`CrdtChangeBuilder`] which accumulates CRDT
//! operations (TrunkOp, BranchOp, LeafOp) as files are recorded. It serves
//! as the main integration point between the record workflow and the
//! hierarchical CRDT model.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                      CRDT Change Builder Pipeline                        │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input: Recording Operations                                            │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ add_file("main.rs", content, encoding)                           │  │
//! │  │ add_line(trunk_id, after, content)                               │  │
//! │  │ add_token(branch_id, after, kind, content)                       │  │
//! │  │ delete_file(trunk_id)                                            │  │
//! │  │ delete_line(branch_id)                                           │  │
//! │  │ apply_line_change(trunk_id, change)                              │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  CrdtChangeBuilder                                                      │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ • Allocates unique IDs for all CRDT entities                     │  │
//! │  │ • Accumulates operations in correct order                        │  │
//! │  │ • Manages content buffer for leaf data                           │  │
//! │  │ • Tracks statistics and validation state                         │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  Output: CrdtChangeResult                                               │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ file_ops: Vec<FileOps>     (per-file operations)                 │  │
//! │  │ content: Vec<u8>           (accumulated content)                 │  │
//! │  │ stats: CrdtBuildStats      (statistics)                          │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Types
//!
//! - [`CrdtChangeBuilder`]: Main builder for accumulating operations
//! - [`CrdtChangeResult`]: Final result containing all operations
//! - [`FileOps`]: Operations for a single file (trunk + branches + leaves)
//! - [`LineOps`]: Operations for a single line (branch + leaves)
//! - [`TokenOps`]: Operations for tokens within a line
//! - [`CrdtBuildStats`]: Statistics about the build process
//!
//! # Example
//!
//! ```rust
//! use atomic_core::record::workflow::crdt::builder::{
//!     CrdtChangeBuilder, CrdtBuildStats,
//! };
//! use atomic_core::change::Encoding;
//! use atomic_core::types::NodeId;
//! use atomic_core::diff::token::TokenKind;
//!
//! let change_id = NodeId::new(1);
//! let mut builder = CrdtChangeBuilder::new(change_id);
//!
//! // Add a new file
//! let trunk_id = builder.add_file("src/main.rs", Some(Encoding::Utf8));
//!
//! // Add lines to the file
//! let branch_id = builder.add_line(trunk_id, None);
//!
//! // Add tokens to the line
//! builder.add_token(branch_id, None, TokenKind::Word, b"fn");
//! builder.add_token(branch_id, None, TokenKind::Whitespace, b" ");
//! builder.add_token(branch_id, None, TokenKind::Word, b"main");
//!
//! // Finish and get the result
//! let result = builder.finish();
//! assert!(result.stats().files_added > 0);
//! ```
//!
//! # Integration with Line Analysis
//!
//! The builder can apply line changes from the [`LineAnalyzer`]:
//!
//! ```rust
//! use atomic_core::record::workflow::crdt::{
//!     builder::CrdtChangeBuilder,
//!     line_ops::{LineAnalyzer, AnalysisOptions, LineChange, LineChangeKind},
//! };
//! use atomic_core::types::NodeId;
//! use atomic_core::crdt::TrunkId;
//!
//! let change_id = NodeId::new(2);
//! let mut builder = CrdtChangeBuilder::new(change_id);
//!
//! // Existing file from a previous change
//! let trunk_id = TrunkId::new(NodeId::new(1), 0);
//!
//! // Analyze differences
//! let old = b"old line\n";
//! let new = b"new line\n";
//! let analyzer = LineAnalyzer::new(old, new, AnalysisOptions::default());
//! let analysis = analyzer.analyze();
//!
//! // Apply each change
//! for change in analysis.changes() {
//!     builder.apply_line_change(trunk_id, change);
//! }
//!
//! let result = builder.finish();
//! ```
//!
//! [`LineAnalyzer`]: super::line_ops::LineAnalyzer

use crate::change::Encoding;
use crate::crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId, TrunkOp};
use crate::diff::token::TokenKind;
use crate::types::NodeId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

use super::line_ops::{LineChange, LineChangeKind};
use super::tokenize::{ContentTokenizer, TokenizeOptions};

// BUILD ERROR

/// Errors that can occur during CRDT change building.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrdtBuildError {
    /// Attempted to add content to an unknown trunk.
    UnknownTrunk {
        /// The trunk ID that was not found.
        trunk_id: TrunkId,
    },

    /// Attempted to add content to an unknown branch.
    UnknownBranch {
        /// The branch ID that was not found.
        branch_id: BranchId,
    },

    /// The builder is in an invalid state for the requested operation.
    InvalidState {
        /// Description of the invalid state.
        description: String,
    },

    /// A referenced ID does not exist.
    InvalidReference {
        /// Description of the invalid reference.
        description: String,
    },

    /// Content validation failed.
    ValidationFailed {
        /// Description of the validation failure.
        description: String,
    },
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

// BUILD STATS

/// Statistics about the CRDT change building process.
///
/// Tracks counts and metrics about operations generated during building.
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

// TOKEN OPS

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

// LINE OPS

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
    /// This is the canonical representation for a modified line.  Carries
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

// FILE OPS

/// Operations for a single file.
///
/// Contains the trunk operation and all associated line/token operations.
#[derive(Debug, Clone)]
pub struct FileOps {
    /// The trunk ID for this file.
    trunk_id: TrunkId,

    /// The file path.
    path: String,

    /// The operation to perform on the trunk (if any).
    trunk_op: Option<TrunkOp>,

    /// Line operations within this file.
    line_ops: Vec<LineOps>,
}

impl FileOps {
    /// Creates a new file operation container.
    pub fn new(trunk_id: TrunkId, path: String, trunk_op: Option<TrunkOp>) -> Self {
        Self {
            trunk_id,
            path,
            trunk_op,
            line_ops: Vec::new(),
        }
    }

    /// Creates a file creation operation.
    pub fn create(trunk_id: TrunkId, path: String, encoding: Option<Encoding>) -> Self {
        Self {
            trunk_id,
            path: path.clone(),
            trunk_op: Some(TrunkOp::Create { path, encoding }),
            line_ops: Vec::new(),
        }
    }

    /// Creates a file deletion operation.
    pub fn delete(trunk_id: TrunkId, path: String) -> Self {
        Self {
            trunk_id,
            path,
            trunk_op: Some(TrunkOp::Delete { trunk: trunk_id }),
            line_ops: Vec::new(),
        }
    }

    /// Returns the trunk ID.
    #[inline]
    pub fn trunk_id(&self) -> TrunkId {
        self.trunk_id
    }

    /// Returns the file path.
    #[inline]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the trunk operation (if any).
    #[inline]
    pub fn trunk_op(&self) -> Option<&TrunkOp> {
        self.trunk_op.as_ref()
    }

    /// Returns the line operations.
    #[inline]
    pub fn line_ops(&self) -> &[LineOps] {
        &self.line_ops
    }

    /// Adds a line operation.
    pub fn add_line_op(&mut self, op: LineOps) {
        self.line_ops.push(op);
    }

    /// Returns the number of line operations.
    #[inline]
    pub fn line_count(&self) -> usize {
        self.line_ops.len()
    }

    /// Returns true if this file has any operations.
    pub fn has_operations(&self) -> bool {
        self.trunk_op.is_some() || !self.line_ops.is_empty()
    }

    /// Consumes and returns the trunk operation.
    pub fn into_trunk_op(self) -> Option<TrunkOp> {
        self.trunk_op
    }

    /// Convert to the serializable `change::ops::FileOps` type.
    ///
    /// This converts from the builder's internal representation to the
    /// format stored in changes.
    pub fn to_change_ops(&self) -> crate::change::ops::FileOps {
        let mut result = crate::change::ops::FileOps::new(
            self.trunk_id,
            self.path.clone(),
            self.trunk_op.clone(),
        );

        for line_op in &self.line_ops {
            let change_line_op =
                crate::change::ops::LineOps::new(line_op.branch_id(), line_op.operation().clone());
            result.add_line_op(change_line_op);
        }

        result
    }

    /// Consume and convert to the serializable `change::ops::FileOps` type.
    pub fn into_change_ops(self) -> crate::change::ops::FileOps {
        let mut result = crate::change::ops::FileOps::new(self.trunk_id, self.path, self.trunk_op);

        for line_op in self.line_ops {
            let old_line_num = line_op.old_line_num();
            let new_line_num = line_op.new_line_num();
            let mut change_line_op =
                crate::change::ops::LineOps::new(line_op.branch_id(), line_op.into_operation());
            // Preserve line numbers if set
            if let Some(n) = old_line_num {
                change_line_op = change_line_op.with_old_line_num(n);
            }
            if let Some(n) = new_line_num {
                change_line_op = change_line_op.with_new_line_num(n);
            }
            result.add_line_op(change_line_op);
        }

        result
    }
}

// CRDT CHANGE RESULT

/// The result of building CRDT operations for a change.
///
/// Contains all operations organized by file, along with the accumulated
/// content buffer and build statistics.
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

    /// Returns the file operations.
    #[inline]
    pub fn file_ops(&self) -> &[FileOps] {
        &self.file_ops
    }

    /// Returns the accumulated content.
    #[inline]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Returns the build statistics.
    #[inline]
    pub fn stats(&self) -> &CrdtBuildStats {
        &self.stats
    }

    /// Returns true if no operations were generated.
    pub fn is_empty(&self) -> bool {
        self.file_ops.is_empty() && self.content.is_empty()
    }

    /// Returns the total number of files affected.
    pub fn file_count(&self) -> usize {
        self.file_ops.len()
    }

    /// Returns all trunk operations.
    pub fn trunk_ops(&self) -> Vec<&TrunkOp> {
        self.file_ops.iter().filter_map(|f| f.trunk_op()).collect()
    }

    /// Returns all branch operations with their IDs.
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

// CRDT CHANGE BUILDER

/// Builder for accumulating CRDT operations during change recording.
///
/// The builder provides a high-level API for adding files, lines, and tokens
/// while managing ID allocation and content accumulation internally.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::crdt::builder::CrdtChangeBuilder;
/// use atomic_core::change::Encoding;
/// use atomic_core::types::NodeId;
///
/// let change_id = NodeId::new(1);
/// let mut builder = CrdtChangeBuilder::new(change_id);
///
/// // Add a file with content
/// let content = b"fn main() {\n    println!(\"Hello\");\n}\n";
/// let trunk_id = builder.add_file_with_content(
///     "main.rs",
///     content,
///     Some(Encoding::Utf8),
/// );
///
/// let result = builder.finish();
/// assert!(result.stats().files_added > 0);
/// ```
#[derive(Debug)]
pub struct CrdtChangeBuilder {
    /// The change ID for generating CRDT IDs.
    change_id: NodeId,

    /// Counter for trunk IDs.
    next_trunk_idx: u32,

    /// Counter for branch IDs.
    next_branch_idx: u32,

    /// Counter for leaf IDs.
    next_leaf_idx: u32,

    /// File operations being built.
    file_ops: Vec<FileOps>,

    /// Map from trunk ID to index in file_ops.
    trunk_index: HashMap<TrunkId, usize>,

    /// Map from branch ID to (file_index, line_index).
    branch_index: HashMap<BranchId, (usize, usize)>,

    /// Accumulated content.
    content: Vec<u8>,

    /// Build statistics.
    stats: CrdtBuildStats,

    /// The last allocated branch ID (for chaining).
    last_branch_id: Option<BranchId>,

    /// The last allocated leaf ID (for chaining).
    last_leaf_id: Option<LeafId>,
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
    fn alloc_trunk_id(&mut self) -> TrunkId {
        let id = TrunkId::new(self.change_id, self.next_trunk_idx);
        self.next_trunk_idx += 1;
        id
    }

    /// Allocates a new branch ID.
    fn alloc_branch_id(&mut self) -> BranchId {
        let id = BranchId::new(self.change_id, self.next_branch_idx);
        self.next_branch_idx += 1;
        self.last_branch_id = Some(id);
        id
    }

    /// Allocates a new leaf ID.
    fn alloc_leaf_id(&mut self) -> LeafId {
        let id = LeafId::new(self.change_id, self.next_leaf_idx);
        self.next_leaf_idx += 1;
        self.last_leaf_id = Some(id);
        id
    }

    /// Appends content to the buffer and returns the byte range.
    fn append_content(&mut self, data: &[u8]) -> std::ops::Range<usize> {
        let start = self.content.len();
        self.content.extend_from_slice(data);
        let end = self.content.len();
        self.stats.content_bytes += data.len();
        start..end
    }

    /// Adds a new file and returns its trunk ID.
    ///
    /// This creates a `TrunkOp::Create` for the file. Use [`add_line`] to
    /// add content to the file.
    ///
    /// [`add_line`]: Self::add_line
    pub fn add_file(&mut self, path: &str, encoding: Option<Encoding>) -> TrunkId {
        let trunk_id = self.alloc_trunk_id();
        let file_op = FileOps::create(trunk_id, path.to_string(), encoding);

        let file_idx = self.file_ops.len();
        self.trunk_index.insert(trunk_id, file_idx);
        self.file_ops.push(file_op);

        self.stats.files_added += 1;
        trunk_id
    }

    /// Adds a new file with content, automatically tokenizing into lines.
    ///
    /// This is a convenience method that creates the file and populates it
    /// with lines and tokens from the provided content.
    pub fn add_file_with_content(
        &mut self,
        path: &str,
        content: &[u8],
        encoding: Option<Encoding>,
    ) -> TrunkId {
        let trunk_id = self.add_file(path, encoding);

        // Tokenize content into lines
        let tokenizer = ContentTokenizer::new(content);
        let mut prev_branch: Option<BranchId> = None;

        for line in tokenizer.lines() {
            let branch_id = self.alloc_branch_id();

            // Generate leaf operations for tokens
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

            // Create line operation
            let line_op = LineOps::insert(branch_id, prev_branch, leaf_ops);

            // Add to file
            if let Some(&file_idx) = self.trunk_index.get(&trunk_id) {
                let line_idx = self.file_ops[file_idx].line_ops.len();
                self.branch_index.insert(branch_id, (file_idx, line_idx));
                self.file_ops[file_idx].add_line_op(line_op);
            }

            self.stats.lines_added += 1;
            prev_branch = Some(branch_id);
        }

        trunk_id
    }

    /// Marks a file for deletion.
    pub fn delete_file(&mut self, trunk_id: TrunkId) {
        let file_op = FileOps::delete(trunk_id, String::new());
        self.file_ops.push(file_op);
        self.stats.files_deleted += 1;
    }

    /// Marks a file for move/rename.
    pub fn move_file(&mut self, trunk_id: TrunkId, new_path: &str) {
        let file_op = FileOps::new(
            trunk_id,
            new_path.to_string(),
            Some(TrunkOp::Move {
                trunk: trunk_id,
                new_path: new_path.to_string(),
            }),
        );
        self.file_ops.push(file_op);
        self.stats.files_moved += 1;
    }

    /// Adds a new line to a file and returns its branch ID.
    ///
    /// # Arguments
    ///
    /// * `trunk_id` - The file to add the line to
    /// * `after` - The branch to insert after (None for start of file)
    pub fn add_line(&mut self, trunk_id: TrunkId, after: Option<BranchId>) -> BranchId {
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
        trunk_id: TrunkId,
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
            let file_op = FileOps::new(TrunkId::new(NodeId::new(0), 0), String::new(), None);
            let mut file_op = file_op;
            file_op.add_line_op(line_op);
            self.file_ops.push(file_op);
        }

        self.stats.lines_deleted += 1;
    }

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
        _branch_id: BranchId,
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

    /// Applies a line change from the analyzer.
    ///
    /// This is the integration point between the [`LineAnalyzer`] and the builder.
    ///
    /// [`LineAnalyzer`]: super::line_ops::LineAnalyzer
    pub fn apply_line_change(&mut self, trunk_id: TrunkId, change: &LineChange) {
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

    /// Merges another builder's results into this one.
    ///
    /// This is useful for parallel recording of multiple files.
    pub fn merge(&mut self, other: CrdtChangeBuilder) {
        // Merge file operations
        for file_op in other.file_ops {
            self.file_ops.push(file_op);
        }

        // Merge content
        self.content.extend(other.content);

        // Merge stats
        self.stats.merge(&other.stats);
    }

    /// Finishes building and returns the result.
    ///
    /// Consumes the builder and returns all accumulated operations.
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

// TESTS

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // CrdtBuildError Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_build_error_unknown_trunk_display() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let err = CrdtBuildError::UnknownTrunk { trunk_id };
        assert!(err.to_string().contains("unknown trunk"));
    }

    #[test]
    fn test_build_error_unknown_branch_display() {
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let err = CrdtBuildError::UnknownBranch { branch_id };
        assert!(err.to_string().contains("unknown branch"));
    }

    #[test]
    fn test_build_error_invalid_state_display() {
        let err = CrdtBuildError::InvalidState {
            description: "no active file".to_string(),
        };
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn test_build_error_is_error_trait() {
        let err = CrdtBuildError::ValidationFailed {
            description: "test".to_string(),
        };
        let _: &dyn std::error::Error = &err;
    }

    // ------------------------------------------------------------------------
    // CrdtBuildStats Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_build_stats_new() {
        let stats = CrdtBuildStats::new();
        assert_eq!(stats.files_added, 0);
        assert_eq!(stats.total_ops(), 0);
        assert!(!stats.has_changes());
    }

    #[test]
    fn test_build_stats_total_file_ops() {
        let mut stats = CrdtBuildStats::new();
        stats.files_added = 2;
        stats.files_deleted = 1;
        stats.files_moved = 1;
        stats.files_undeleted = 1;

        assert_eq!(stats.total_file_ops(), 5);
    }

    #[test]
    fn test_build_stats_total_line_ops() {
        let mut stats = CrdtBuildStats::new();
        stats.lines_added = 10;
        stats.lines_deleted = 3;
        stats.lines_modified = 2;

        assert_eq!(stats.total_line_ops(), 15);
    }

    #[test]
    fn test_build_stats_total_token_ops() {
        let mut stats = CrdtBuildStats::new();
        stats.tokens_added = 50;
        stats.tokens_deleted = 10;
        stats.tokens_replaced = 5;

        assert_eq!(stats.total_token_ops(), 65);
    }

    #[test]
    fn test_build_stats_has_changes() {
        let mut stats = CrdtBuildStats::new();
        assert!(!stats.has_changes());

        stats.files_added = 1;
        assert!(stats.has_changes());
    }

    #[test]
    fn test_build_stats_merge() {
        let mut stats1 = CrdtBuildStats::new();
        stats1.files_added = 1;
        stats1.lines_added = 10;

        let mut stats2 = CrdtBuildStats::new();
        stats2.files_added = 2;
        stats2.lines_added = 20;

        stats1.merge(&stats2);

        assert_eq!(stats1.files_added, 3);
        assert_eq!(stats1.lines_added, 30);
    }

    #[test]
    fn test_build_stats_display() {
        let mut stats = CrdtBuildStats::new();
        stats.files_added = 1;
        stats.lines_added = 5;
        stats.tokens_added = 20;

        let display = format!("{}", stats);
        assert!(display.contains("files:"));
        assert!(display.contains("lines:"));
        assert!(display.contains("tokens:"));
    }

    // ------------------------------------------------------------------------
    // TokenOps Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_token_ops_new() {
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let op = LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"test".to_vec(),
        };
        let token_ops = TokenOps::new(leaf_id, op.clone());

        assert_eq!(token_ops.leaf_id(), leaf_id);
    }

    #[test]
    fn test_token_ops_into_operation() {
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let op = LeafOp::Delete { leaf: leaf_id };
        let token_ops = TokenOps::new(leaf_id, op);

        let _ = token_ops.into_operation();
    }

    // ------------------------------------------------------------------------
    // LineOps Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_line_ops_new() {
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let op = BranchOp::Insert {
            after: None,
            content: vec![],
        };
        let line_ops = LineOps::new(branch_id, op);

        assert_eq!(line_ops.branch_id(), branch_id);
        assert!(line_ops.token_ops().is_empty());
    }

    #[test]
    fn test_line_ops_insert() {
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let leaf_ops = vec![LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"test".to_vec(),
        }];
        let line_ops = LineOps::insert(branch_id, None, leaf_ops);

        assert_eq!(line_ops.branch_id(), branch_id);
    }

    #[test]
    fn test_line_ops_delete() {
        let branch_id = BranchId::new(NodeId::new(1), 5);
        let line_ops = LineOps::delete(branch_id, vec![]);

        assert_eq!(line_ops.branch_id(), branch_id);
        match line_ops.operation() {
            BranchOp::Delete { branch, .. } => assert_eq!(*branch, branch_id),
            _ => panic!("Expected BranchOp::Delete"),
        }
    }

    #[test]
    fn test_line_ops_delete_empty() {
        let branch_id = BranchId::new(NodeId::new(1), 5);
        let line_ops = LineOps::delete_empty(branch_id);

        assert_eq!(line_ops.branch_id(), branch_id);
        match line_ops.operation() {
            BranchOp::Delete { branch, content } => {
                assert_eq!(*branch, branch_id);
                assert!(content.is_empty());
            }
            _ => panic!("Expected BranchOp::Delete"),
        }
    }

    #[test]
    fn test_line_ops_add_token_op() {
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let leaf_id = LeafId::new(NodeId::new(1), 0);
        let mut line_ops = LineOps::new(
            branch_id,
            BranchOp::Insert {
                after: None,
                content: vec![],
            },
        );

        line_ops.add_token_op(TokenOps::new(leaf_id, LeafOp::Delete { leaf: leaf_id }));

        assert_eq!(line_ops.token_ops().len(), 1);
    }

    // ------------------------------------------------------------------------
    // FileOps Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_file_ops_new() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::new(trunk_id, "test.rs".to_string(), None);

        assert_eq!(file_ops.trunk_id(), trunk_id);
        assert_eq!(file_ops.path(), "test.rs");
        assert!(file_ops.trunk_op().is_none());
        assert!(file_ops.line_ops().is_empty());
    }

    #[test]
    fn test_file_ops_create() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::create(trunk_id, "main.rs".to_string(), Some(Encoding::Utf8));

        assert!(file_ops.trunk_op().is_some());
        match file_ops.trunk_op().unwrap() {
            TrunkOp::Create { path, encoding } => {
                assert_eq!(path, "main.rs");
                assert_eq!(*encoding, Some(Encoding::Utf8));
            }
            _ => panic!("Expected TrunkOp::Create"),
        }
    }

    #[test]
    fn test_file_ops_delete() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_ops = FileOps::delete(trunk_id, "old.rs".to_string());

        match file_ops.trunk_op().unwrap() {
            TrunkOp::Delete { trunk } => assert_eq!(*trunk, trunk_id),
            _ => panic!("Expected TrunkOp::Delete"),
        }
    }

    #[test]
    fn test_file_ops_add_line_op() {
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let branch_id = BranchId::new(NodeId::new(1), 0);
        let mut file_ops = FileOps::create(trunk_id, "test.rs".to_string(), None);

        file_ops.add_line_op(LineOps::insert(branch_id, None, vec![]));

        assert_eq!(file_ops.line_count(), 1);
        assert!(file_ops.has_operations());
    }

    // ------------------------------------------------------------------------
    // CrdtChangeResult Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_crdt_change_result_new() {
        let result = CrdtChangeResult::new();
        assert!(result.is_empty());
        assert_eq!(result.file_count(), 0);
        assert!(result.content().is_empty());
    }

    #[test]
    fn test_crdt_change_result_trunk_ops() {
        let mut result = CrdtChangeResult::new();

        // Add a file op directly for testing
        let trunk_id = TrunkId::new(NodeId::new(1), 0);
        let file_op = FileOps::create(trunk_id, "test.rs".to_string(), None);
        result.file_ops.push(file_op);

        assert_eq!(result.trunk_ops().len(), 1);
    }

    #[test]
    fn test_crdt_change_result_into_parts() {
        let result = CrdtChangeResult::new();
        let (file_ops, content, stats) = result.into_parts();

        assert!(file_ops.is_empty());
        assert!(content.is_empty());
        assert!(!stats.has_changes());
    }

    // ------------------------------------------------------------------------
    // CrdtChangeBuilder Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_builder_new() {
        let change_id = NodeId::new(1);
        let builder = CrdtChangeBuilder::new(change_id);

        assert_eq!(builder.change_id(), change_id);
        assert!(!builder.has_operations());
    }

    #[test]
    fn test_builder_add_file() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let trunk_id = builder.add_file("test.rs", None);

        assert_eq!(trunk_id.change_id(), change_id);
        assert!(builder.has_operations());
        assert_eq!(builder.current_stats().files_added, 1);
    }

    #[test]
    fn test_builder_add_file_with_encoding() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let trunk_id = builder.add_file("main.rs", Some(Encoding::Utf8));
        let result = builder.finish();

        assert_eq!(result.file_count(), 1);
        match result.file_ops()[0].trunk_op().unwrap() {
            TrunkOp::Create { encoding, .. } => {
                assert_eq!(*encoding, Some(Encoding::Utf8));
            }
            _ => panic!("Expected TrunkOp::Create"),
        }
    }

    #[test]
    fn test_builder_add_file_with_content() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let content = b"line one\nline two\n";
        let trunk_id = builder.add_file_with_content("test.txt", content, None);

        let result = builder.finish();

        assert_eq!(result.file_count(), 1);
        assert!(result.stats().lines_added >= 2);
        assert!(result.stats().tokens_added > 0);
    }

    #[test]
    fn test_builder_delete_file() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let trunk_id = TrunkId::new(NodeId::new(0), 0); // Existing file
        builder.delete_file(trunk_id);

        let result = builder.finish();
        assert_eq!(result.stats().files_deleted, 1);
    }

    #[test]
    fn test_builder_move_file() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let trunk_id = TrunkId::new(NodeId::new(0), 0);
        builder.move_file(trunk_id, "new/path.rs");

        let result = builder.finish();
        assert_eq!(result.stats().files_moved, 1);
    }

    #[test]
    fn test_builder_add_line() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let trunk_id = builder.add_file("test.rs", None);
        let branch_id = builder.add_line(trunk_id, None);

        assert_eq!(branch_id.change_id(), change_id);
        assert_eq!(builder.current_stats().lines_added, 1);
    }

    #[test]
    fn test_builder_add_line_with_content() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let trunk_id = builder.add_file("test.rs", None);
        let branch_id = builder.add_line_with_content(trunk_id, None, b"let x = 42;");

        assert!(builder.current_stats().tokens_added > 0);
    }

    #[test]
    fn test_builder_delete_line() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let branch_id = BranchId::new(NodeId::new(0), 5);
        builder.delete_line(branch_id);

        assert_eq!(builder.current_stats().lines_deleted, 1);
    }

    #[test]
    fn test_builder_add_token() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let trunk_id = builder.add_file("test.rs", None);
        let branch_id = builder.add_line(trunk_id, None);
        let leaf_id = builder.add_token(branch_id, None, TokenKind::Word, b"hello");

        assert_eq!(leaf_id.change_id(), change_id);
        assert_eq!(builder.current_stats().tokens_added, 1);
    }

    #[test]
    fn test_builder_delete_token() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let leaf_id = LeafId::new(NodeId::new(0), 3);
        builder.delete_token(leaf_id);

        assert_eq!(builder.current_stats().tokens_deleted, 1);
    }

    #[test]
    fn test_builder_replace_token() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let leaf_id = LeafId::new(NodeId::new(0), 3);
        builder.replace_token(leaf_id, b"new_value");

        assert_eq!(builder.current_stats().tokens_replaced, 1);
    }

    #[test]
    fn test_builder_apply_line_change_insert() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let trunk_id = builder.add_file("test.rs", None);
        let change = LineChange::insert(0, b"new line".to_vec());

        builder.apply_line_change(trunk_id, &change);

        assert!(builder.current_stats().lines_added >= 1);
    }

    #[test]
    fn test_builder_apply_line_change_equal() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        let trunk_id = builder.add_file("test.rs", None);
        let change = LineChange::equal(0, 0, b"unchanged".to_vec());

        builder.apply_line_change(trunk_id, &change);

        // Equal lines don't generate operations
        assert_eq!(builder.current_stats().lines_added, 0);
    }

    #[test]
    fn test_builder_merge() {
        let change_id = NodeId::new(1);

        let mut builder1 = CrdtChangeBuilder::new(change_id);
        builder1.add_file("file1.rs", None);

        let mut builder2 = CrdtChangeBuilder::new(change_id);
        builder2.add_file("file2.rs", None);

        builder1.merge(builder2);

        assert_eq!(builder1.current_stats().files_added, 2);
    }

    #[test]
    fn test_builder_finish() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        builder.add_file("test.rs", None);

        let result = builder.finish();

        assert!(!result.is_empty());
        assert_eq!(result.stats().files_added, 1);
    }

    #[test]
    fn test_builder_last_branch() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        assert!(builder.last_branch().is_none());

        let trunk_id = builder.add_file("test.rs", None);
        let branch_id = builder.add_line(trunk_id, None);

        assert_eq!(builder.last_branch(), Some(branch_id));
    }

    #[test]
    fn test_builder_last_leaf() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        assert!(builder.last_leaf().is_none());

        let trunk_id = builder.add_file("test.rs", None);
        let branch_id = builder.add_line(trunk_id, None);
        let leaf_id = builder.add_token(branch_id, None, TokenKind::Word, b"test");

        assert_eq!(builder.last_leaf(), Some(leaf_id));
    }

    // ------------------------------------------------------------------------
    // Integration Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_integration_full_file_workflow() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        // Create a file with content
        let content = b"fn main() {\n    println!(\"Hello\");\n}\n";
        let _trunk_id = builder.add_file_with_content("main.rs", content, Some(Encoding::Utf8));

        let result = builder.finish();

        assert_eq!(result.stats().files_added, 1);
        assert!(result.stats().lines_added >= 3);
        assert!(result.stats().tokens_added > 5);
        assert!(!result.content().is_empty());
    }

    #[test]
    fn test_integration_multiple_files() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        builder.add_file_with_content("file1.rs", b"content1", None);
        builder.add_file_with_content("file2.rs", b"content2", None);
        builder.add_file_with_content("file3.rs", b"content3", None);

        let result = builder.finish();

        assert_eq!(result.stats().files_added, 3);
        assert_eq!(result.file_count(), 3);
    }

    #[test]
    fn test_integration_mixed_operations() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        // Add a file
        builder.add_file_with_content("new.rs", b"new content", None);

        // Delete a file
        let existing_trunk = TrunkId::new(NodeId::new(0), 0);
        builder.delete_file(existing_trunk);

        // Move a file
        let another_trunk = TrunkId::new(NodeId::new(0), 1);
        builder.move_file(another_trunk, "new/location.rs");

        let result = builder.finish();

        assert_eq!(result.stats().files_added, 1);
        assert_eq!(result.stats().files_deleted, 1);
        assert_eq!(result.stats().files_moved, 1);
    }

    #[test]
    fn test_integration_empty_file() {
        let change_id = NodeId::new(1);
        let mut builder = CrdtChangeBuilder::new(change_id);

        builder.add_file_with_content("empty.txt", b"", None);

        let result = builder.finish();

        assert_eq!(result.stats().files_added, 1);
        // Empty file may have no lines
    }
}

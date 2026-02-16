//! CRDT operations for change storage.
//!
//! This module defines the hierarchical CRDT operations that are stored
//! in changes. These operations follow the Trunk → Branch → Leaf model:
//!
//! - **FileOps** (Trunk level): File-level operations (create, delete, move)
//! - **LineOps** (Branch level): Line-level operations (insert, delete, restore)
//! - **TokenOps** (Leaf level): Token-level operations (insert, delete, replace)
//!
//! # Graph Position Enrichment
//!
//! After globalization, LineOps can be enriched with their corresponding
//! graph positions via the `content_range` field. This enables the CRDT
//! layer to be linked to the graph layer through the BRANCH_VERTEX table.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        Change Structure                                  │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Change                                                                 │
//! │  └── file_ops: Vec<FileOps>                                            │
//! │      └── FileOps                                                        │
//! │          ├── trunk_id: TrunkId                                         │
//! │          ├── path: String                                              │
//! │          ├── trunk_op: Option<TrunkOp>   (Create/Delete/Move/Undelete) │
//! │          └── line_ops: Vec<LineOps>                                    │
//! │              └── LineOps                                                │
//! │                  ├── branch_id: BranchId                               │
//! │                  └── operation: BranchOp                               │
//! │                      └── Insert { after, content: Vec<LeafOp> }        │
//! │                          └── LeafOp::Insert { after, kind, content }   │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_core::change::ops::{FileOps, LineOps};
//! use atomic_core::crdt::{TrunkId, TrunkOp, BranchId, BranchOp, LeafOp};
//! use atomic_core::change::Encoding;
//! use atomic_core::diff::token::TokenKind;
//! use atomic_core::types::NodeId;
//!
//! // Create a new file with content
//! let trunk_id = TrunkId::new(NodeId::new(1), 0);
//! let branch_id = BranchId::new(NodeId::new(1), 0);
//!
//! let mut file_ops = FileOps::create(trunk_id, "hello.txt".to_string(), Some(Encoding::Utf8));
//!
//! // Add a line with tokens
//! let line_ops = LineOps::insert(
//!     branch_id,
//!     None, // Insert at start
//!     vec![
//!         LeafOp::Insert {
//!             after: None,
//!             kind: TokenKind::Word,
//!             content: b"Hello".to_vec(),
//!         },
//!         LeafOp::Insert {
//!             after: None,
//!             kind: TokenKind::Whitespace,
//!             content: b" ".to_vec(),
//!         },
//!         LeafOp::Insert {
//!             after: None,
//!             kind: TokenKind::Word,
//!             content: b"World".to_vec(),
//!         },
//!     ],
//! );
//!
//! file_ops.add_line_op(line_ops);
//! ```

use crate::change::Encoding;
use crate::crdt::{BranchId, BranchOp, LeafOp, TrunkId, TrunkOp};
use crate::types::ChangePosition;
use serde::{Deserialize, Serialize};
use std::fmt;

// =============================================================================
// FileOps (Trunk Level)
// =============================================================================

/// Operations for a single file.
///
/// Contains the trunk operation (file-level: create/delete/move) and all
/// associated line operations within the file.
///
/// This is the top level of the CRDT hierarchy stored in a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOps {
    /// The trunk ID for this file.
    trunk_id: TrunkId,

    /// The file path.
    path: String,

    /// The operation to perform on the trunk (if any).
    ///
    /// This is `Some` for file creation, deletion, move, or undelete.
    /// This is `None` when only modifying file contents (edit operations).
    trunk_op: Option<TrunkOp>,

    /// Line operations within this file.
    line_ops: Vec<LineOps>,
}

impl FileOps {
    /// Creates a new file operation container.
    ///
    /// Use this when you have both the trunk operation and will add line ops later.
    pub fn new(trunk_id: TrunkId, path: String, trunk_op: Option<TrunkOp>) -> Self {
        Self {
            trunk_id,
            path,
            trunk_op,
            line_ops: Vec::new(),
        }
    }

    /// Creates a file creation operation.
    ///
    /// # Arguments
    ///
    /// * `trunk_id` - The trunk ID for the new file
    /// * `path` - The file path
    /// * `encoding` - Optional text encoding (None for binary files)
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

    /// Creates a file move/rename operation.
    pub fn move_file(trunk_id: TrunkId, _old_path: String, new_path: String) -> Self {
        Self {
            trunk_id,
            path: new_path.clone(),
            trunk_op: Some(TrunkOp::Move {
                trunk: trunk_id,
                new_path,
            }),
            line_ops: Vec::new(),
        }
    }

    /// Creates a file undelete operation.
    pub fn undelete(trunk_id: TrunkId, path: String) -> Self {
        Self {
            trunk_id,
            path,
            trunk_op: Some(TrunkOp::Undelete { trunk: trunk_id }),
            line_ops: Vec::new(),
        }
    }

    /// Creates an edit operation (modifying an existing file).
    ///
    /// This has no trunk operation, only line operations.
    pub fn edit(trunk_id: TrunkId, path: String) -> Self {
        Self {
            trunk_id,
            path,
            trunk_op: None,
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

    /// Returns a mutable reference to the line operations.
    #[inline]
    pub fn line_ops_mut(&mut self) -> &mut Vec<LineOps> {
        &mut self.line_ops
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

    /// Returns the total number of token operations across all lines.
    pub fn token_count(&self) -> usize {
        self.line_ops.iter().map(|line| line.leaf_op_count()).sum()
    }

    /// Returns true if this file has any operations.
    pub fn has_operations(&self) -> bool {
        self.trunk_op.is_some() || !self.line_ops.is_empty()
    }

    /// Returns true if this is a file creation.
    pub fn is_create(&self) -> bool {
        matches!(self.trunk_op, Some(TrunkOp::Create { .. }))
    }

    /// Returns true if this is a file deletion.
    pub fn is_delete(&self) -> bool {
        matches!(self.trunk_op, Some(TrunkOp::Delete { .. }))
    }

    /// Returns true if this is a file move/rename.
    pub fn is_move(&self) -> bool {
        matches!(self.trunk_op, Some(TrunkOp::Move { .. }))
    }

    /// Returns true if this is an edit (no trunk operation).
    pub fn is_edit(&self) -> bool {
        self.trunk_op.is_none() && !self.line_ops.is_empty()
    }

    /// Consumes and returns the trunk operation.
    pub fn into_trunk_op(self) -> Option<TrunkOp> {
        self.trunk_op
    }

    /// Consumes and returns all parts.
    pub fn into_parts(self) -> (TrunkId, String, Option<TrunkOp>, Vec<LineOps>) {
        (self.trunk_id, self.path, self.trunk_op, self.line_ops)
    }
}

impl fmt::Display for FileOps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op_type = match &self.trunk_op {
            Some(TrunkOp::Create { .. }) => "create",
            Some(TrunkOp::Delete { .. }) => "delete",
            Some(TrunkOp::Move { .. }) => "move",
            Some(TrunkOp::Undelete { .. }) => "undelete",
            None => "edit",
        };
        write!(
            f,
            "FileOps({} {}, {} lines, {} tokens)",
            op_type,
            self.path,
            self.line_ops.len(),
            self.token_count()
        )
    }
}

// =============================================================================
// LineOps (Branch Level)
// =============================================================================

/// Operations for a single line.
///
/// Contains the branch operation which includes any associated leaf (token)
/// operations for line insertions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineOps {
    /// The branch ID for this line.
    branch_id: BranchId,

    /// The operation to perform on the branch.
    ///
    /// For inserts, this contains the leaf operations (tokens).
    operation: BranchOp,

    /// The line number in the old file (for deletes/modifies).
    /// None for pure insertions.
    ///
    /// We use `#[serde(default)]` so that missing fields deserialize as `None`.
    #[serde(default)]
    old_line_num: Option<usize>,

    /// The line number in the new file (for inserts/modifies).
    /// None for pure deletions.
    ///
    /// We use `#[serde(default)]` so that missing fields deserialize as `None`.
    #[serde(default)]
    new_line_num: Option<usize>,

    /// The byte range in the change content buffer after globalization.
    ///
    /// This is set during globalization to link the CRDT branch to its
    /// corresponding graph vertex. The range (start, end) corresponds to
    /// the `ChangePosition` values used in the graph's `Insertion` atom.
    ///
    /// When combined with the change's NodeId during apply, this gives us
    /// the `GraphNode<NodeId>` needed to populate the BRANCH_VERTEX table.
    ///
    /// - `None` before globalization or for operations that don't create content
    /// - `Some((start, end))` after globalization for insert operations
    #[serde(default)]
    content_range: Option<(ChangePosition, ChangePosition)>,
}

impl LineOps {
    /// Creates a new line operation.
    pub fn new(branch_id: BranchId, operation: BranchOp) -> Self {
        Self {
            branch_id,
            operation,
            old_line_num: None,
            new_line_num: None,
            content_range: None,
        }
    }

    /// Creates a new line operation with line numbers.
    pub fn new_with_line_nums(
        branch_id: BranchId,
        operation: BranchOp,
        old_line_num: Option<usize>,
        new_line_num: Option<usize>,
    ) -> Self {
        Self {
            branch_id,
            operation,
            old_line_num,
            new_line_num,
            content_range: None,
        }
    }

    /// Creates a line insert operation with leaf operations.
    ///
    /// # Arguments
    ///
    /// * `branch_id` - The branch ID for this new line
    /// * `after` - Insert after this branch ID (None = start of file)
    /// * `leaf_ops` - The token operations for this line's content
    pub fn insert(branch_id: BranchId, after: Option<BranchId>, leaf_ops: Vec<LeafOp>) -> Self {
        Self {
            branch_id,
            operation: BranchOp::Insert {
                after,
                content: leaf_ops,
            },
            old_line_num: None,
            new_line_num: None,
            content_range: None,
        }
    }

    /// Creates a line insert operation with line number.
    pub fn insert_at(
        branch_id: BranchId,
        after: Option<BranchId>,
        leaf_ops: Vec<LeafOp>,
        new_line_num: usize,
    ) -> Self {
        Self {
            branch_id,
            operation: BranchOp::Insert {
                after,
                content: leaf_ops,
            },
            old_line_num: None,
            new_line_num: Some(new_line_num),
            content_range: None,
        }
    }

    /// Creates a line delete operation.
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
            old_line_num: None,
            new_line_num: None,
            content_range: None,
        }
    }

    /// Creates a line delete operation with line number.
    pub fn delete_at(branch_id: BranchId, content: Vec<LeafOp>, old_line_num: usize) -> Self {
        Self {
            branch_id,
            operation: BranchOp::Delete {
                branch: branch_id,
                content,
            },
            old_line_num: Some(old_line_num),
            new_line_num: None,
            content_range: None,
        }
    }

    /// Creates a line delete operation without content.
    ///
    /// Use this when the original content is not available.
    /// The diff will show a placeholder for the deleted content.
    pub fn delete_empty(branch_id: BranchId) -> Self {
        Self {
            branch_id,
            operation: BranchOp::Delete {
                branch: branch_id,
                content: Vec::new(),
            },
            old_line_num: None,
            new_line_num: None,
            content_range: None,
        }
    }

    /// Creates a line restore operation.
    pub fn restore(branch_id: BranchId) -> Self {
        Self {
            branch_id,
            operation: BranchOp::Restore { branch: branch_id },
            old_line_num: None,
            new_line_num: None,
            content_range: None,
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

    /// Set the content range (byte positions in the change content buffer).
    ///
    /// This is called during globalization to link the CRDT branch to its
    /// corresponding graph vertex position.
    ///
    /// # Arguments
    ///
    /// * `start` - Start byte position in the change content buffer
    /// * `end` - End byte position in the change content buffer
    pub fn set_content_range(&mut self, start: ChangePosition, end: ChangePosition) {
        self.content_range = Some((start, end));
    }

    /// Set the content range using a builder pattern.
    #[must_use]
    pub fn with_content_range(mut self, start: ChangePosition, end: ChangePosition) -> Self {
        self.content_range = Some((start, end));
        self
    }

    /// Get the content range.
    ///
    /// Returns the byte range in the change content buffer, if set during
    /// globalization. This range corresponds to the graph vertex positions.
    #[inline]
    pub fn content_range(&self) -> Option<(ChangePosition, ChangePosition)> {
        self.content_range
    }

    /// Check if this line operation has been enriched with graph positions.
    #[inline]
    pub fn has_content_range(&self) -> bool {
        self.content_range.is_some()
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

    /// Returns the leaf operations (for inserts and deletes).
    ///
    /// Returns an empty slice for restore operations.
    pub fn leaf_ops(&self) -> &[LeafOp] {
        match &self.operation {
            BranchOp::Insert { content, .. } => content,
            BranchOp::Delete { content, .. } => content,
            BranchOp::Restore { .. } => &[],
        }
    }

    /// Returns the number of leaf operations.
    pub fn leaf_op_count(&self) -> usize {
        self.leaf_ops().len()
    }

    /// Returns true if this is an insert operation.
    pub fn is_insert(&self) -> bool {
        matches!(self.operation, BranchOp::Insert { .. })
    }

    /// Returns true if this is a delete operation.
    pub fn is_delete(&self) -> bool {
        matches!(self.operation, BranchOp::Delete { .. })
    }

    /// Returns true if this is a restore operation.
    pub fn is_restore(&self) -> bool {
        matches!(self.operation, BranchOp::Restore { .. })
    }

    /// Returns the "after" reference for insert operations.
    pub fn insert_after(&self) -> Option<BranchId> {
        match &self.operation {
            BranchOp::Insert { after, .. } => *after,
            _ => None,
        }
    }

    /// Consumes and returns the branch operation.
    pub fn into_operation(self) -> BranchOp {
        self.operation
    }

    /// Consumes and returns all parts.
    pub fn into_parts(self) -> (BranchId, BranchOp) {
        (self.branch_id, self.operation)
    }
}

impl fmt::Display for LineOps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op_type = match &self.operation {
            BranchOp::Insert { content, .. } => format!("insert({} tokens)", content.len()),
            BranchOp::Delete { .. } => "delete".to_string(),
            BranchOp::Restore { .. } => "restore".to_string(),
        };
        write!(f, "LineOps({}, {})", self.branch_id, op_type)
    }
}

// =============================================================================
// Statistics
// =============================================================================

/// Statistics about CRDT operations in a change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileOpsStats {
    /// Number of files created.
    pub files_created: usize,
    /// Number of files deleted.
    pub files_deleted: usize,
    /// Number of files moved/renamed.
    pub files_moved: usize,
    /// Number of files undeleted.
    pub files_undeleted: usize,
    /// Number of files edited (content only).
    pub files_edited: usize,
    /// Total number of lines added.
    pub lines_added: usize,
    /// Total number of lines deleted.
    pub lines_deleted: usize,
    /// Total number of lines restored.
    pub lines_restored: usize,
    /// Total number of tokens added.
    pub tokens_added: usize,
    /// Total number of tokens deleted.
    pub tokens_deleted: usize,
    /// Total number of tokens replaced.
    pub tokens_replaced: usize,
}

impl FileOpsStats {
    /// Creates new empty statistics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Computes statistics from a list of file operations.
    pub fn from_file_ops(file_ops: &[FileOps]) -> Self {
        let mut stats = Self::new();

        for file in file_ops {
            // Count file-level operations
            match file.trunk_op() {
                Some(TrunkOp::Create { .. }) => stats.files_created += 1,
                Some(TrunkOp::Delete { .. }) => stats.files_deleted += 1,
                Some(TrunkOp::Move { .. }) => stats.files_moved += 1,
                Some(TrunkOp::Undelete { .. }) => stats.files_undeleted += 1,
                None if !file.line_ops().is_empty() => stats.files_edited += 1,
                None => {}
            }

            // Count line and token operations
            for line in file.line_ops() {
                match line.operation() {
                    BranchOp::Insert { content, .. } => {
                        stats.lines_added += 1;
                        for leaf_op in content {
                            match leaf_op {
                                LeafOp::Insert { .. } => stats.tokens_added += 1,
                                LeafOp::Delete { .. } => stats.tokens_deleted += 1,
                                LeafOp::Replace { .. } => stats.tokens_replaced += 1,
                                LeafOp::Restore { .. } => {} // Restore is rare in new lines
                            }
                        }
                    }
                    BranchOp::Delete { .. } => stats.lines_deleted += 1,
                    BranchOp::Restore { .. } => stats.lines_restored += 1,
                }
            }
        }

        stats
    }

    /// Returns the total number of file operations.
    pub fn total_file_ops(&self) -> usize {
        self.files_created
            + self.files_deleted
            + self.files_moved
            + self.files_undeleted
            + self.files_edited
    }

    /// Returns the total number of line operations.
    pub fn total_line_ops(&self) -> usize {
        self.lines_added + self.lines_deleted + self.lines_restored
    }

    /// Returns the total number of token operations.
    pub fn total_token_ops(&self) -> usize {
        self.tokens_added + self.tokens_deleted + self.tokens_replaced
    }

    /// Returns true if there are any changes.
    pub fn has_changes(&self) -> bool {
        self.total_file_ops() > 0 || self.total_line_ops() > 0 || self.total_token_ops() > 0
    }
}

impl fmt::Display for FileOpsStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} files (+{} -{} ~{} ^{}), {} lines (+{} -{} ^{}), {} tokens (+{} -{} ~{})",
            self.total_file_ops(),
            self.files_created,
            self.files_deleted,
            self.files_moved,
            self.files_undeleted,
            self.total_line_ops(),
            self.lines_added,
            self.lines_deleted,
            self.lines_restored,
            self.total_token_ops(),
            self.tokens_added,
            self.tokens_deleted,
            self.tokens_replaced,
        )
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::token::TokenKind;
    use crate::types::NodeId;

    fn test_trunk_id() -> TrunkId {
        TrunkId::new(NodeId::new(1), 0)
    }

    fn test_branch_id(idx: u32) -> BranchId {
        BranchId::new(NodeId::new(1), idx)
    }

    // =========================================================================
    // FileOps Tests
    // =========================================================================

    #[test]
    fn test_file_ops_create() {
        let ops = FileOps::create(
            test_trunk_id(),
            "test.txt".to_string(),
            Some(Encoding::Utf8),
        );

        assert!(ops.is_create());
        assert!(!ops.is_delete());
        assert!(!ops.is_edit());
        assert_eq!(ops.path(), "test.txt");
        assert!(ops.has_operations());
    }

    #[test]
    fn test_file_ops_delete() {
        let ops = FileOps::delete(test_trunk_id(), "old.txt".to_string());

        assert!(ops.is_delete());
        assert!(!ops.is_create());
        assert_eq!(ops.path(), "old.txt");
    }

    #[test]
    fn test_file_ops_move() {
        let ops = FileOps::move_file(
            test_trunk_id(),
            "old/path.txt".to_string(),
            "new/path.txt".to_string(),
        );

        assert!(ops.is_move());
        assert_eq!(ops.path(), "new/path.txt");
    }

    #[test]
    fn test_file_ops_edit() {
        let mut ops = FileOps::edit(test_trunk_id(), "modify.txt".to_string());
        ops.add_line_op(LineOps::insert(test_branch_id(0), None, vec![]));

        assert!(ops.is_edit());
        assert!(!ops.is_create());
        assert_eq!(ops.line_count(), 1);
    }

    #[test]
    fn test_file_ops_token_count() {
        let mut ops = FileOps::edit(test_trunk_id(), "test.txt".to_string());

        // Add a line with 3 tokens
        ops.add_line_op(LineOps::insert(
            test_branch_id(0),
            None,
            vec![
                LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Word,
                    content: b"Hello".to_vec(),
                },
                LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Whitespace,
                    content: b" ".to_vec(),
                },
                LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Word,
                    content: b"World".to_vec(),
                },
            ],
        ));

        // Add another line with 2 tokens
        ops.add_line_op(LineOps::insert(
            test_branch_id(1),
            Some(test_branch_id(0)),
            vec![
                LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Word,
                    content: b"Hello".to_vec(),
                },
                LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Newline,
                    content: b"\n".to_vec(),
                },
            ],
        ));

        assert_eq!(ops.line_count(), 2);
        assert_eq!(ops.token_count(), 5);
    }

    #[test]
    fn test_file_ops_display() {
        let ops = FileOps::create(
            test_trunk_id(),
            "hello.rs".to_string(),
            Some(Encoding::Utf8),
        );
        let display = format!("{}", ops);
        assert!(display.contains("create"));
        assert!(display.contains("hello.rs"));
    }

    #[test]
    fn test_file_ops_into_parts() {
        let ops = FileOps::create(
            test_trunk_id(),
            "test.txt".to_string(),
            Some(Encoding::Utf8),
        );
        let (trunk_id, path, trunk_op, line_ops) = ops.into_parts();

        assert_eq!(trunk_id, test_trunk_id());
        assert_eq!(path, "test.txt");
        assert!(trunk_op.is_some());
        assert!(line_ops.is_empty());
    }

    // =========================================================================
    // LineOps Tests
    // =========================================================================

    #[test]
    fn test_line_ops_insert() {
        let ops = LineOps::insert(
            test_branch_id(0),
            None,
            vec![LeafOp::Insert {
                after: None,
                kind: TokenKind::Word,
                content: b"Hello".to_vec(),
            }],
        );

        assert!(ops.is_insert());
        assert!(!ops.is_delete());
        assert_eq!(ops.leaf_op_count(), 1);
        assert_eq!(ops.insert_after(), None);
    }

    #[test]
    fn test_line_ops_insert_after() {
        let ops = LineOps::insert(test_branch_id(1), Some(test_branch_id(0)), vec![]);

        assert!(ops.is_insert());
        assert_eq!(ops.insert_after(), Some(test_branch_id(0)));
    }

    #[test]
    fn test_line_ops_delete() {
        let ops = LineOps::delete(test_branch_id(5), vec![]);

        assert!(ops.is_delete());
        assert!(!ops.is_insert());
        assert_eq!(ops.leaf_op_count(), 0);
    }

    #[test]
    fn test_line_ops_delete_with_content() {
        use crate::diff::token::TokenKind;
        let content = vec![LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"hello".to_vec(),
        }];
        let ops = LineOps::delete(test_branch_id(5), content);

        assert!(ops.is_delete());
        assert_eq!(ops.leaf_op_count(), 1);
    }

    #[test]
    fn test_line_ops_delete_empty() {
        let ops = LineOps::delete_empty(test_branch_id(5));

        assert!(ops.is_delete());
        assert_eq!(ops.leaf_op_count(), 0);
    }

    #[test]
    fn test_line_ops_restore() {
        let ops = LineOps::restore(test_branch_id(3));

        assert!(ops.is_restore());
        assert!(!ops.is_delete());
    }

    #[test]
    fn test_line_ops_display() {
        let ops = LineOps::insert(
            test_branch_id(0),
            None,
            vec![
                LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Word,
                    content: b"Hello".to_vec(),
                },
                LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Word,
                    content: b"World".to_vec(),
                },
            ],
        );
        let display = format!("{}", ops);
        assert!(display.contains("insert"));
        assert!(display.contains("2 tokens"));
    }

    #[test]
    fn test_line_ops_into_parts() {
        let ops = LineOps::delete(test_branch_id(7), vec![]);
        let (branch_id, operation) = ops.into_parts();

        assert_eq!(branch_id, test_branch_id(7));
        assert!(matches!(operation, BranchOp::Delete { .. }));
    }

    #[test]
    fn test_line_ops_content_range() {
        use crate::types::ChangePosition;

        let mut ops = LineOps::insert(test_branch_id(0), None, vec![]);

        // Initially no content range
        assert!(!ops.has_content_range());
        assert!(ops.content_range().is_none());

        // Set content range
        let start = ChangePosition::new(100);
        let end = ChangePosition::new(150);
        ops.set_content_range(start, end);

        // Now has content range
        assert!(ops.has_content_range());
        let range = ops.content_range().unwrap();
        assert_eq!(range.0.get(), 100);
        assert_eq!(range.1.get(), 150);
    }

    #[test]
    fn test_line_ops_with_content_range_builder() {
        use crate::types::ChangePosition;

        let start = ChangePosition::new(200);
        let end = ChangePosition::new(250);

        let ops = LineOps::insert(test_branch_id(1), None, vec![]).with_content_range(start, end);

        assert!(ops.has_content_range());
        let range = ops.content_range().unwrap();
        assert_eq!(range.0.get(), 200);
        assert_eq!(range.1.get(), 250);
    }

    #[test]
    fn test_line_ops_content_range_serialization() {
        use crate::types::ChangePosition;

        let mut ops = LineOps::insert(test_branch_id(0), None, vec![]);
        ops.set_content_range(ChangePosition::new(10), ChangePosition::new(20));

        // JSON roundtrip
        let json = serde_json::to_string(&ops).unwrap();
        let deserialized: LineOps = serde_json::from_str(&json).unwrap();

        assert!(deserialized.has_content_range());
        let range = deserialized.content_range().unwrap();
        assert_eq!(range.0.get(), 10);
        assert_eq!(range.1.get(), 20);
    }

    // =========================================================================
    // FileOpsStats Tests
    // =========================================================================

    #[test]
    fn test_stats_empty() {
        let stats = FileOpsStats::new();

        assert_eq!(stats.total_file_ops(), 0);
        assert_eq!(stats.total_line_ops(), 0);
        assert_eq!(stats.total_token_ops(), 0);
        assert!(!stats.has_changes());
    }

    #[test]
    fn test_stats_from_file_ops() {
        let mut file1 =
            FileOps::create(test_trunk_id(), "new.txt".to_string(), Some(Encoding::Utf8));
        file1.add_line_op(LineOps::insert(
            test_branch_id(0),
            None,
            vec![
                LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Word,
                    content: b"Hello".to_vec(),
                },
                LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Word,
                    content: b"World".to_vec(),
                },
            ],
        ));

        let file2 = FileOps::delete(TrunkId::new(NodeId::new(2), 0), "old.txt".to_string());

        let stats = FileOpsStats::from_file_ops(&[file1, file2]);

        assert_eq!(stats.files_created, 1);
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.lines_added, 1);
        assert_eq!(stats.tokens_added, 2);
        assert!(stats.has_changes());
    }

    #[test]
    fn test_stats_display() {
        let mut stats = FileOpsStats::new();
        stats.files_created = 2;
        stats.lines_added = 10;
        stats.tokens_added = 50;

        let display = format!("{}", stats);
        assert!(display.contains("2 files"));
        assert!(display.contains("10 lines"));
        assert!(display.contains("50 tokens"));
    }

    // =========================================================================
    // Serialization Tests
    // =========================================================================

    #[test]
    fn test_file_ops_serialize_roundtrip() {
        let mut ops = FileOps::create(
            test_trunk_id(),
            "test.txt".to_string(),
            Some(Encoding::Utf8),
        );
        ops.add_line_op(LineOps::insert(
            test_branch_id(0),
            None,
            vec![LeafOp::Insert {
                after: None,
                kind: TokenKind::Word,
                content: b"Hello".to_vec(),
            }],
        ));

        let json = serde_json::to_string(&ops).unwrap();
        let deserialized: FileOps = serde_json::from_str(&json).unwrap();

        assert_eq!(ops, deserialized);
    }

    #[test]
    fn test_line_ops_serialize_roundtrip() {
        let ops = LineOps::insert(
            test_branch_id(0),
            Some(test_branch_id(1)),
            vec![
                LeafOp::Insert {
                    after: None,
                    kind: TokenKind::Word,
                    content: b"Hello".to_vec(),
                },
                LeafOp::Delete {
                    leaf: crate::crdt::LeafId::new(NodeId::new(1), 3),
                },
            ],
        );

        let json = serde_json::to_string(&ops).unwrap();
        let deserialized: LineOps = serde_json::from_str(&json).unwrap();

        assert_eq!(ops, deserialized);
    }

    #[test]
    fn test_file_ops_postcard_roundtrip() {
        let mut ops = FileOps::create(test_trunk_id(), "binary.dat".to_string(), None);
        ops.add_line_op(LineOps::delete(test_branch_id(5), vec![]));

        let bytes = postcard::to_allocvec(&ops).unwrap();
        let deserialized: FileOps = postcard::from_bytes(&bytes).unwrap();

        assert_eq!(ops, deserialized);
    }
}

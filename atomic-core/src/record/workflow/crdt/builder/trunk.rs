//! File-level (trunk) operations for the CRDT change builder.

use crate::change::ops as change_ops;
use crate::change::Encoding;
use crate::crdt::{TrunkId, TrunkOp};

use super::branch::LineOps;

// ============================================================================
// FILE OPS
// ============================================================================

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
    pub(crate) line_ops: Vec<LineOps>,
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
    pub fn to_change_ops(&self) -> change_ops::FileOps {
        let mut result =
            change_ops::FileOps::new(self.trunk_id, self.path.clone(), self.trunk_op.clone());

        for line_op in &self.line_ops {
            let change_line_op =
                change_ops::LineOps::new(line_op.branch_id(), line_op.operation().clone());
            result.add_line_op(change_line_op);
        }

        result
    }

    /// Consume and convert to the serializable `change::ops::FileOps` type.
    pub fn into_change_ops(self) -> change_ops::FileOps {
        let mut result = change_ops::FileOps::new(self.trunk_id, self.path, self.trunk_op);

        for line_op in self.line_ops {
            let old_line_num = line_op.old_line_num();
            let new_line_num = line_op.new_line_num();
            let mut change_line_op =
                change_ops::LineOps::new(line_op.branch_id(), line_op.into_operation());
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

// ============================================================================
// BUILDER TRUNK METHODS
// ============================================================================

impl super::CrdtChangeBuilder {
    /// Adds a new file and returns its trunk ID.
    ///
    /// Creates a `TrunkOp::Create` for the file. Use [`add_line`](Self::add_line)
    /// to add content to the file.
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
    pub fn add_file_with_content(
        &mut self,
        path: &str,
        content: &[u8],
        encoding: Option<Encoding>,
    ) -> TrunkId {
        use super::super::tokenize::ContentTokenizer;
        use super::branch::LineOps;
        use crate::crdt::LeafOp;

        let trunk_id = self.add_file(path, encoding);

        let tokenizer = ContentTokenizer::new(content);
        let mut prev_branch: Option<crate::crdt::BranchId> = None;

        for (line_idx, line) in tokenizer.lines().enumerate() {
            let branch_id = self.alloc_branch_id();

            let mut leaf_ops = Vec::new();
            let mut prev_leaf: Option<crate::crdt::LeafId> = None;

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

            // Tag the LineOps with the new-file line number (1-indexed).
            // Globalize's `enrich_file_ops_for_add` matches LineOps by
            // `new_line_num` to populate `content_range`, which apply then
            // uses to wire `BRANCH_VERTEX`.  Without this tag, FileAdd
            // branches never get a BRANCH_VERTEX row — and the CRDT-driven
            // output walker raises `OrphanBranch` on every line.
            let line_op = LineOps::insert(branch_id, prev_branch, leaf_ops)
                .with_new_line_num(line_idx + 1);

            if let Some(&file_idx) = self.trunk_index.get(&trunk_id) {
                let inner_line_idx = self.file_ops[file_idx].line_ops.len();
                self.branch_index.insert(branch_id, (file_idx, inner_line_idx));
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
}

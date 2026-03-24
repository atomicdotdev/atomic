//! Integration tests for the CRDT record → apply workflow.
//!
//! These tests validate that the Phase 10.3 record workflow and
//! Phase 10.4 apply workflow integrate correctly end-to-end.

use std::collections::HashMap;
use std::ops::Range;

use atomic_core::change::Encoding;
use atomic_core::crdt::apply::{
    apply_branch_op, apply_leaf_op, apply_trunk_op, ApplyContext, ApplyOptions, MutCrdtTxnT,
};
use atomic_core::crdt::{
    Branch, BranchId, BranchOp, BranchState, Leaf, LeafId, LeafOp, LeafState, Trunk, TrunkId,
    TrunkOp, TrunkState,
};
use atomic_core::diff::token::TokenKind;
use atomic_core::pristine::PristineError;
use atomic_core::record::workflow::crdt::{AnalysisOptions, CrdtChangeBuilder, LineAnalyzer};
use atomic_core::types::{Inode, NodeId};

// Mock Transaction Implementation

/// A mock CRDT transaction for testing the apply workflow.
#[derive(Debug, Default)]
struct MockCrdtTxn {
    trunks: HashMap<TrunkId, Trunk>,
    branches: HashMap<BranchId, Branch>,
    leaves: HashMap<LeafId, Leaf>,
    path_index: HashMap<String, TrunkId>,
    inode_index: HashMap<Inode, TrunkId>,
    trunk_branches: HashMap<TrunkId, Vec<BranchId>>,
    branch_leaves: HashMap<BranchId, Vec<LeafId>>,
    next_inode: u64,
}

impl MockCrdtTxn {
    fn new() -> Self {
        Self {
            next_inode: 1,
            ..Default::default()
        }
    }

    /// Count trunks
    fn trunk_count(&self) -> usize {
        self.trunks.len()
    }

    /// Count branches for a trunk
    fn branch_count_for_trunk(&self, trunk_id: &TrunkId) -> usize {
        self.trunk_branches
            .get(trunk_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Count leaves for a branch
    fn leaf_count_for_branch(&self, branch_id: &BranchId) -> usize {
        self.branch_leaves
            .get(branch_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Get trunk by path
    fn get_trunk_by_path_helper(&self, path: &str) -> Option<&Trunk> {
        self.path_index.get(path).and_then(|id| self.trunks.get(id))
    }
}

impl MutCrdtTxnT for MockCrdtTxn {
    type Error = PristineError;

    fn put_trunk(&mut self, trunk: &Trunk) -> Result<bool, Self::Error> {
        let id = trunk.id();
        let is_new = !self.trunks.contains_key(&id);
        self.trunks.insert(id, trunk.clone());
        self.path_index.insert(trunk.path().to_string(), id);
        self.inode_index.insert(trunk.inode(), id);
        Ok(is_new)
    }

    fn get_trunk(&self, id: TrunkId) -> Result<Option<Trunk>, Self::Error> {
        Ok(self.trunks.get(&id).cloned())
    }

    fn has_trunk(&self, id: TrunkId) -> Result<bool, Self::Error> {
        Ok(self.trunks.contains_key(&id))
    }

    fn del_trunk(&mut self, id: TrunkId) -> Result<bool, Self::Error> {
        if let Some(trunk) = self.trunks.remove(&id) {
            self.path_index.remove(trunk.path());
            self.inode_index.remove(&trunk.inode());
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn update_trunk_state(&mut self, id: TrunkId, state: TrunkState) -> Result<(), Self::Error> {
        if let Some(trunk) = self.trunks.get_mut(&id) {
            trunk.set_state(state);
        }
        Ok(())
    }

    fn update_trunk_path(&mut self, id: TrunkId, new_path: &str) -> Result<(), Self::Error> {
        if let Some(trunk) = self.trunks.get_mut(&id) {
            self.path_index.remove(trunk.path());
            trunk.set_path(new_path.to_string());
            self.path_index.insert(new_path.to_string(), id);
        }
        Ok(())
    }

    fn get_trunk_by_path(&self, path: &str) -> Result<Option<TrunkId>, Self::Error> {
        Ok(self.path_index.get(path).copied())
    }

    fn get_trunk_by_inode(&self, inode: Inode) -> Result<Option<TrunkId>, Self::Error> {
        Ok(self.inode_index.get(&inode).copied())
    }

    fn put_branch(
        &mut self,
        branch: &Branch,
        after: Option<BranchId>,
    ) -> Result<bool, Self::Error> {
        let id = branch.id();
        let is_new = !self.branches.contains_key(&id);
        self.branches.insert(id, branch.clone());

        let branches = self.trunk_branches.entry(branch.trunk()).or_default();
        if let Some(after_id) = after {
            if let Some(pos) = branches.iter().position(|b| *b == after_id) {
                branches.insert(pos + 1, id);
            } else {
                branches.push(id);
            }
        } else {
            branches.insert(0, id);
        }
        Ok(is_new)
    }

    fn get_branch(&self, id: BranchId) -> Result<Option<Branch>, Self::Error> {
        Ok(self.branches.get(&id).cloned())
    }

    fn has_branch(&self, id: BranchId) -> Result<bool, Self::Error> {
        Ok(self.branches.contains_key(&id))
    }

    fn del_branch(&mut self, id: BranchId) -> Result<bool, Self::Error> {
        if let Some(branch) = self.branches.remove(&id) {
            if let Some(branches) = self.trunk_branches.get_mut(&branch.trunk()) {
                branches.retain(|b| *b != id);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn update_branch_state(&mut self, id: BranchId, state: BranchState) -> Result<(), Self::Error> {
        if let Some(branch) = self.branches.get_mut(&id) {
            branch.set_state(state);
        }
        Ok(())
    }

    fn list_branches(&self, trunk_id: TrunkId) -> Result<Vec<BranchId>, Self::Error> {
        Ok(self
            .trunk_branches
            .get(&trunk_id)
            .cloned()
            .unwrap_or_default())
    }

    fn count_branches(&self, trunk_id: TrunkId) -> Result<usize, Self::Error> {
        Ok(self
            .trunk_branches
            .get(&trunk_id)
            .map(|v| v.len())
            .unwrap_or(0))
    }

    fn put_leaf(&mut self, leaf: &Leaf, after: Option<LeafId>) -> Result<bool, Self::Error> {
        let id = leaf.id();
        let is_new = !self.leaves.contains_key(&id);
        self.leaves.insert(id, leaf.clone());

        let leaves = self.branch_leaves.entry(leaf.branch()).or_default();
        if let Some(after_id) = after {
            if let Some(pos) = leaves.iter().position(|l| *l == after_id) {
                leaves.insert(pos + 1, id);
            } else {
                leaves.push(id);
            }
        } else {
            leaves.insert(0, id);
        }
        Ok(is_new)
    }

    fn get_leaf(&self, id: LeafId) -> Result<Option<Leaf>, Self::Error> {
        Ok(self.leaves.get(&id).cloned())
    }

    fn has_leaf(&self, id: LeafId) -> Result<bool, Self::Error> {
        Ok(self.leaves.contains_key(&id))
    }

    fn del_leaf(&mut self, id: LeafId) -> Result<bool, Self::Error> {
        if let Some(leaf) = self.leaves.remove(&id) {
            if let Some(leaves) = self.branch_leaves.get_mut(&leaf.branch()) {
                leaves.retain(|l| *l != id);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn update_leaf_state(&mut self, id: LeafId, state: LeafState) -> Result<(), Self::Error> {
        if let Some(leaf) = self.leaves.get_mut(&id) {
            leaf.set_state(state);
        }
        Ok(())
    }

    fn update_leaf_content(&mut self, id: LeafId, range: Range<u32>) -> Result<(), Self::Error> {
        if let Some(leaf) = self.leaves.get_mut(&id) {
            leaf.set_content_range(range);
        }
        Ok(())
    }

    fn list_leaves(&self, branch_id: BranchId) -> Result<Vec<LeafId>, Self::Error> {
        Ok(self
            .branch_leaves
            .get(&branch_id)
            .cloned()
            .unwrap_or_default())
    }

    fn count_leaves(&self, branch_id: BranchId) -> Result<usize, Self::Error> {
        Ok(self
            .branch_leaves
            .get(&branch_id)
            .map(|v| v.len())
            .unwrap_or(0))
    }

    fn alloc_inode(&mut self) -> Result<Inode, Self::Error> {
        let inode = Inode::new(self.next_inode);
        self.next_inode += 1;
        Ok(inode)
    }
}

// Integration Tests

/// Test that we can build CRDT operations for a new file and apply them.
#[test]
fn test_record_and_apply_new_file() {
    let change_id = NodeId::new(1);
    let mut builder = CrdtChangeBuilder::new(change_id);

    // Record: Add a new file
    let trunk_id = builder.add_file("src/main.rs", Some(Encoding::Utf8));

    // Add a line with tokens
    let branch_id = builder.add_line(trunk_id, None);
    builder.add_token(branch_id, None, TokenKind::Word, b"fn");
    builder.add_token(branch_id, None, TokenKind::Whitespace, b" ");
    builder.add_token(branch_id, None, TokenKind::Word, b"main");

    // Finish building
    let result = builder.finish();

    // Verify build stats
    assert_eq!(result.stats().files_added, 1);
    assert_eq!(result.stats().lines_added, 1);
    assert_eq!(result.stats().tokens_added, 3);

    // Now apply to a mock transaction
    let mut txn = MockCrdtTxn::new();
    let mut ctx = ApplyContext::new(ApplyOptions::default());

    // Apply trunk operation
    for file_ops in result.file_ops() {
        if let Some(trunk_op) = file_ops.trunk_op() {
            apply_trunk_op(&mut txn, &mut ctx, file_ops.trunk_id(), trunk_op).unwrap();
        }

        // Apply line operations
        for line_ops in file_ops.line_ops() {
            apply_branch_op(
                &mut txn,
                &mut ctx,
                file_ops.trunk_id(),
                line_ops.branch_id(),
                line_ops.operation(),
                result.content(),
            )
            .unwrap();
        }
    }

    // Verify the applied state
    assert_eq!(txn.trunk_count(), 1);

    let trunk = txn.get_trunk_by_path_helper("src/main.rs").unwrap();
    assert_eq!(trunk.path(), "src/main.rs");
    assert_eq!(trunk.state(), TrunkState::Alive);
    assert_eq!(trunk.encoding(), Some(Encoding::Utf8));

    // Verify context stats
    let outcome = ctx.finish();
    assert_eq!(outcome.stats().trunks_created(), 1);
    assert_eq!(outcome.stats().branches_inserted(), 1);
}

/// Test that we can apply file deletion.
#[test]
fn test_apply_file_deletion() {
    let change_id = NodeId::new(1);

    // First, create a file
    let mut txn = MockCrdtTxn::new();
    let mut ctx = ApplyContext::new(ApplyOptions::default());

    let trunk_id = TrunkId::new(change_id, 0);
    let create_op = TrunkOp::Create {
        path: "src/lib.rs".to_string(),
        encoding: Some(Encoding::Utf8),
    };
    apply_trunk_op(&mut txn, &mut ctx, trunk_id, &create_op).unwrap();

    // Verify file exists
    assert!(txn.has_trunk(trunk_id).unwrap());
    assert_eq!(
        txn.get_trunk_by_path_helper("src/lib.rs").unwrap().state(),
        TrunkState::Alive
    );

    // Now delete the file
    let delete_op = TrunkOp::Delete { trunk: trunk_id };
    apply_trunk_op(&mut txn, &mut ctx, trunk_id, &delete_op).unwrap();

    // Verify file is marked as deleted (not removed)
    assert!(txn.has_trunk(trunk_id).unwrap());
    let trunk = txn.get_trunk(trunk_id).unwrap().unwrap();
    assert_eq!(trunk.state(), TrunkState::Deleted);
}

/// Test that we can apply file move/rename.
#[test]
fn test_apply_file_move() {
    let change_id = NodeId::new(1);

    // First, create a file
    let mut txn = MockCrdtTxn::new();
    let mut ctx = ApplyContext::new(ApplyOptions::default());

    let trunk_id = TrunkId::new(change_id, 0);
    let create_op = TrunkOp::Create {
        path: "src/old.rs".to_string(),
        encoding: Some(Encoding::Utf8),
    };
    apply_trunk_op(&mut txn, &mut ctx, trunk_id, &create_op).unwrap();

    // Verify old path exists
    assert!(txn.get_trunk_by_path("src/old.rs").unwrap().is_some());

    // Move the file
    let move_op = TrunkOp::Move {
        trunk: trunk_id,
        new_path: "src/new.rs".to_string(),
    };
    apply_trunk_op(&mut txn, &mut ctx, trunk_id, &move_op).unwrap();

    // Verify path changed
    assert!(txn.get_trunk_by_path("src/old.rs").unwrap().is_none());
    assert!(txn.get_trunk_by_path("src/new.rs").unwrap().is_some());

    let trunk = txn.get_trunk(trunk_id).unwrap().unwrap();
    assert_eq!(trunk.path(), "src/new.rs");
}

/// Test that we can undelete a file.
#[test]
fn test_apply_file_undelete() {
    let change_id = NodeId::new(1);

    // Create and delete a file
    let mut txn = MockCrdtTxn::new();
    let mut ctx = ApplyContext::new(ApplyOptions::default());

    let trunk_id = TrunkId::new(change_id, 0);
    let create_op = TrunkOp::Create {
        path: "src/restored.rs".to_string(),
        encoding: Some(Encoding::Utf8),
    };
    apply_trunk_op(&mut txn, &mut ctx, trunk_id, &create_op).unwrap();

    let delete_op = TrunkOp::Delete { trunk: trunk_id };
    apply_trunk_op(&mut txn, &mut ctx, trunk_id, &delete_op).unwrap();

    // Verify deleted
    assert_eq!(
        txn.get_trunk(trunk_id).unwrap().unwrap().state(),
        TrunkState::Deleted
    );

    // Undelete
    let undelete_op = TrunkOp::Undelete { trunk: trunk_id };
    apply_trunk_op(&mut txn, &mut ctx, trunk_id, &undelete_op).unwrap();

    // Verify restored
    assert_eq!(
        txn.get_trunk(trunk_id).unwrap().unwrap().state(),
        TrunkState::Alive
    );
}

/// Test applying line operations (branch level).
#[test]
fn test_apply_branch_operations() {
    let change_id = NodeId::new(1);

    let mut txn = MockCrdtTxn::new();
    let mut ctx = ApplyContext::new(ApplyOptions::default());

    // Create a file first
    let trunk_id = TrunkId::new(change_id, 0);
    let create_op = TrunkOp::Create {
        path: "src/lines.rs".to_string(),
        encoding: Some(Encoding::Utf8),
    };
    apply_trunk_op(&mut txn, &mut ctx, trunk_id, &create_op).unwrap();

    let content: &[u8] = &[];

    // Insert first line
    let branch_id_1 = BranchId::new(change_id, 0);
    let insert_op_1 = BranchOp::Insert {
        after: None, // Insert at start
        content: vec![],
    };
    apply_branch_op(
        &mut txn,
        &mut ctx,
        trunk_id,
        branch_id_1,
        &insert_op_1,
        content,
    )
    .unwrap();

    // Insert second line after first
    let branch_id_2 = BranchId::new(change_id, 1);
    let insert_op_2 = BranchOp::Insert {
        after: Some(branch_id_1),
        content: vec![],
    };
    apply_branch_op(
        &mut txn,
        &mut ctx,
        trunk_id,
        branch_id_2,
        &insert_op_2,
        content,
    )
    .unwrap();

    // Verify branches were created
    assert_eq!(txn.branch_count_for_trunk(&trunk_id), 2);
    assert!(txn.has_branch(branch_id_1).unwrap());
    assert!(txn.has_branch(branch_id_2).unwrap());

    // Delete first line
    let delete_op = BranchOp::Delete {
        branch: branch_id_1,
        content: vec![],
    };
    apply_branch_op(
        &mut txn,
        &mut ctx,
        trunk_id,
        branch_id_1,
        &delete_op,
        content,
    )
    .unwrap();

    // Verify first line is marked deleted
    let branch_1 = txn.get_branch(branch_id_1).unwrap().unwrap();
    assert_eq!(branch_1.state(), BranchState::Deleted);

    // Second line should still be alive
    let branch_2 = txn.get_branch(branch_id_2).unwrap().unwrap();
    assert_eq!(branch_2.state(), BranchState::Alive);
}

/// Test applying token operations (leaf level).
#[test]
fn test_apply_leaf_operations() {
    let change_id = NodeId::new(1);

    let mut txn = MockCrdtTxn::new();
    let content: &[u8] = &[];
    let mut ctx = ApplyContext::new(ApplyOptions::default());

    // Create file and line
    let trunk_id = TrunkId::new(change_id, 0);
    let create_op = TrunkOp::Create {
        path: "src/tokens.rs".to_string(),
        encoding: Some(Encoding::Utf8),
    };
    apply_trunk_op(&mut txn, &mut ctx, trunk_id, &create_op).unwrap();

    let branch_id = BranchId::new(change_id, 0);
    let insert_branch = BranchOp::Insert {
        after: None,
        content: vec![],
    };
    apply_branch_op(
        &mut txn,
        &mut ctx,
        trunk_id,
        branch_id,
        &insert_branch,
        content,
    )
    .unwrap();

    // Insert tokens using content bytes directly
    let leaf_id_1 = LeafId::new(change_id, 0);
    let insert_leaf_1 = LeafOp::Insert {
        after: None,
        kind: TokenKind::Word,
        content: b"fn".to_vec(),
    };
    apply_leaf_op(
        &mut txn,
        &mut ctx,
        branch_id,
        leaf_id_1,
        &insert_leaf_1,
        content,
    )
    .unwrap();

    let leaf_id_2 = LeafId::new(change_id, 1);
    let insert_leaf_2 = LeafOp::Insert {
        after: Some(leaf_id_1),
        kind: TokenKind::Whitespace,
        content: b" ".to_vec(),
    };
    apply_leaf_op(
        &mut txn,
        &mut ctx,
        branch_id,
        leaf_id_2,
        &insert_leaf_2,
        content,
    )
    .unwrap();

    let leaf_id_3 = LeafId::new(change_id, 2);
    let insert_leaf_3 = LeafOp::Insert {
        after: Some(leaf_id_2),
        kind: TokenKind::Word,
        content: b"main".to_vec(),
    };
    apply_leaf_op(
        &mut txn,
        &mut ctx,
        branch_id,
        leaf_id_3,
        &insert_leaf_3,
        content,
    )
    .unwrap();

    // Verify leaves were created
    assert_eq!(txn.leaf_count_for_branch(&branch_id), 3);

    // Verify leaf kinds
    let leaf_1 = txn.get_leaf(leaf_id_1).unwrap().unwrap();
    assert_eq!(leaf_1.kind(), TokenKind::Word);

    let leaf_3 = txn.get_leaf(leaf_id_3).unwrap().unwrap();
    assert_eq!(leaf_3.kind(), TokenKind::Word);

    // Delete a token
    let delete_leaf = LeafOp::Delete { leaf: leaf_id_2 };
    apply_leaf_op(
        &mut txn,
        &mut ctx,
        branch_id,
        leaf_id_2,
        &delete_leaf,
        content,
    )
    .unwrap();

    // Verify token is marked deleted (not removed)
    let leaf_2 = txn.get_leaf(leaf_id_2).unwrap().unwrap();
    assert_eq!(leaf_2.state(), LeafState::Deleted);
}

/// Test the Replace operation preserves leaf ID for blame tracking.
#[test]
fn test_apply_leaf_replace_preserves_id() {
    let change_id = NodeId::new(1);

    let mut txn = MockCrdtTxn::new();
    let content: &[u8] = &[];
    let mut ctx = ApplyContext::new(ApplyOptions::default());

    // Create file, line, and token
    let trunk_id = TrunkId::new(change_id, 0);
    let create_op = TrunkOp::Create {
        path: "src/replace.rs".to_string(),
        encoding: Some(Encoding::Utf8),
    };
    apply_trunk_op(&mut txn, &mut ctx, trunk_id, &create_op).unwrap();

    let branch_id = BranchId::new(change_id, 0);
    let insert_branch = BranchOp::Insert {
        after: None,
        content: vec![],
    };
    apply_branch_op(
        &mut txn,
        &mut ctx,
        trunk_id,
        branch_id,
        &insert_branch,
        content,
    )
    .unwrap();

    let leaf_id = LeafId::new(change_id, 0);
    let insert_leaf = LeafOp::Insert {
        after: None,
        kind: TokenKind::Word,
        content: b"old_value".to_vec(),
    };
    apply_leaf_op(
        &mut txn,
        &mut ctx,
        branch_id,
        leaf_id,
        &insert_leaf,
        content,
    )
    .unwrap();

    // Verify original leaf exists
    let leaf = txn.get_leaf(leaf_id).unwrap().unwrap();
    assert_eq!(leaf.kind(), TokenKind::Word);
    assert_eq!(leaf.state(), LeafState::Alive);

    // Replace content (preserves ID for blame)
    let replace_op = LeafOp::Replace {
        leaf: leaf_id,
        new_content: b"new_value".to_vec(),
    };
    apply_leaf_op(&mut txn, &mut ctx, branch_id, leaf_id, &replace_op, content).unwrap();

    // Verify same ID still exists
    let leaf_after = txn.get_leaf(leaf_id).unwrap().unwrap();
    // ID is still the same - crucial for blame tracking!
    assert_eq!(leaf_after.branch(), branch_id);
    assert_eq!(leaf_after.state(), LeafState::Alive);
}

/// Test line analysis for modifications.
#[test]
fn test_line_analysis_diff() {
    let old_content = b"line one\nline two\nline three\n";
    let new_content = b"line one\nmodified line\nline three\nnew line\n";

    // Analyze differences
    let analyzer = LineAnalyzer::new(old_content, new_content, AnalysisOptions::default());
    let analysis = analyzer.analyze();

    // Verify analysis found changes
    let stats = analysis.stats();
    assert!(stats.total_changes > 0);

    // Verify we detected the modifications
    assert!(stats.inserted_lines > 0 || stats.modified_lines > 0);
}

/// Test that the builder correctly tracks statistics.
#[test]
fn test_builder_statistics() {
    let change_id = NodeId::new(1);
    let mut builder = CrdtChangeBuilder::new(change_id);

    // Add multiple files
    let trunk_1 = builder.add_file("file1.rs", Some(Encoding::Utf8));
    let trunk_2 = builder.add_file("file2.rs", Some(Encoding::Utf8));

    // Add lines to first file
    let branch_1 = builder.add_line(trunk_1, None);
    let branch_2 = builder.add_line(trunk_1, None);

    // Add lines to second file
    let branch_3 = builder.add_line(trunk_2, None);

    // Add tokens
    builder.add_token(branch_1, None, TokenKind::Word, b"hello");
    builder.add_token(branch_1, None, TokenKind::Whitespace, b" ");
    builder.add_token(branch_2, None, TokenKind::Word, b"world");
    builder.add_token(branch_3, None, TokenKind::Number, b"42");

    let result = builder.finish();

    // Verify statistics
    assert_eq!(result.stats().files_added, 2);
    assert_eq!(result.stats().lines_added, 3);
    assert_eq!(result.stats().tokens_added, 4);
    assert!(result.stats().content_bytes > 0);
    assert!(result.stats().has_changes());
}

/// Test inode allocation works correctly.
#[test]
fn test_inode_allocation() {
    let change_id = NodeId::new(1);

    let mut txn = MockCrdtTxn::new();
    let mut ctx = ApplyContext::new(ApplyOptions::default());

    // Create multiple files and verify unique inodes
    for i in 0..5 {
        let trunk_id = TrunkId::new(change_id, i);
        let create_op = TrunkOp::Create {
            path: format!("file{}.rs", i),
            encoding: Some(Encoding::Utf8),
        };
        apply_trunk_op(&mut txn, &mut ctx, trunk_id, &create_op).unwrap();
    }

    // Verify all files have unique inodes
    let mut inodes = std::collections::HashSet::new();
    for i in 0..5 {
        let trunk_id = TrunkId::new(change_id, i);
        let trunk = txn.get_trunk(trunk_id).unwrap().unwrap();
        assert!(inodes.insert(trunk.inode()), "Duplicate inode detected!");
    }
}

/// Test full workflow: multiple files, lines, and tokens.
#[test]
fn test_multi_file_workflow() {
    let change_id = NodeId::new(1);
    let mut builder = CrdtChangeBuilder::new(change_id);

    // Create two files with content
    let trunk_1 = builder.add_file("src/lib.rs", Some(Encoding::Utf8));
    let trunk_2 = builder.add_file("src/main.rs", Some(Encoding::Utf8));

    // lib.rs: pub mod utils;
    let branch_lib = builder.add_line(trunk_1, None);
    builder.add_token(branch_lib, None, TokenKind::Word, b"pub");
    builder.add_token(branch_lib, None, TokenKind::Whitespace, b" ");
    builder.add_token(branch_lib, None, TokenKind::Word, b"mod");
    builder.add_token(branch_lib, None, TokenKind::Whitespace, b" ");
    builder.add_token(branch_lib, None, TokenKind::Word, b"utils");

    // main.rs: fn main() {}
    let branch_main = builder.add_line(trunk_2, None);
    builder.add_token(branch_main, None, TokenKind::Word, b"fn");
    builder.add_token(branch_main, None, TokenKind::Whitespace, b" ");
    builder.add_token(branch_main, None, TokenKind::Word, b"main");

    let result = builder.finish();

    // Apply all operations
    let mut txn = MockCrdtTxn::new();
    let mut ctx = ApplyContext::new(ApplyOptions::default());

    for file_ops in result.file_ops() {
        if let Some(trunk_op) = file_ops.trunk_op() {
            apply_trunk_op(&mut txn, &mut ctx, file_ops.trunk_id(), trunk_op).unwrap();
        }

        for line_ops in file_ops.line_ops() {
            apply_branch_op(
                &mut txn,
                &mut ctx,
                file_ops.trunk_id(),
                line_ops.branch_id(),
                line_ops.operation(),
                result.content(),
            )
            .unwrap();
        }
    }

    // Verify both files exist
    assert_eq!(txn.trunk_count(), 2);
    assert!(txn.get_trunk_by_path_helper("src/lib.rs").is_some());
    assert!(txn.get_trunk_by_path_helper("src/main.rs").is_some());

    // Verify branches
    assert_eq!(txn.branch_count_for_trunk(&trunk_1), 1);
    assert_eq!(txn.branch_count_for_trunk(&trunk_2), 1);

    let outcome = ctx.finish();
    assert_eq!(outcome.stats().trunks_created(), 2);
    assert_eq!(outcome.stats().branches_inserted(), 2);
    assert!(!outcome.has_conflicts());
}

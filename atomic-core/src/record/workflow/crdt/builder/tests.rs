//! Tests for the CRDT change builder module.

#[cfg(test)]
mod tests {
    use crate::change::Encoding;
    use crate::crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId, TrunkOp};
    use crate::diff::token::TokenKind;
    use crate::record::workflow::crdt::builder::{
        CrdtBuildError, CrdtBuildStats, CrdtChangeBuilder, CrdtChangeResult, FileOps, LineOps,
        TokenOps,
    };
    use crate::record::workflow::crdt::line_ops::{LineChange, LineChangeKind};
    use crate::types::NodeId;

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

        let _trunk_id = builder.add_file("main.rs", Some(Encoding::Utf8));
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
        let _trunk_id = builder.add_file_with_content("test.txt", content, None);

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
        let _branch_id = builder.add_line_with_content(trunk_id, None, b"let x = 42;");

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

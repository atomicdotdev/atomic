#![allow(unused_imports)]
use super::*;
use crate::change::Encoding;
use crate::crdt::{BranchId, BranchOp, LeafId, LeafOp, TrunkId, TrunkOp};
use crate::diff::token::TokenKind;
use crate::types::NodeId;

// ------------------------------------------------------------------------
// ConversionOptions Tests
// ------------------------------------------------------------------------

#[test]
fn test_conversion_options_default() {
    let opts = ConversionOptions::default();
    assert!(opts.tokenize_content());
    assert!(opts.preserve_whitespace());
    assert!(opts.code_aware());
    assert!(opts.include_empty_lines());
    assert_eq!(
        opts.max_tokenize_size(),
        ConversionOptions::DEFAULT_MAX_TOKENIZE_SIZE
    );
}

#[test]
fn test_conversion_options_new() {
    let opts = ConversionOptions::new();
    assert!(opts.tokenize_content());
}

#[test]
fn test_conversion_options_builder_tokenize_content() {
    let opts = ConversionOptions::new().with_tokenize_content(false);
    assert!(!opts.tokenize_content());
}

#[test]
fn test_conversion_options_builder_preserve_whitespace() {
    let opts = ConversionOptions::new().with_preserve_whitespace(false);
    assert!(!opts.preserve_whitespace());
}

#[test]
fn test_conversion_options_builder_code_aware() {
    let opts = ConversionOptions::new().with_code_aware(false);
    assert!(!opts.code_aware());
}

#[test]
fn test_conversion_options_builder_include_empty_lines() {
    let opts = ConversionOptions::new().with_include_empty_lines(false);
    assert!(!opts.include_empty_lines());
}

#[test]
fn test_conversion_options_builder_max_tokenize_size() {
    let opts = ConversionOptions::new().with_max_tokenize_size(5000);
    assert_eq!(opts.max_tokenize_size(), 5000);
}

#[test]
fn test_conversion_options_builder_chain() {
    let opts = ConversionOptions::new()
        .with_tokenize_content(false)
        .with_preserve_whitespace(false)
        .with_code_aware(false)
        .with_include_empty_lines(false)
        .with_max_tokenize_size(1000);

    assert!(!opts.tokenize_content());
    assert!(!opts.preserve_whitespace());
    assert!(!opts.code_aware());
    assert!(!opts.include_empty_lines());
    assert_eq!(opts.max_tokenize_size(), 1000);
}

#[test]
fn test_conversion_options_to_tokenize_options() {
    let opts = ConversionOptions::new()
        .with_preserve_whitespace(false)
        .with_code_aware(true);

    let tokenize_opts = opts.to_tokenize_options();
    assert!(tokenize_opts.merge_whitespace()); // Inverted from preserve
    assert!(tokenize_opts.code_aware());
}

// ------------------------------------------------------------------------
// ConvertError Tests
// ------------------------------------------------------------------------

#[test]
fn test_convert_error_unsupported_hunk_display() {
    let err = ConvertError::UnsupportedHunk {
        description: "unknown graph_op type".to_string(),
    };
    assert!(err.to_string().contains("unsupported"));
    assert!(err.to_string().contains("unknown graph_op type"));
}

#[test]
fn test_convert_error_content_too_large_display() {
    let err = ConvertError::ContentTooLarge {
        size: 2_000_000,
        max_size: 1_000_000,
    };
    assert!(err.to_string().contains("too large"));
    assert!(err.to_string().contains("2000000"));
}

#[test]
fn test_convert_error_missing_content_display() {
    let err = ConvertError::MissingContent {
        description: "file content".to_string(),
    };
    assert!(err.to_string().contains("missing"));
    assert!(err.to_string().contains("file content"));
}

#[test]
fn test_convert_error_invalid_state_display() {
    let err = ConvertError::InvalidState {
        description: "no active file".to_string(),
    };
    assert!(err.to_string().contains("invalid"));
}

#[test]
fn test_convert_error_tokenization_failed_display() {
    let err = ConvertError::TokenizationFailed {
        message: "bad encoding".to_string(),
    };
    assert!(err.to_string().contains("tokenization"));
}

#[test]
fn test_convert_error_is_error_trait() {
    let err = ConvertError::MissingContent {
        description: "test".to_string(),
    };
    let _: &dyn std::error::Error = &err;
}

// ------------------------------------------------------------------------
// ConversionStats Tests
// ------------------------------------------------------------------------

#[test]
fn test_conversion_stats_new() {
    let stats = ConversionStats::new();
    assert_eq!(stats.hunks_converted, 0);
    assert_eq!(stats.total_ops(), 0);
}

#[test]
fn test_conversion_stats_merge() {
    let mut stats1 = ConversionStats::new();
    stats1.hunks_converted = 2;
    stats1.trunk_ops = 1;
    stats1.branch_ops = 5;
    stats1.leaf_ops = 20;

    let mut stats2 = ConversionStats::new();
    stats2.hunks_converted = 3;
    stats2.trunk_ops = 2;
    stats2.branch_ops = 10;
    stats2.leaf_ops = 30;

    stats1.merge(&stats2);

    assert_eq!(stats1.hunks_converted, 5);
    assert_eq!(stats1.trunk_ops, 3);
    assert_eq!(stats1.branch_ops, 15);
    assert_eq!(stats1.leaf_ops, 50);
}

#[test]
fn test_conversion_stats_total_ops() {
    let mut stats = ConversionStats::new();
    stats.trunk_ops = 1;
    stats.branch_ops = 5;
    stats.leaf_ops = 20;

    assert_eq!(stats.total_ops(), 26);
}

#[test]
fn test_conversion_stats_display() {
    let mut stats = ConversionStats::new();
    stats.hunks_converted = 3;
    stats.trunk_ops = 1;
    stats.branch_ops = 10;
    stats.leaf_ops = 50;
    stats.content_bytes = 500;
    stats.lines_processed = 10;

    let display = format!("{}", stats);
    assert!(display.contains("3 hunks"));
    assert!(display.contains("1 trunk"));
    assert!(display.contains("10 branch"));
    assert!(display.contains("50 leaf"));
}

// ------------------------------------------------------------------------
// ConvertedOps Tests
// ------------------------------------------------------------------------

#[test]
fn test_converted_ops_new() {
    let ops = ConvertedOps::new();
    assert!(ops.is_empty());
    assert!(ops.trunk_ops().is_empty());
    assert!(ops.branch_ops().is_empty());
    assert!(ops.leaf_ops().is_empty());
    assert!(ops.content().is_empty());
}

#[test]
fn test_converted_ops_add_trunk_op() {
    let mut ops = ConvertedOps::new();
    ops.add_trunk_op(TrunkOp::Create {
        path: "test.rs".to_string(),
        encoding: None,
    });

    assert!(!ops.is_empty());
    assert_eq!(ops.trunk_ops().len(), 1);
    assert_eq!(ops.stats().trunk_ops, 1);
}

#[test]
fn test_converted_ops_add_branch_op() {
    let mut ops = ConvertedOps::new();
    let branch_id = BranchId::new(NodeId::new(1), 0);
    ops.add_branch_op(
        branch_id,
        BranchOp::Insert {
            after: None,
            content: vec![],
        },
    );

    assert!(!ops.is_empty());
    assert_eq!(ops.branch_ops().len(), 1);
    assert_eq!(ops.stats().branch_ops, 1);
}

#[test]
fn test_converted_ops_add_leaf_op() {
    let mut ops = ConvertedOps::new();
    let leaf_id = LeafId::new(NodeId::new(1), 0);
    ops.add_leaf_op(
        leaf_id,
        LeafOp::Insert {
            after: None,
            kind: TokenKind::Word,
            content: b"test".to_vec(),
        },
    );

    assert!(!ops.is_empty());
    assert_eq!(ops.leaf_ops().len(), 1);
    assert_eq!(ops.stats().leaf_ops, 1);
}

#[test]
fn test_converted_ops_append_content() {
    let mut ops = ConvertedOps::new();
    let range1 = ops.append_content(b"hello");
    let range2 = ops.append_content(b"world");

    assert_eq!(range1, 0..5);
    assert_eq!(range2, 5..10);
    assert_eq!(ops.content(), b"helloworld");
    assert_eq!(ops.stats().content_bytes, 10);
}

#[test]
fn test_converted_ops_merge() {
    let mut ops1 = ConvertedOps::new();
    ops1.add_trunk_op(TrunkOp::Create {
        path: "a.rs".to_string(),
        encoding: None,
    });
    ops1.append_content(b"aaa");

    let mut ops2 = ConvertedOps::new();
    ops2.add_trunk_op(TrunkOp::Create {
        path: "b.rs".to_string(),
        encoding: None,
    });
    ops2.append_content(b"bbb");

    ops1.merge(ops2);

    assert_eq!(ops1.trunk_ops().len(), 2);
    assert_eq!(ops1.content(), b"aaabbb");
    assert_eq!(ops1.stats().trunk_ops, 2);
}

#[test]
fn test_converted_ops_into_parts() {
    let mut ops = ConvertedOps::new();
    ops.add_trunk_op(TrunkOp::Create {
        path: "test.rs".to_string(),
        encoding: None,
    });
    ops.append_content(b"content");

    let (trunk_ops, branch_ops, leaf_ops, content) = ops.into_parts();

    assert_eq!(trunk_ops.len(), 1);
    assert!(branch_ops.is_empty());
    assert!(leaf_ops.is_empty());
    assert_eq!(content, b"content");
}

// ------------------------------------------------------------------------
// HunkConverter Tests
// ------------------------------------------------------------------------

#[test]
fn test_hunk_converter_new() {
    let change_id = NodeId::new(1);
    let converter = HunkConverter::new(change_id, ConversionOptions::default());

    assert_eq!(converter.change_id(), change_id);
}

#[test]
fn test_hunk_converter_with_defaults() {
    let change_id = NodeId::new(1);
    let converter = HunkConverter::with_defaults(change_id);

    assert_eq!(converter.change_id(), change_id);
    assert!(converter.options().tokenize_content());
}

#[test]
fn test_hunk_converter_convert_file_content_simple() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let content = b"hello world";
    let ops = converter.convert_file_content("test.txt", content, None);

    assert!(!ops.is_empty());
    assert_eq!(ops.trunk_ops().len(), 1);
    assert!(ops.branch_ops().len() >= 1);
    assert_eq!(ops.stats().files_added, 1);
}

#[test]
fn test_hunk_converter_convert_file_content_multiline() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let content = b"line one\nline two\nline three\n";
    let ops = converter.convert_file_content("test.txt", content, None);

    assert!(!ops.is_empty());
    assert_eq!(ops.trunk_ops().len(), 1);
    // Should have branches for each line (including empty trailing)
    assert!(ops.branch_ops().len() >= 3);
    assert!(ops.stats().lines_processed >= 3);
}

#[test]
fn test_hunk_converter_convert_file_content_with_encoding() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let content = b"fn main() {}";
    let ops = converter.convert_file_content("main.rs", content, Some(Encoding::Utf8));

    assert_eq!(ops.trunk_ops().len(), 1);
    match &ops.trunk_ops()[0] {
        TrunkOp::Create { path, encoding } => {
            assert_eq!(path, "main.rs");
            assert_eq!(*encoding, Some(Encoding::Utf8));
        }
        _ => panic!("Expected TrunkOp::Create"),
    }
}

#[test]
fn test_hunk_converter_convert_file_content_large_binary() {
    let change_id = NodeId::new(1);
    let opts = ConversionOptions::new().with_max_tokenize_size(100);
    let mut converter = HunkConverter::new(change_id, opts);

    // Create content larger than max_tokenize_size
    let content = vec![b'x'; 200];
    let ops = converter.convert_file_content("large.bin", &content, None);

    // Should have 1 trunk op and 1 branch op (binary mode)
    assert_eq!(ops.trunk_ops().len(), 1);
    assert_eq!(ops.branch_ops().len(), 1);
}

#[test]
fn test_hunk_converter_convert_file_deletion() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let trunk_id = TrunkId::new(NodeId::new(0), 0);
    let ops = converter.convert_file_deletion(trunk_id);

    assert_eq!(ops.trunk_ops().len(), 1);
    match &ops.trunk_ops()[0] {
        TrunkOp::Delete { trunk } => assert_eq!(*trunk, trunk_id),
        _ => panic!("Expected TrunkOp::Delete"),
    }
    assert_eq!(ops.stats().files_deleted, 1);
}

#[test]
fn test_hunk_converter_convert_file_move() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let trunk_id = TrunkId::new(NodeId::new(0), 0);
    let ops = converter.convert_file_move(trunk_id, "new/path.rs");

    assert_eq!(ops.trunk_ops().len(), 1);
    match &ops.trunk_ops()[0] {
        TrunkOp::Move { trunk, new_path } => {
            assert_eq!(*trunk, trunk_id);
            assert_eq!(new_path, "new/path.rs");
        }
        _ => panic!("Expected TrunkOp::Move"),
    }
    assert_eq!(ops.stats().files_moved, 1);
}

#[test]
fn test_hunk_converter_convert_file_undeletion() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let trunk_id = TrunkId::new(NodeId::new(0), 0);
    let ops = converter.convert_file_undeletion(trunk_id);

    assert_eq!(ops.trunk_ops().len(), 1);
    match &ops.trunk_ops()[0] {
        TrunkOp::Undelete { trunk } => assert_eq!(*trunk, trunk_id),
        _ => panic!("Expected TrunkOp::Undelete"),
    }
}

#[test]
fn test_hunk_converter_convert_line_insert() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let trunk_id = TrunkId::new(NodeId::new(0), 0);
    let ops = converter.convert_line_insert(trunk_id, None, b"new line content");

    assert_eq!(ops.branch_ops().len(), 1);
    assert!(ops.stats().lines_processed >= 1);
}

#[test]
fn test_hunk_converter_convert_line_insert_after() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let trunk_id = TrunkId::new(NodeId::new(0), 0);
    let after_branch = BranchId::new(NodeId::new(0), 5);
    let ops = converter.convert_line_insert(trunk_id, Some(after_branch), b"inserted");

    assert_eq!(ops.branch_ops().len(), 1);
    let (_, branch_op) = &ops.branch_ops()[0];
    match branch_op {
        BranchOp::Insert { after, .. } => assert_eq!(*after, Some(after_branch)),
        _ => panic!("Expected BranchOp::Insert"),
    }
}

#[test]
fn test_hunk_converter_convert_line_delete() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let branch_id = BranchId::new(NodeId::new(0), 3);
    let ops = converter.convert_line_delete(branch_id);

    assert_eq!(ops.branch_ops().len(), 1);
    let (id, branch_op) = &ops.branch_ops()[0];
    assert_eq!(*id, branch_id);
    match branch_op {
        BranchOp::Delete { branch, .. } => assert_eq!(*branch, branch_id),
        _ => panic!("Expected BranchOp::Delete"),
    }
}

#[test]
fn test_hunk_converter_convert_token_insert() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let branch_id = BranchId::new(NodeId::new(0), 0);
    let ops = converter.convert_token_insert(branch_id, None, TokenKind::Word, b"hello");

    assert_eq!(ops.leaf_ops().len(), 1);
    assert_eq!(ops.stats().tokens_generated, 1);
}

#[test]
fn test_hunk_converter_convert_token_delete() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let leaf_id = LeafId::new(NodeId::new(0), 5);
    let ops = converter.convert_token_delete(leaf_id);

    assert_eq!(ops.leaf_ops().len(), 1);
    let (id, leaf_op) = &ops.leaf_ops()[0];
    assert_eq!(*id, leaf_id);
    match leaf_op {
        LeafOp::Delete { leaf } => assert_eq!(*leaf, leaf_id),
        _ => panic!("Expected LeafOp::Delete"),
    }
}

#[test]
fn test_hunk_converter_convert_token_replace() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let leaf_id = LeafId::new(NodeId::new(0), 7);
    let ops = converter.convert_token_replace(leaf_id, b"new_value");

    assert_eq!(ops.leaf_ops().len(), 1);
    let (id, leaf_op) = &ops.leaf_ops()[0];
    assert_eq!(*id, leaf_id);
    match leaf_op {
        LeafOp::Replace { leaf, new_content } => {
            assert_eq!(*leaf, leaf_id);
            assert_eq!(new_content, b"new_value");
        }
        _ => panic!("Expected LeafOp::Replace"),
    }
}

#[test]
fn test_hunk_converter_reset_counters() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    // Generate some IDs
    converter.convert_file_content("test.rs", b"content", None);

    // Reset and verify we can generate again
    converter.reset_counters();
    let ops = converter.convert_file_content("test2.rs", b"more", None);

    // Should have IDs starting from 0 again
    assert!(!ops.is_empty());
}

#[test]
fn test_hunk_converter_no_tokenization() {
    let change_id = NodeId::new(1);
    let opts = ConversionOptions::new().with_tokenize_content(false);
    let mut converter = HunkConverter::new(change_id, opts);

    let ops = converter.convert_file_content("test.txt", b"hello world", None);

    // Without tokenization, each line should be a single leaf
    assert!(!ops.is_empty());
    // Check that we have fewer tokens than with tokenization
    let tokens = ops.stats().tokens_generated;
    assert!(tokens > 0);
}

// ------------------------------------------------------------------------
// Integration Tests
// ------------------------------------------------------------------------

#[test]
fn test_integration_full_file_workflow() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    // Add a file
    let content = b"fn main() {\n    println!(\"Hello\");\n}\n";
    let ops = converter.convert_file_content("main.rs", content, Some(Encoding::Utf8));

    // Verify structure
    assert_eq!(ops.trunk_ops().len(), 1);
    assert!(ops.branch_ops().len() >= 3); // At least 3 lines
    assert!(ops.stats().tokens_generated > 0);
    assert_eq!(ops.stats().files_added, 1);
}

#[test]
fn test_integration_empty_file() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let ops = converter.convert_file_content("empty.txt", b"", None);

    assert_eq!(ops.trunk_ops().len(), 1);
    // Empty file may have 0 branches depending on options
}

#[test]
fn test_integration_code_aware_tokenization() {
    let change_id = NodeId::new(1);
    let opts = ConversionOptions::new().with_code_aware(true);
    let mut converter = HunkConverter::new(change_id, opts);

    let content = b"let x = 42;";
    let ops = converter.convert_file_content("code.rs", content, None);

    // Code-aware should recognize operators and numbers
    assert!(!ops.is_empty());
    assert!(ops.stats().tokens_generated > 2); // More than just words
}

#[test]
fn test_integration_multiple_files() {
    let change_id = NodeId::new(1);
    let mut converter = HunkConverter::with_defaults(change_id);

    let ops1 = converter.convert_file_content("file1.rs", b"content1", None);
    let ops2 = converter.convert_file_content("file2.rs", b"content2", None);

    assert_eq!(ops1.trunk_ops().len(), 1);
    assert_eq!(ops2.trunk_ops().len(), 1);

    // Merge them
    let mut combined = ops1;
    combined.merge(ops2);

    assert_eq!(combined.trunk_ops().len(), 2);
    assert_eq!(combined.stats().files_added, 2);
}

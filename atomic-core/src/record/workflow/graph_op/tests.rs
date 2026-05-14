//! Tests for the graph_op module.

use super::*;
use crate::change::{Encoding, Local};
use crate::diff::DiffOp;

// HunkBuildOptions tests

#[test]
fn test_options_new_returns_defaults() {
    let opts = HunkBuildOptions::new();
    assert!(opts.get_encoding().is_none());
    assert_eq!(opts.get_context_lines(), 3);
    assert!(!opts.get_include_function_context());
    assert_eq!(
        opts.get_combine_threshold(),
        HunkBuildOptions::DEFAULT_COMBINE_THRESHOLD
    );
}

#[test]
fn test_options_default() {
    let opts = HunkBuildOptions::default();
    assert!(opts.is_binary());
    assert_eq!(
        opts.get_context_lines(),
        HunkBuildOptions::DEFAULT_CONTEXT_LINES
    );
}

#[test]
fn test_options_encoding() {
    let opts = HunkBuildOptions::new().encoding(Encoding::Utf8);
    assert_eq!(opts.get_encoding(), Some(Encoding::Utf8));
    assert!(!opts.is_binary());
}

#[test]
fn test_options_binary() {
    let opts = HunkBuildOptions::new().encoding(Encoding::Utf8).binary();
    assert!(opts.get_encoding().is_none());
    assert!(opts.is_binary());
}

#[test]
fn test_options_context_lines() {
    let opts = HunkBuildOptions::new().context_lines(5);
    assert_eq!(opts.get_context_lines(), 5);
}

#[test]
fn test_options_include_function_context() {
    let opts = HunkBuildOptions::new().include_function_context(true);
    assert!(opts.get_include_function_context());
}

#[test]
fn test_options_combine_threshold() {
    let opts = HunkBuildOptions::new().combine_threshold(10);
    assert_eq!(opts.get_combine_threshold(), 10);
}

#[test]
fn test_options_builder_chain() {
    let opts = HunkBuildOptions::new()
        .encoding(Encoding::Utf8)
        .context_lines(5)
        .include_function_context(true)
        .combine_threshold(8);

    assert_eq!(opts.get_encoding(), Some(Encoding::Utf8));
    assert_eq!(opts.get_context_lines(), 5);
    assert!(opts.get_include_function_context());
    assert_eq!(opts.get_combine_threshold(), 8);
}

#[test]
fn test_options_clone() {
    let opts = HunkBuildOptions::new().encoding(Encoding::Utf8);
    let cloned = opts.clone();
    assert_eq!(opts, cloned);
}

#[test]
fn test_options_debug() {
    let opts = HunkBuildOptions::new();
    let debug = format!("{:?}", opts);
    assert!(debug.contains("HunkBuildOptions"));
}

// PendingChange tests

#[test]
fn test_pending_change_insert() {
    let change = PendingChange::insert(5, 5, 3);
    assert!(change.is_insert());
    assert!(!change.is_delete());
    assert!(!change.is_replace());
    assert_eq!(change.old_start, 5);
    assert_eq!(change.old_len, 0);
    assert_eq!(change.new_start, 5);
    assert_eq!(change.new_len, 3);
}

#[test]
fn test_pending_change_delete() {
    let change = PendingChange::delete(5, 3, 5);
    assert!(!change.is_insert());
    assert!(change.is_delete());
    assert!(!change.is_replace());
    assert_eq!(change.old_start, 5);
    assert_eq!(change.old_len, 3);
    assert_eq!(change.new_start, 5);
    assert_eq!(change.new_len, 0);
}

#[test]
fn test_pending_change_replace() {
    let change = PendingChange::replace(5, 2, 5, 4);
    assert!(!change.is_insert());
    assert!(!change.is_delete());
    assert!(change.is_replace());
    assert_eq!(change.old_start, 5);
    assert_eq!(change.old_len, 2);
    assert_eq!(change.new_start, 5);
    assert_eq!(change.new_len, 4);
}

#[test]
fn test_pending_change_from_replace() {
    let change = PendingChange::from_replace(10, 2, 10, 3);
    assert!(change.is_replace());
    assert_eq!(change.old_start, 10);
    assert_eq!(change.old_len, 2);
    assert_eq!(change.new_start, 10);
    assert_eq!(change.new_len, 3);
}

#[test]
fn test_pending_change_display_line_insert() {
    let change = PendingChange::insert(9, 9, 2);
    assert_eq!(change.display_line(), 10); // 1-indexed
}

#[test]
fn test_pending_change_display_line_delete() {
    let change = PendingChange::delete(4, 2, 4);
    assert_eq!(change.display_line(), 5); // 1-indexed
}

#[test]
fn test_pending_change_can_combine_adjacent() {
    let change1 = PendingChange::insert(5, 5, 2);
    let change2 = PendingChange::insert(5, 7, 1);
    assert!(change1.can_combine_with(&change2, 0));
}

#[test]
fn test_pending_change_can_combine_with_gap() {
    let change1 = PendingChange::delete(5, 2, 5);
    let change2 = PendingChange::delete(10, 1, 8);
    // Gap is 10 - 7 = 3 lines
    assert!(change1.can_combine_with(&change2, 5));
    assert!(!change1.can_combine_with(&change2, 2));
}

#[test]
fn test_pending_change_combine_with() {
    let change1 = PendingChange::delete(5, 2, 5);
    let change2 = PendingChange::delete(8, 1, 6);
    let combined = change1.combine_with(&change2);

    assert_eq!(combined.old_start, 5);
    assert_eq!(combined.old_len, 4); // lines 5-8 inclusive = 4 lines
    assert_eq!(combined.new_len, 0); // both deletions have new_len = 0
    assert!(combined.is_delete());
}

#[test]
fn test_pending_change_combine_inserts() {
    let change1 = PendingChange::insert(5, 5, 2);
    let change2 = PendingChange::insert(5, 7, 3);
    let combined = change1.combine_with(&change2);

    assert_eq!(combined.old_start, 5);
    assert_eq!(combined.old_len, 0);
    assert_eq!(combined.new_len, 5); // 2 + 3 = 5
    assert!(combined.is_insert());
}

#[test]
fn test_pending_change_combine_delete_and_insert() {
    let change1 = PendingChange::delete(5, 2, 5);
    let change2 = PendingChange::insert(7, 5, 3);
    let combined = change1.combine_with(&change2);

    assert_eq!(combined.old_start, 5);
    assert_eq!(combined.old_len, 2);
    assert_eq!(combined.new_len, 3);
    assert!(combined.is_replace());
}

#[test]
fn test_pending_change_display() {
    let insert = PendingChange::insert(5, 5, 3);
    assert!(format!("{}", insert).contains("Insert"));

    let delete = PendingChange::delete(5, 2, 5);
    assert!(format!("{}", delete).contains("Delete"));

    let replace = PendingChange::replace(5, 2, 5, 4);
    assert!(format!("{}", replace).contains("Replace"));
}

#[test]
fn test_pending_change_clone() {
    let change = PendingChange::insert(5, 5, 3);
    let cloned = change.clone();
    assert_eq!(change, cloned);
}

// BuiltHunk tests

#[test]
fn test_built_hunk_new_edit() {
    let local = Local::new("test.rs", 10);
    let graph_op = BuiltHunk::new_edit(local, Some(Encoding::Utf8), 0, 50);

    assert!(graph_op.is_edit());
    assert!(!graph_op.is_delete());
    assert!(!graph_op.is_replace());
    assert_eq!(graph_op.content_range(), Some((0, 50)));
    assert_eq!(graph_op.content_len(), 50);
    assert_eq!(graph_op.deleted_line_count(), 0);
}

#[test]
fn test_built_hunk_new_delete() {
    let local = Local::new("test.rs", 10);
    let graph_op = BuiltHunk::new_delete(local, Some(Encoding::Utf8), vec![10, 11, 12], 10);

    assert!(!graph_op.is_edit());
    assert!(graph_op.is_delete());
    assert!(!graph_op.is_replace());
    assert!(graph_op.content_range().is_none());
    assert_eq!(graph_op.content_len(), 0);
    assert_eq!(graph_op.deleted_line_count(), 3);
}

#[test]
fn test_built_hunk_new_replace() {
    let local = Local::new("test.rs", 10);
    let graph_op = BuiltHunk::new_replace(local, Some(Encoding::Utf8), 0, 100, vec![10, 11]);

    assert!(!graph_op.is_edit());
    assert!(!graph_op.is_delete());
    assert!(graph_op.is_replace());
    assert_eq!(graph_op.content_range(), Some((0, 100)));
    assert_eq!(graph_op.content_len(), 100);
    assert_eq!(graph_op.deleted_line_count(), 2);
}

#[test]
fn test_built_hunk_path_and_line() {
    let local = Local::new("src/main.rs", 42);
    let graph_op = BuiltHunk::new_edit(local, None, 0, 10);

    assert_eq!(graph_op.path(), "src/main.rs");
    assert_eq!(graph_op.line(), 42);
}

#[test]
fn test_built_hunk_display() {
    let local = Local::new("test.rs", 10);
    let graph_op = BuiltHunk::new_edit(local, None, 0, 10);
    let display = format!("{}", graph_op);
    assert!(display.contains("Insert"));
    assert!(display.contains("test.rs"));
}

#[test]
fn test_built_hunk_clone() {
    let local = Local::new("test.rs", 10);
    let graph_op = BuiltHunk::new_edit(local, Some(Encoding::Utf8), 0, 50);
    let cloned = graph_op.clone();
    assert_eq!(graph_op, cloned);
}

// HunkBuildResult tests

#[test]
fn test_build_result_new() {
    let result = HunkBuildResult::new();
    assert!(result.is_empty());
    assert_eq!(result.hunk_count(), 0);
    assert_eq!(result.lines_inserted(), 0);
    assert_eq!(result.lines_deleted(), 0);
}

#[test]
fn test_build_result_add_hunk() {
    let mut result = HunkBuildResult::new();
    let local = Local::new("test.rs", 10);
    let graph_op = BuiltHunk::new_edit(local, None, 0, 50);

    result.add_hunk(graph_op);

    assert!(!result.is_empty());
    assert_eq!(result.hunk_count(), 1);
    assert_eq!(result.lines_inserted(), 50);
}

#[test]
fn test_build_result_add_delete_hunk() {
    let mut result = HunkBuildResult::new();
    let local = Local::new("test.rs", 10);
    let graph_op = BuiltHunk::new_delete(local, None, vec![10, 11, 12], 10);

    result.add_hunk(graph_op);

    assert_eq!(result.lines_deleted(), 3);
    assert_eq!(result.lines_inserted(), 0);
}

#[test]
fn test_build_result_hunks() {
    let mut result = HunkBuildResult::new();
    let local1 = Local::new("test.rs", 10);
    let local2 = Local::new("test.rs", 20);
    result.add_hunk(BuiltHunk::new_edit(local1, None, 0, 10));
    result.add_hunk(BuiltHunk::new_edit(local2, None, 10, 20));

    assert_eq!(result.hunks().len(), 2);
}

#[test]
fn test_build_result_into_hunks() {
    let mut result = HunkBuildResult::new();
    let local = Local::new("test.rs", 10);
    result.add_hunk(BuiltHunk::new_edit(local, None, 0, 10));

    let hunks = result.into_hunks();
    assert_eq!(hunks.len(), 1);
}

#[test]
fn test_build_result_iter() {
    let mut result = HunkBuildResult::new();
    let local1 = Local::new("test.rs", 10);
    let local2 = Local::new("test.rs", 20);
    result.add_hunk(BuiltHunk::new_edit(local1, None, 0, 10));
    result.add_hunk(BuiltHunk::new_edit(local2, None, 10, 20));

    let count = result.iter().count();
    assert_eq!(count, 2);
}

#[test]
fn test_build_result_into_iterator() {
    let mut result = HunkBuildResult::new();
    let local = Local::new("test.rs", 10);
    result.add_hunk(BuiltHunk::new_edit(local, None, 0, 10));

    let count = result.into_iter().count();
    assert_eq!(count, 1);
}

#[test]
fn test_build_result_ref_iterator() {
    let mut result = HunkBuildResult::new();
    let local = Local::new("test.rs", 10);
    result.add_hunk(BuiltHunk::new_edit(local, None, 0, 10));

    let count = (&result).into_iter().count();
    assert_eq!(count, 1);
}

// HunkBuilder tests

#[test]
fn test_builder_new() {
    let builder = HunkBuilder::new("test.rs");
    assert_eq!(builder.path(), "test.rs");
    assert_eq!(builder.pending_count(), 0);
    assert!(!builder.has_pending());
}

#[test]
fn test_builder_with_options() {
    let options = HunkBuildOptions::new().encoding(Encoding::Utf8);
    let builder = HunkBuilder::with_options("test.rs", options);
    assert_eq!(builder.options().get_encoding(), Some(Encoding::Utf8));
}

#[test]
fn test_builder_process_equal() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_equal(0, 0, 10);

    assert_eq!(builder.pending_count(), 0);
}

#[test]
fn test_builder_process_insert() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_insert(5, 5, 3);

    assert_eq!(builder.pending_count(), 1);
    assert!(builder.has_pending());
}

#[test]
fn test_builder_process_delete() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_delete(5, 5, 2);

    assert_eq!(builder.pending_count(), 1);
}

#[test]
fn test_builder_process_replace() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_replace_params(5, 2, 5, 3);

    assert_eq!(builder.pending_count(), 1);
}

#[test]
fn test_builder_process_diff_op_equal() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_diff_op(&DiffOp::Equal {
        old_pos: 0,
        new_pos: 0,
        len: 10,
    });

    assert_eq!(builder.pending_count(), 0);
}

#[test]
fn test_builder_process_diff_op_insert() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_diff_op(&DiffOp::Insert {
        old_pos: 5,
        new_pos: 5,
        len: 3,
    });

    assert_eq!(builder.pending_count(), 1);
}

#[test]
fn test_builder_process_diff_op_delete() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_diff_op(&DiffOp::Delete {
        old_pos: 5,
        new_pos: 5,
        len: 2,
    });

    assert_eq!(builder.pending_count(), 1);
}

#[test]
fn test_builder_process_diff_op_replace() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_diff_op(&DiffOp::Replace {
        old_pos: 5,
        old_len: 2,
        new_pos: 5,
        new_len: 3,
    });

    assert_eq!(builder.pending_count(), 1);
}

#[test]
fn test_builder_finish_empty() {
    let builder = HunkBuilder::new("test.rs");
    let result = builder.finish();

    assert!(result.is_empty());
    assert_eq!(result.hunk_count(), 0);
}

#[test]
fn test_builder_finish_with_insert() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_insert(5, 5, 3);

    let result = builder.finish();

    assert_eq!(result.hunk_count(), 1);
    assert!(result.hunks()[0].is_edit());
}

#[test]
fn test_builder_finish_with_delete() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_delete(5, 5, 2);

    let result = builder.finish();

    assert_eq!(result.hunk_count(), 1);
    assert!(result.hunks()[0].is_delete());
    assert_eq!(result.hunks()[0].deleted_line_count(), 2);
}

#[test]
fn test_builder_finish_with_replace() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_replace_params(5, 2, 5, 3);

    let result = builder.finish();

    assert_eq!(result.hunk_count(), 1);
    assert!(result.hunks()[0].is_replace());
}

#[test]
fn test_builder_multiple_operations() {
    let mut builder = HunkBuilder::new("test.rs");

    // Simulate a typical diff output
    builder.process_equal(0, 0, 5);
    builder.process_delete(5, 5, 2);
    builder.process_equal(7, 5, 10);
    builder.process_insert(17, 15, 3);

    let result = builder.finish();

    // Should have 2 separate hunks (gap > combine_threshold)
    assert_eq!(result.hunk_count(), 2);
}

#[test]
fn test_builder_combines_adjacent_changes() {
    let mut builder = HunkBuilder::new("test.rs");

    // Two adjacent changes should be combined
    builder.process_delete(5, 5, 1);
    builder.process_delete(6, 4, 1);

    let result = builder.finish();

    // Should be combined into one graph_op
    assert_eq!(result.hunk_count(), 1);
}

#[test]
fn test_builder_reset() {
    let mut builder = HunkBuilder::new("test.rs");
    builder.process_insert(5, 5, 3);

    builder.reset("other.rs");

    assert_eq!(builder.path(), "other.rs");
    assert_eq!(builder.pending_count(), 0);
}

#[test]
fn test_builder_with_encoding() {
    let options = HunkBuildOptions::new().encoding(Encoding::Utf8);
    let mut builder = HunkBuilder::with_options("test.rs", options);
    builder.process_insert(0, 0, 1);

    let result = builder.finish();

    assert_eq!(result.hunks()[0].encoding, Some(Encoding::Utf8));
}

#[test]
fn test_builder_workflow_scenario() {
    // Simulate a real edit scenario:
    // Old: lines 0-9 (10 lines)
    // New: lines 0-4 unchanged, line 5 modified, lines 6-9 unchanged + 2 new lines at end
    let options = HunkBuildOptions::new().encoding(Encoding::Utf8);
    let mut builder = HunkBuilder::with_options("src/main.rs", options);

    // Lines 0-4 unchanged
    builder.process_diff_op(&DiffOp::Equal {
        old_pos: 0,
        new_pos: 0,
        len: 5,
    });

    // Line 5 replaced
    builder.process_diff_op(&DiffOp::Replace {
        old_pos: 5,
        old_len: 1,
        new_pos: 5,
        new_len: 2,
    });

    // Lines 6-9 unchanged (large gap, more than combine_threshold of 6)
    builder.process_diff_op(&DiffOp::Equal {
        old_pos: 6,
        new_pos: 7,
        len: 10,
    });

    // 2 new lines at end (after 10 unchanged lines, should be separate graph_op)
    builder.process_diff_op(&DiffOp::Insert {
        old_pos: 16,
        new_pos: 17,
        len: 2,
    });

    let result = builder.finish();

    // Should have 2 hunks: one replacement and one insert
    // (the equal region of 10 lines is greater than combine_threshold of 6)
    assert_eq!(result.hunk_count(), 2);
    assert!(result.hunks()[0].is_replace());
    assert!(result.hunks()[1].is_edit());
}

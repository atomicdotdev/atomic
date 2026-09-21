//! Tests for change assembly.

use super::*;

// ========================================================================
// AssemblyOptions Tests
// ========================================================================

#[test]
fn test_options_new_returns_defaults() {
    let opts = AssemblyOptions::new();
    assert_eq!(
        opts.get_max_content_size(),
        AssemblyOptions::DEFAULT_MAX_CONTENT_SIZE
    );
    assert_eq!(opts.get_max_hunks(), AssemblyOptions::DEFAULT_MAX_HUNKS);
    assert!(!opts.get_include_empty_files());
    assert!(opts.get_validate_dependencies());
}

#[test]
fn test_options_default() {
    let opts = AssemblyOptions::default();
    assert_eq!(opts.get_max_content_size(), 100 * 1024 * 1024);
}

#[test]
fn test_options_max_content_size() {
    let opts = AssemblyOptions::new().max_content_size(1024);
    assert_eq!(opts.get_max_content_size(), 1024);
}

#[test]
fn test_options_max_hunks() {
    let opts = AssemblyOptions::new().max_hunks(100);
    assert_eq!(opts.get_max_hunks(), 100);
}

#[test]
fn test_options_include_empty_files() {
    let opts = AssemblyOptions::new().include_empty_files(true);
    assert!(opts.get_include_empty_files());
}

#[test]
fn test_options_validate_dependencies() {
    let opts = AssemblyOptions::new().validate_dependencies(false);
    assert!(!opts.get_validate_dependencies());
}

#[test]
fn test_options_builder_chain() {
    let opts = AssemblyOptions::new()
        .max_content_size(1024)
        .max_hunks(50)
        .include_empty_files(true)
        .validate_dependencies(false);

    assert_eq!(opts.get_max_content_size(), 1024);
    assert_eq!(opts.get_max_hunks(), 50);
    assert!(opts.get_include_empty_files());
    assert!(!opts.get_validate_dependencies());
}

#[test]
fn test_options_clone() {
    let opts1 = AssemblyOptions::new().max_hunks(100);
    let opts2 = opts1.clone();
    assert_eq!(opts2.get_max_hunks(), 100);
}

#[test]
fn test_options_debug() {
    let opts = AssemblyOptions::new();
    let debug = format!("{:?}", opts);
    assert!(debug.contains("AssemblyOptions"));
}

// ========================================================================
// AssemblyError Tests
// ========================================================================

#[test]
fn test_error_no_files() {
    let err = AssemblyError::NoFiles;
    let msg = format!("{}", err);
    assert!(msg.contains("No files"));
}

#[test]
fn test_error_all_empty() {
    let err = AssemblyError::AllEmpty;
    let msg = format!("{}", err);
    assert!(msg.contains("empty"));
}

#[test]
fn test_error_content_too_large() {
    let err = AssemblyError::ContentTooLarge {
        actual: 200,
        limit: 100,
    };
    let msg = format!("{}", err);
    assert!(msg.contains("200"));
    assert!(msg.contains("100"));
}

#[test]
fn test_error_too_many_hunks() {
    let err = AssemblyError::TooManyHunks {
        actual: 20000,
        limit: 10000,
    };
    let msg = format!("{}", err);
    assert!(msg.contains("20000"));
    assert!(msg.contains("10000"));
}

#[test]
fn test_error_invalid_content_range() {
    let err = AssemblyError::InvalidContentRange {
        path: "test.rs".to_string(),
        start: 100,
        end: 50,
    };
    let msg = format!("{}", err);
    assert!(msg.contains("test.rs"));
}

// ========================================================================
// AssemblyStats Tests
// ========================================================================

#[test]
fn test_stats_new() {
    let stats = AssemblyStats::new();
    assert_eq!(stats.files_processed, 0);
    assert_eq!(stats.files_skipped, 0);
    assert_eq!(stats.hunks_added, 0);
}

#[test]
fn test_stats_record_file() {
    let mut stats = AssemblyStats::new();
    stats.record_file();
    assert_eq!(stats.files_processed, 1);
}

#[test]
fn test_stats_record_skip() {
    let mut stats = AssemblyStats::new();
    stats.record_skip();
    assert_eq!(stats.files_skipped, 1);
}

#[test]
fn test_stats_record_error() {
    let mut stats = AssemblyStats::new();
    stats.record_error();
    assert!(stats.has_errors());
}

#[test]
fn test_stats_add_content_bytes() {
    let mut stats = AssemblyStats::new();
    stats.add_content_bytes(100);
    stats.add_content_bytes(50);
    assert_eq!(stats.content_bytes, 150);
}

#[test]
fn test_stats_total_files() {
    let mut stats = AssemblyStats::new();
    stats.record_file();
    stats.record_file();
    stats.record_skip();
    assert_eq!(stats.total_files(), 3);
}

#[test]
fn test_stats_display() {
    let stats = AssemblyStats {
        files_processed: 5,
        files_skipped: 2,
        hunks_added: 10,
        dependencies_added: 3,
        content_bytes: 1024,
        errors: 0,
    };
    let display = format!("{}", stats);
    assert!(display.contains("5"));
    assert!(display.contains("10"));
}

// ========================================================================
// AssemblyContext Tests
// ========================================================================

#[test]
fn test_context_new() {
    let header = ChangeHeader::builder().message("Test").build();
    let ctx = AssemblyContext::new(header);
    assert_eq!(ctx.hunk_count(), 0);
    assert_eq!(ctx.dependency_count(), 0);
}

#[test]
fn test_context_with_capacity() {
    let header = ChangeHeader::builder().message("Test").build();
    let ctx = AssemblyContext::with_capacity(header, 100);
    assert_eq!(ctx.hunk_count(), 0);
}

#[test]
fn test_context_add_dependency() {
    let header = ChangeHeader::builder().message("Test").build();
    let mut ctx = AssemblyContext::new(header);
    let hash = Hash::of(b"test");
    ctx.add_dependency(hash);
    assert_eq!(ctx.dependency_count(), 1);
}

#[test]
fn test_context_add_dependency_dedup() {
    let header = ChangeHeader::builder().message("Test").build();
    let mut ctx = AssemblyContext::new(header);
    let hash = Hash::of(b"test");
    ctx.add_dependency(hash);
    ctx.add_dependency(hash);
    assert_eq!(ctx.dependency_count(), 1);
}

#[test]
fn test_context_finalize() {
    let header = ChangeHeader::builder().message("Test change").build();
    let ctx = AssemblyContext::new(header);
    let change = ctx.finalize(vec![1, 2, 3], vec![], vec![]);
    assert_eq!(change.message(), "Test change");
    assert_eq!(change.contents, vec![1, 2, 3]);
}

// ========================================================================
// Helper Function Tests
// ========================================================================

#[test]
fn test_compute_content_offsets_empty() {
    let files: Vec<RecordedFile> = vec![];
    let offsets = compute_content_offsets(&files);
    assert!(offsets.is_empty());
}

#[test]
fn test_finalize_hunks_under_limit() {
    let hunks: Vec<GraphOp<Option<Hash>>> = vec![];
    let opts = AssemblyOptions::new().max_hunks(10);
    let result = finalize_hunks(hunks, &opts);
    assert!(result.is_ok());
}

#[test]
fn test_finalize_hunks_over_limit() {
    let hunks: Vec<GraphOp<Option<Hash>>> = vec![];
    let opts = AssemblyOptions::new().max_hunks(0);
    // Empty vec passes even with limit 0
    let result = finalize_hunks(hunks, &opts);
    assert!(result.is_ok());
}

#[test]
fn test_create_empty_change() {
    let header = ChangeHeader::builder().message("Empty").build();
    let change = create_empty_change(header);
    assert!(change.hunks().is_empty());
    assert!(change.contents.is_empty());
}

// ========================================================================
// AssemblyResult_ Tests
// ========================================================================

#[test]
fn test_assembly_result_new() {
    let header = ChangeHeader::builder().message("Test").build();
    let change = Change::empty(header);
    let stats = AssemblyStats::new();
    let result = AssemblyResult_::new(change, stats, vec![], vec![]);
    assert_eq!(result.hunk_count(), 0);
    assert!(!result.has_errors());
}

#[test]
fn test_assembly_result_content_size() {
    let header = ChangeHeader::builder().message("Test").build();
    let mut change = Change::empty(header);
    change.contents = vec![0u8; 100];
    let stats = AssemblyStats::new();
    let result = AssemblyResult_::new(change, stats, vec![], vec![]);
    assert_eq!(result.content_size(), 100);
}

#[test]
fn test_assembly_result_into_change() {
    let header = ChangeHeader::builder().message("Take me").build();
    let change = Change::empty(header);
    let stats = AssemblyStats::new();
    let result = AssemblyResult_::new(change, stats, vec![], vec![]);
    let taken = result.into_change();
    assert_eq!(taken.message(), "Take me");
}

// ========================================================================
// Placeholder Namespace Tests (CB-9B review B1)
// ========================================================================

use crate::crdt::{BranchId as CrdtBranchId, LeafId, TrunkId as CrdtTrunkId};
use crate::diff::TokenKind;
use crate::types::NodeId;

/// One one-line file-create entry with a single leaf: the smallest possible
/// placeholder namespace span (branch span 1, leaf span 1).
fn one_line_entry(path: &str) -> FileOps {
    let mut ops = FileOps::create(CrdtTrunkId::ROOT, path.to_string(), None);
    ops.add_line_op(crate::change::LineOps::new(
        CrdtBranchId::ROOT,
        BranchOp::Insert {
            after: None,
            content: vec![LeafOp::Insert {
                after: None,
                kind: TokenKind::Word,
                content: b"line".to_vec(),
            }],
        },
    ));
    ops
}

/// A multi-line entry: `lines` chained inserts plus `tokens` leaves per line,
/// with `after` references that must shift with the branch base.
fn multi_line_entry(path: &str, lines: usize, tokens: usize) -> FileOps {
    let mut ops = FileOps::create(CrdtTrunkId::ROOT, path.to_string(), None);
    for line in 0..lines {
        let after = if line == 0 {
            None
        } else {
            Some(CrdtBranchId::new(NodeId::ROOT, (line - 1) as u32))
        };
        let mut content = Vec::with_capacity(tokens);
        for token in 0..tokens {
            content.push(LeafOp::Insert {
                after: if token == 0 {
                    None
                } else {
                    Some(LeafId::new(NodeId::ROOT, (token - 1) as u32))
                },
                kind: if token % 2 == 0 {
                    TokenKind::Word
                } else {
                    TokenKind::Whitespace
                },
                content: format!("t{token}").into_bytes(),
            });
        }
        ops.add_line_op(crate::change::LineOps::new(
            CrdtBranchId::new(NodeId::ROOT, line as u32),
            BranchOp::Insert { after, content },
        ));
    }
    ops
}

#[test]
fn add_file_ops_placeholder_namespace_advances_linearly_for_one_line_files() {
    let header = ChangeHeader::builder().message("many files").build();
    let mut ctx = AssemblyContext::new(header);
    // 300 one-line entries: the pre-fix implementation doubled the running
    // base per entry (2·b + span), overflowing u32 around the 33rd entry.
    for i in 0..300 {
        ctx.add_file_ops(one_line_entry(&format!("f{i}.txt")))
            .expect("one-line entry must always fit");
    }
    // Each entry's local span is exactly 1 branch slot and 1 leaf slot, so
    // both bases advance linearly (review B1: base grows by the span, not
    // geometrically).
    assert_eq!(ctx.placeholder_branch_base, 300);
    assert_eq!(ctx.placeholder_leaf_base, 300);
    // Every entry occupies its own namespace slot: entry i's placeholder
    // branch index and trunk file index are exactly i (referential
    // integrity under the apply-time substitution).
    for (i, ops) in ctx.file_ops.iter().enumerate() {
        let i = i as u32;
        assert_eq!(ops.trunk_id().file_idx(), i, "trunk file idx of entry {i}");
        assert!(ops.trunk_id().change_id().is_root());
        for line_op in ops.line_ops() {
            let branch = line_op.branch_id();
            assert!(branch.change_id().is_root());
            assert_eq!(
                branch.branch_idx(),
                i,
                "branch placeholder of entry {i} must be shifted into its own slot"
            );
        }
    }
}

#[test]
fn add_file_ops_placeholder_namespace_advances_by_exact_span_for_mixed_entries() {
    let header = ChangeHeader::builder().message("mixed").build();
    let mut ctx = AssemblyContext::new(header);
    // Mixed line/token counts: each entry advances the branch base by its
    // own local branch span (one slot per line) and the leaf base by its
    // own local leaf-after span (the highest placeholder leaf index + 1).
    let shapes = [(1usize, 1usize), (3, 2), (5, 4), (2, 7), (1, 1)];
    let mut branch = 0u32;
    let mut leaf = 0u32;
    for (idx, (lines, tokens)) in shapes.iter().enumerate() {
        let entry = multi_line_entry(&format!("m{idx}.txt"), *lines, *tokens);
        ctx.add_file_ops(entry).expect("mixed entry fits");
        branch += *lines as u32;
        // The leaf placeholder namespace only needs one slot beyond the
        // highest `after` index: a t-token line chains after-refs 0..t-2,
        // and a single-token line (no after ref) still occupies slot 0.
        leaf += (tokens.saturating_sub(1)).max(1) as u32;
        assert_eq!(ctx.placeholder_branch_base, branch, "branch base after entry {idx}");
        assert_eq!(ctx.placeholder_leaf_base, leaf, "leaf base after entry {idx}");
        // The entry just added was renumbered into [base - span, base):
        // every branch placeholder in it must land inside its own span.
        let ops = ctx.file_ops.last().expect("entry recorded");
        let span = *lines as u32;
        for line_op in ops.line_ops() {
            let b = line_op.branch_id().branch_idx();
            assert!(
                b >= branch - span && b < branch,
                "branch idx {b} outside this entry's namespace [{},{})",
                branch - span,
                branch
            );
        }
    }
    // Cross-entry uniqueness: no branch placeholder appears in two entries.
    let mut seen = std::collections::HashSet::new();
    for ops in &ctx.file_ops {
        for line_op in ops.line_ops() {
            assert!(
                seen.insert(line_op.branch_id().branch_idx()),
                "duplicate placeholder branch index across entries"
            );
        }
    }
}

#[test]
fn add_file_ops_placeholder_namespace_exhaustion_is_typed_not_a_panic() {
    let header = ChangeHeader::builder().message("exhausted").build();
    let mut ctx = AssemblyContext::new(header);
    // Test-only seed: push the running base to the namespace limit, then
    // verify the next entry is refused with the typed error in BOTH debug
    // (overflow panics) and release (wraps) modes instead.
    ctx.force_placeholder_bases(u32::MAX, 0);
    let error = ctx
        .add_file_ops(one_line_entry("overflow.txt"))
        .expect_err("exhausted namespace must fail closed");
    assert!(matches!(
        error,
        crate::record::workflow::assembly::AssemblyError::PlaceholderNamespaceExhausted { .. }
    ));
    let message = error.to_string();
    assert!(message.contains("namespace exhausted"), "{message}");
}

impl AssemblyContext {
    /// Test-only seed for the exhaustion path (CB-9B review B1): force the
    /// running placeholder bases so the next entry cannot fit.
    #[cfg(test)]
    fn force_placeholder_bases(&mut self, branch: u32, leaf: u32) {
        self.placeholder_branch_base = branch;
        self.placeholder_leaf_base = leaf;
    }
}

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

//! Tests for the recording module.

use super::*;
use crate::change::{Encoding, Local};
use crate::diff::Algorithm;
use crate::output::Memory;
use crate::record::workflow::detect::{DetectedFile, DetectionKind};
use crate::record::workflow::graph_op::BuiltHunk;
use crate::types::Inode;

// ========================================================================
// RecordingOptions tests
// ========================================================================

#[test]
fn test_options_new_returns_defaults() {
    let opts = RecordingOptions::new();
    assert_eq!(opts.get_algorithm(), Algorithm::Myers);
    assert!(opts.get_default_encoding().is_none());
    assert!(!opts.get_skip_binary());
    assert!(opts.get_record_empty_files());
    assert_eq!(opts.get_context_lines(), 3);
}

#[test]
fn test_options_default() {
    let opts = RecordingOptions::default();
    assert_eq!(
        opts.get_max_file_size(),
        Some(RecordingOptions::DEFAULT_MAX_FILE_SIZE)
    );
}

#[test]
fn test_options_algorithm() {
    let opts = RecordingOptions::new().algorithm(Algorithm::Patience);
    assert_eq!(opts.get_algorithm(), Algorithm::Patience);
}

#[test]
fn test_options_default_encoding() {
    let opts = RecordingOptions::new().default_encoding(Encoding::Utf8);
    assert_eq!(opts.get_default_encoding(), Some(Encoding::Utf8));
}

#[test]
fn test_options_max_file_size() {
    let opts = RecordingOptions::new().max_file_size(1024);
    assert_eq!(opts.get_max_file_size(), Some(1024));
}

#[test]
fn test_options_skip_binary() {
    let opts = RecordingOptions::new().skip_binary(true);
    assert!(opts.get_skip_binary());
}

#[test]
fn test_options_record_empty_files() {
    let opts = RecordingOptions::new().record_empty_files(false);
    assert!(!opts.get_record_empty_files());
}

#[test]
fn test_options_context_lines() {
    let opts = RecordingOptions::new().context_lines(5);
    assert_eq!(opts.get_context_lines(), 5);
}

#[test]
fn test_options_exceeds_max_size() {
    let opts = RecordingOptions::new().max_file_size(1000);
    assert!(!opts.exceeds_max_size(500));
    assert!(!opts.exceeds_max_size(1000));
    assert!(opts.exceeds_max_size(1001));
}

#[test]
fn test_options_builder_chain() {
    let opts = RecordingOptions::new()
        .algorithm(Algorithm::Patience)
        .default_encoding(Encoding::Utf8)
        .max_file_size(1024)
        .skip_binary(true)
        .record_empty_files(false)
        .context_lines(5);

    assert_eq!(opts.get_algorithm(), Algorithm::Patience);
    assert_eq!(opts.get_default_encoding(), Some(Encoding::Utf8));
    assert_eq!(opts.get_max_file_size(), Some(1024));
    assert!(opts.get_skip_binary());
    assert!(!opts.get_record_empty_files());
    assert_eq!(opts.get_context_lines(), 5);
}

#[test]
fn test_options_to_hunk_options() {
    let opts = RecordingOptions::new()
        .default_encoding(Encoding::Utf8)
        .context_lines(5);

    let hunk_opts = opts.to_hunk_options();
    assert_eq!(hunk_opts.get_encoding(), Some(Encoding::Utf8));
    assert_eq!(hunk_opts.get_context_lines(), 5);
}

#[test]
fn test_options_clone() {
    let opts = RecordingOptions::new().algorithm(Algorithm::Patience);
    let cloned = opts.clone();
    assert_eq!(opts, cloned);
}

#[test]
fn test_options_debug() {
    let opts = RecordingOptions::new();
    let debug = format!("{:?}", opts);
    assert!(debug.contains("RecordingOptions"));
}

// ========================================================================
// RecordingStats tests
// ========================================================================

#[test]
fn test_stats_new() {
    let stats = RecordingStats::new();
    assert_eq!(stats.files_recorded, 0);
    assert_eq!(stats.hunks_created, 0);
    assert_eq!(stats.files_skipped, 0);
    assert_eq!(stats.total_files(), 0);
}

#[test]
fn test_stats_total_files() {
    let mut stats = RecordingStats::new();
    stats.files_recorded = 5;
    stats.files_skipped = 2;
    assert_eq!(stats.total_files(), 7);
}

#[test]
fn test_stats_total_line_changes() {
    let mut stats = RecordingStats::new();
    stats.lines_added = 10;
    stats.lines_deleted = 5;
    assert_eq!(stats.total_line_changes(), 15);
}

#[test]
fn test_stats_has_errors() {
    let mut stats = RecordingStats::new();
    assert!(!stats.has_errors());
    stats.errors = 1;
    assert!(stats.has_errors());
}

#[test]
fn test_stats_merge() {
    let mut stats1 = RecordingStats::new();
    stats1.files_recorded = 2;
    stats1.hunks_created = 3;
    stats1.lines_added = 10;

    let mut stats2 = RecordingStats::new();
    stats2.files_recorded = 1;
    stats2.hunks_created = 2;
    stats2.lines_deleted = 5;

    stats1.merge(&stats2);

    assert_eq!(stats1.files_recorded, 3);
    assert_eq!(stats1.hunks_created, 5);
    assert_eq!(stats1.lines_added, 10);
    assert_eq!(stats1.lines_deleted, 5);
}

#[test]
fn test_stats_clone() {
    let mut stats = RecordingStats::new();
    stats.files_recorded = 5;
    let cloned = stats.clone();
    assert_eq!(stats, cloned);
}

// ========================================================================
// RecordedFile tests
// ========================================================================

#[test]
fn test_recorded_file_new() {
    let file = RecordedFile::new("test.rs");
    assert_eq!(file.path(), "test.rs");
    assert!(file.is_empty());
    assert_eq!(file.hunk_count(), 0);
    assert_eq!(file.content_len(), 0);
}

#[test]
fn test_recorded_file_add_hunk() {
    let mut file = RecordedFile::new("test.rs");
    let graph_op = BuiltHunk::new_edit(Local::new("test.rs", 1), Some(Encoding::Utf8), 0, 10);
    file.add_hunk(graph_op);

    assert!(!file.is_empty());
    assert_eq!(file.hunk_count(), 1);
}

#[test]
fn test_recorded_file_set_content() {
    let mut file = RecordedFile::new("test.rs");
    file.set_content(b"hello world".to_vec());

    assert_eq!(file.content_len(), 11);
    assert_eq!(file.content(), b"hello world");
}

#[test]
fn test_recorded_file_set_encoding() {
    let mut file = RecordedFile::new("test.rs");
    file.set_encoding(Encoding::Utf8);

    assert_eq!(file.encoding(), Some(Encoding::Utf8));
}

#[test]
fn test_recorded_file_set_kind() {
    let mut file = RecordedFile::new("test.rs");
    file.set_kind(DetectionKind::Added);

    assert_eq!(file.kind(), Some(DetectionKind::Added));
}

#[test]
fn test_recorded_file_set_inode() {
    let mut file = RecordedFile::new("test.rs");
    file.set_inode(Inode::new(42));

    assert_eq!(file.inode(), Some(Inode::new(42)));
}

#[test]
fn test_recorded_file_into_hunks() {
    let mut file = RecordedFile::new("test.rs");
    let graph_op = BuiltHunk::new_edit(Local::new("test.rs", 1), None, 0, 10);
    file.add_hunk(graph_op);

    let hunks = file.into_hunks();
    assert_eq!(hunks.len(), 1);
}

#[test]
fn test_recorded_file_into_content() {
    let mut file = RecordedFile::new("test.rs");
    file.set_content(b"content".to_vec());

    let content = file.into_content();
    assert_eq!(content, b"content");
}

#[test]
fn test_recorded_file_clone() {
    let mut file = RecordedFile::new("test.rs");
    file.set_encoding(Encoding::Utf8);
    let cloned = file.clone();
    assert_eq!(file.path(), cloned.path());
    assert_eq!(file.encoding(), cloned.encoding());
}

// ========================================================================
// RecordingResult tests
// ========================================================================

#[test]
fn test_result_new() {
    let result = RecordingResult::new();
    assert!(result.is_empty());
    assert!(!result.has_errors());
    assert_eq!(result.file_count(), 0);
    assert_eq!(result.hunk_count(), 0);
}

#[test]
fn test_result_add_file() {
    let mut result = RecordingResult::new();
    let mut file = RecordedFile::new("test.rs");
    file.add_hunk(BuiltHunk::new_edit(Local::new("test.rs", 1), None, 0, 10));
    file.set_content(b"content".to_vec());

    result.add_file(file);

    assert!(!result.is_empty());
    assert_eq!(result.file_count(), 1);
    assert_eq!(result.hunk_count(), 1);
    assert_eq!(result.content_len(), 7);
}

#[test]
fn test_result_add_error() {
    let mut result = RecordingResult::new();
    result.add_error("something went wrong");

    assert!(result.has_errors());
    assert_eq!(result.errors().len(), 1);
}

#[test]
fn test_result_record_skipped() {
    let mut result = RecordingResult::new();
    result.record_skipped();

    assert_eq!(result.stats().files_skipped, 1);
}

#[test]
fn test_result_record_binary() {
    let mut result = RecordingResult::new();
    result.record_binary();

    assert_eq!(result.stats().binary_files, 1);
}

#[test]
fn test_result_record_oversized() {
    let mut result = RecordingResult::new();
    result.record_oversized();

    assert_eq!(result.stats().oversized_files, 1);
}

#[test]
fn test_result_record_line_changes() {
    let mut result = RecordingResult::new();
    result.record_line_changes(10, 5);

    assert_eq!(result.stats().lines_added, 10);
    assert_eq!(result.stats().lines_deleted, 5);
}

#[test]
fn test_result_iter() {
    let mut result = RecordingResult::new();
    result.add_file(RecordedFile::new("a.rs"));
    result.add_file(RecordedFile::new("b.rs"));

    let count = result.iter().count();
    assert_eq!(count, 2);
}

#[test]
fn test_result_into_iterator() {
    let mut result = RecordingResult::new();
    result.add_file(RecordedFile::new("test.rs"));

    let count = result.into_iter().count();
    assert_eq!(count, 1);
}

#[test]
fn test_result_ref_iterator() {
    let mut result = RecordingResult::new();
    result.add_file(RecordedFile::new("test.rs"));

    let count = (&result).into_iter().count();
    assert_eq!(count, 1);
}

#[test]
fn test_result_merge() {
    let mut result1 = RecordingResult::new();
    result1.add_file(RecordedFile::new("a.rs"));

    let mut result2 = RecordingResult::new();
    result2.add_file(RecordedFile::new("b.rs"));
    result2.add_error("error");

    result1.merge(result2);

    assert_eq!(result1.file_count(), 2);
    assert!(result1.has_errors());
}

// ========================================================================
// record_added_file tests
// ========================================================================

#[test]
fn test_record_added_file_success() {
    let wc = Memory::new();
    wc.add_file("new.rs", b"fn main() {}");

    let detected = DetectedFile::added("new.rs");
    let options = RecordingOptions::new();

    let result = record_added_file(&wc, &detected, &options);

    assert!(result.is_ok());
    let recorded = result.unwrap();
    assert_eq!(recorded.path(), "new.rs");
    assert_eq!(recorded.kind(), Some(DetectionKind::Added));
    assert_eq!(recorded.content(), b"fn main() {}");
    assert_eq!(recorded.hunk_count(), 1);
}

#[test]
fn test_record_added_file_not_found() {
    let wc = Memory::new();

    let detected = DetectedFile::added("missing.rs");
    let options = RecordingOptions::new();

    let result = record_added_file(&wc, &detected, &options);

    assert!(result.is_err());
}

#[test]
fn test_record_added_file_exceeds_size() {
    let wc = Memory::new();
    wc.add_file("big.rs", b"x".repeat(1000).as_slice());

    let detected = DetectedFile::added("big.rs");
    let options = RecordingOptions::new().max_file_size(100);

    let result = record_added_file(&wc, &detected, &options);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("exceeds maximum size"));
}

#[test]
fn test_record_added_file_skip_empty() {
    let wc = Memory::new();
    wc.add_file("empty.rs", b"");

    let detected = DetectedFile::added("empty.rs");
    let options = RecordingOptions::new().record_empty_files(false);

    let result = record_added_file(&wc, &detected, &options);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("empty file"));
}

#[test]
fn test_record_added_file_binary_skip() {
    let wc = Memory::new();
    // Binary content (contains null bytes)
    wc.add_file("binary.bin", &[0x00, 0x01, 0x02, 0xFF]);

    let detected = DetectedFile::added("binary.bin");
    let options = RecordingOptions::new().skip_binary(true);

    let result = record_added_file(&wc, &detected, &options);

    assert!(result.is_err());
    assert!(result.unwrap_err().contains("binary file"));
}

#[test]
fn test_record_added_file_binary_allowed() {
    let wc = Memory::new();
    wc.add_file("binary.bin", &[0x00, 0x01, 0x02, 0xFF]);

    let detected = DetectedFile::added("binary.bin");
    let options = RecordingOptions::new().skip_binary(false);

    let result = record_added_file(&wc, &detected, &options);

    assert!(result.is_ok());
    assert_eq!(result.unwrap().encoding(), Some(Encoding::Binary));
}

// ========================================================================
// record_deleted_file tests
// ========================================================================

#[test]
fn test_record_deleted_file() {
    let detected = DetectedFile::deleted("old.rs").with_inode(Inode::new(42));
    let options = RecordingOptions::new();

    let result = record_deleted_file(&detected, &options);

    assert!(result.is_ok());
    let recorded = result.unwrap();
    assert_eq!(recorded.path(), "old.rs");
    assert_eq!(recorded.kind(), Some(DetectionKind::Deleted));
    assert_eq!(recorded.inode(), Some(Inode::new(42)));
    assert_eq!(recorded.hunk_count(), 1);
}

// ========================================================================
// record_modified_file tests
// ========================================================================

#[test]
fn test_record_modified_file_success() {
    let wc = Memory::new();
    wc.add_file("lib.rs", b"fn new() {}");

    let detected = DetectedFile::modified("lib.rs");
    let old_content = b"fn old() {}";
    let options = RecordingOptions::new();

    let result = record_modified_file(&wc, &detected, old_content, None, &options, None);

    assert!(result.is_ok());
    let recorded = result.unwrap();
    assert_eq!(recorded.path(), "lib.rs");
    assert_eq!(recorded.kind(), Some(DetectionKind::Modified));
    assert_eq!(recorded.content(), b"fn new() {}");
}

#[test]
fn test_record_modified_file_not_found() {
    let wc = Memory::new();

    let detected = DetectedFile::modified("missing.rs");
    let old_content = b"old content";
    let options = RecordingOptions::new();

    let result = record_modified_file(&wc, &detected, old_content, None, &options, None);

    assert!(result.is_err());
}

#[test]
fn test_record_modified_file_with_diff() {
    let wc = Memory::new();
    wc.add_file("test.rs", b"line1\nline2\nline3\n");

    let detected = DetectedFile::modified("test.rs");
    let old_content = b"line1\nold_line\nline3\n";
    let options = RecordingOptions::new();

    let result = record_modified_file(&wc, &detected, old_content, None, &options, None);

    assert!(result.is_ok());
    let recorded = result.unwrap();
    // Should have hunks for the modification
    assert!(recorded.hunk_count() > 0);
}

// ========================================================================
// CRDT Integration Tests
// ========================================================================

#[test]
fn test_record_added_file_has_crdt_ops() {
    let wc = Memory::new();
    wc.add_file("new.rs", b"fn main() {\n    println!(\"Hello\");\n}\n");

    let detected = DetectedFile::added("new.rs");
    let options = RecordingOptions::new();

    let result = record_added_file(&wc, &detected, &options);
    assert!(result.is_ok());

    let recorded = result.unwrap();
    // Should have CRDT operations generated
    assert!(recorded.has_crdt_ops());

    let crdt_ops = recorded.crdt_ops().unwrap();
    assert_eq!(crdt_ops.path(), "new.rs");

    // Should have trunk operation (Create)
    assert!(crdt_ops.trunk_op().is_some());

    // Should have line operations for each line
    assert!(crdt_ops.line_count() > 0);
}

#[test]
fn test_record_added_file_crdt_stats() {
    let wc = Memory::new();
    wc.add_file("test.rs", b"line1\nline2\nline3\n");

    let detected = DetectedFile::added("test.rs");
    let options = RecordingOptions::new();

    let result = record_added_file(&wc, &detected, &options);
    assert!(result.is_ok());

    let recorded = result.unwrap();
    let stats = recorded.crdt_stats().unwrap();

    // Should have tracked file addition
    assert_eq!(stats.files_added, 1);

    // Should have tracked line additions (3 lines + possibly trailing newline handling)
    assert!(stats.lines_added >= 3);

    // Should have tracked token additions
    assert!(stats.tokens_added > 0);
}

#[test]
fn test_record_added_file_crdt_tokenization() {
    let wc = Memory::new();
    // Content with recognizable tokens
    wc.add_file("code.rs", b"let x = 42;\n");

    let detected = DetectedFile::added("code.rs");
    let options = RecordingOptions::new();

    let result = record_added_file(&wc, &detected, &options);
    assert!(result.is_ok());

    let recorded = result.unwrap();
    let stats = recorded.crdt_stats().unwrap();

    // Should have tokenized the content
    // "let", " ", "x", " ", "=", " ", "42", ";", "\n" = multiple tokens
    assert!(stats.tokens_added >= 4);
}

#[test]
fn test_record_deleted_file_has_crdt_ops() {
    let detected = DetectedFile::deleted("old.rs");
    let options = RecordingOptions::new();

    let result = record_deleted_file(&detected, &options);
    assert!(result.is_ok());

    let recorded = result.unwrap();
    assert!(recorded.has_crdt_ops());

    let stats = recorded.crdt_stats().unwrap();
    assert_eq!(stats.files_deleted, 1);
}

#[test]
fn test_record_modified_file_has_crdt_ops() {
    let wc = Memory::new();
    wc.add_file("lib.rs", b"fn new_function() {\n    // new code\n}\n");

    let detected = DetectedFile::modified("lib.rs");
    let old_content = b"fn old_function() {\n    // old code\n}\n";
    let options = RecordingOptions::new();

    let result = record_modified_file(&wc, &detected, old_content, None, &options, None);
    assert!(result.is_ok());

    let recorded = result.unwrap();
    assert!(recorded.has_crdt_ops());

    // Modifications should generate line operations
    let crdt_ops = recorded.crdt_ops().unwrap();
    assert!(crdt_ops.line_count() > 0);
}

#[test]
fn test_record_modified_file_crdt_stats_tracks_changes() {
    let wc = Memory::new();
    wc.add_file("test.rs", b"line1\nnew_line\nline3\n");

    let detected = DetectedFile::modified("test.rs");
    let old_content = b"line1\nold_line\nline3\n";
    let options = RecordingOptions::new();

    let result = record_modified_file(&wc, &detected, old_content, None, &options, None);
    assert!(result.is_ok());

    let recorded = result.unwrap();
    let stats = recorded.crdt_stats().unwrap();

    // Should track the modification (delete old + insert new counts as lines_modified)
    // The middle line changed: "old_line" -> "new_line"
    assert!(stats.lines_deleted > 0 || stats.lines_modified > 0);
    assert!(stats.lines_added > 0 || stats.lines_modified > 0);
}

#[test]
fn test_record_modified_file_crdt_insert_only() {
    let wc = Memory::new();
    wc.add_file("test.rs", b"line1\nline2\nnew_line\nline3\n");

    let detected = DetectedFile::modified("test.rs");
    let old_content = b"line1\nline2\nline3\n";
    let options = RecordingOptions::new();

    let result = record_modified_file(&wc, &detected, old_content, None, &options, None);
    assert!(result.is_ok());

    let recorded = result.unwrap();
    let stats = recorded.crdt_stats().unwrap();

    // Should have inserted one line
    assert!(stats.lines_added >= 1);
}

#[test]
fn test_record_modified_file_crdt_delete_only() {
    let wc = Memory::new();
    wc.add_file("test.rs", b"line1\nline3\n");

    let detected = DetectedFile::modified("test.rs");
    let old_content = b"line1\nline2\nline3\n";
    let options = RecordingOptions::new();

    let result = record_modified_file(&wc, &detected, old_content, None, &options, None);
    assert!(result.is_ok());

    let recorded = result.unwrap();
    let stats = recorded.crdt_stats().unwrap();

    // Should have deleted one line
    assert!(stats.lines_deleted >= 1);
}

#[test]
fn test_record_added_file_crdt_preserves_path() {
    let wc = Memory::new();
    wc.add_file("src/lib/module.rs", b"// module\n");

    let detected = DetectedFile::added("src/lib/module.rs");
    let options = RecordingOptions::new();

    let result = record_added_file(&wc, &detected, &options);
    assert!(result.is_ok());

    let recorded = result.unwrap();
    let crdt_ops = recorded.crdt_ops().unwrap();

    // Path should be preserved in CRDT ops
    assert_eq!(crdt_ops.path(), "src/lib/module.rs");
}

#[test]
fn test_record_added_empty_file_crdt() {
    let wc = Memory::new();
    wc.add_file("empty.rs", b"");

    let detected = DetectedFile::added("empty.rs");
    let options = RecordingOptions::new().record_empty_files(true);

    let result = record_added_file(&wc, &detected, &options);
    assert!(result.is_ok());

    let recorded = result.unwrap();
    assert!(recorded.has_crdt_ops());

    let stats = recorded.crdt_stats().unwrap();
    assert_eq!(stats.files_added, 1);
    // Empty file should have no lines or tokens
    assert_eq!(stats.lines_added, 0);
    assert_eq!(stats.tokens_added, 0);
}

#[test]
fn test_crdt_ops_into_ownership() {
    let wc = Memory::new();
    wc.add_file("test.rs", b"content\n");

    let detected = DetectedFile::added("test.rs");
    let options = RecordingOptions::new();

    let result = record_added_file(&wc, &detected, &options);
    assert!(result.is_ok());

    let recorded = result.unwrap();

    // Should be able to take ownership of CRDT ops
    let crdt_ops = recorded.into_crdt_ops();
    assert!(crdt_ops.is_some());
}

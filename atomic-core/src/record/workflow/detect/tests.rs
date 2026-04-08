//! Tests for the change detection module.

use super::*;
use crate::diff::{Algorithm, DiffOp};
use crate::output::Memory;
use crate::types::Inode;
use std::time::SystemTime;

// ========================================================================
// DetectionOptions tests
// ========================================================================

#[test]
fn test_options_new_returns_defaults() {
    let opts = DetectionOptions::new();
    assert!(opts.get_prefix().is_none());
    assert_eq!(opts.get_algorithm(), Algorithm::Myers);
    assert!(opts.get_check_mtime());
    assert!(!opts.get_detect_moves());
    assert!(!opts.get_include_unchanged());
    assert!(!opts.get_force_rediff());
}

#[test]
fn test_options_default() {
    let opts = DetectionOptions::default();
    assert_eq!(
        opts.get_max_diff_size(),
        Some(DetectionOptions::DEFAULT_MAX_DIFF_SIZE)
    );
}

#[test]
fn test_options_prefix() {
    let opts = DetectionOptions::new().prefix("src/");
    assert_eq!(opts.get_prefix(), Some("src/"));
}

#[test]
fn test_options_prefix_empty() {
    let opts = DetectionOptions::new().prefix("");
    assert!(opts.get_prefix().is_none());
}

#[test]
fn test_options_algorithm() {
    let opts = DetectionOptions::new().algorithm(Algorithm::Patience);
    assert_eq!(opts.get_algorithm(), Algorithm::Patience);
}

#[test]
fn test_options_check_mtime() {
    let opts = DetectionOptions::new().check_mtime(false);
    assert!(!opts.get_check_mtime());
}

#[test]
fn test_options_detect_moves() {
    let opts = DetectionOptions::new().detect_moves(true);
    assert!(opts.get_detect_moves());
}

#[test]
fn test_options_include_unchanged() {
    let opts = DetectionOptions::new().include_unchanged(true);
    assert!(opts.get_include_unchanged());
}

#[test]
fn test_options_max_diff_size() {
    let opts = DetectionOptions::new().max_diff_size(1024);
    assert_eq!(opts.get_max_diff_size(), Some(1024));
}

#[test]
fn test_options_force_rediff() {
    let opts = DetectionOptions::new().force_rediff(true);
    assert!(opts.get_force_rediff());
}

#[test]
fn test_options_exceeds_max_size() {
    let opts = DetectionOptions::new().max_diff_size(1000);
    assert!(!opts.exceeds_max_size(500));
    assert!(!opts.exceeds_max_size(1000));
    assert!(opts.exceeds_max_size(1001));
}

#[test]
fn test_options_builder_chain() {
    let opts = DetectionOptions::new()
        .prefix("src/")
        .algorithm(Algorithm::Patience)
        .check_mtime(false)
        .detect_moves(true)
        .include_unchanged(true)
        .max_diff_size(1024)
        .force_rediff(true);

    assert_eq!(opts.get_prefix(), Some("src/"));
    assert_eq!(opts.get_algorithm(), Algorithm::Patience);
    assert!(!opts.get_check_mtime());
    assert!(opts.get_detect_moves());
    assert!(opts.get_include_unchanged());
    assert_eq!(opts.get_max_diff_size(), Some(1024));
    assert!(opts.get_force_rediff());
}

#[test]
fn test_options_clone() {
    let opts = DetectionOptions::new().prefix("src/");
    let cloned = opts.clone();
    assert_eq!(opts, cloned);
}

#[test]
fn test_options_debug() {
    let opts = DetectionOptions::new();
    let debug = format!("{:?}", opts);
    assert!(debug.contains("DetectionOptions"));
}

// ========================================================================
// DetectedFile tests
// ========================================================================

#[test]
fn test_detected_file_added() {
    let file = DetectedFile::added("new.rs");
    assert!(file.is_added());
    assert!(!file.is_deleted());
    assert!(!file.is_modified());
    assert!(!file.is_moved());
    assert!(!file.is_unchanged());
    assert!(file.has_changes());
    assert_eq!(file.path, "new.rs");
}

#[test]
fn test_detected_file_deleted() {
    let file = DetectedFile::deleted("old.rs");
    assert!(!file.is_added());
    assert!(file.is_deleted());
    assert!(!file.is_modified());
    assert!(file.has_changes());
}

#[test]
fn test_detected_file_modified() {
    let file = DetectedFile::modified("changed.rs");
    assert!(!file.is_added());
    assert!(!file.is_deleted());
    assert!(file.is_modified());
    assert!(file.has_changes());
}

#[test]
fn test_detected_file_moved() {
    let file = DetectedFile::moved("old/path.rs", "new/path.rs");
    assert!(file.is_moved());
    assert!(file.has_changes());
    assert_eq!(file.path, "new/path.rs");
    assert_eq!(file.old_path, Some("old/path.rs".to_string()));
}

#[test]
fn test_detected_file_unchanged() {
    let file = DetectedFile::unchanged("same.rs");
    assert!(file.is_unchanged());
    assert!(!file.has_changes());
}

#[test]
fn test_detected_file_with_inode() {
    let file = DetectedFile::added("test.rs").with_inode(Inode::new(42));
    assert_eq!(file.inode, Some(Inode::new(42)));
}

#[test]
fn test_detected_file_with_encoding() {
    use crate::change::Encoding;
    let file = DetectedFile::added("test.rs").with_encoding(Encoding::Utf8);
    assert_eq!(file.encoding, Some(Encoding::Utf8));
}

#[test]
fn test_detected_file_with_diff() {
    let diff_ops = vec![DiffOp::Insert {
        old_pos: 0,
        new_pos: 0,
        len: 1,
    }];
    let file = DetectedFile::modified("test.rs").with_diff(diff_ops);
    assert!(file.has_diff());
    assert_eq!(file.diff_count(), 1);
}

#[test]
fn test_detected_file_as_directory() {
    let file = DetectedFile::added("dir").as_directory();
    assert!(file.is_directory);
}

#[test]
fn test_detected_file_with_size() {
    let file = DetectedFile::added("test.rs").with_size(1024);
    assert_eq!(file.size, Some(1024));
}

#[test]
fn test_detected_file_with_mtime() {
    let now = SystemTime::now();
    let file = DetectedFile::added("test.rs").with_mtime(now);
    assert_eq!(file.mtime, Some(now));
}

#[test]
fn test_detected_file_clone() {
    let file = DetectedFile::added("test.rs");
    let cloned = file.clone();
    assert_eq!(file.path, cloned.path);
    assert_eq!(file.kind, cloned.kind);
}

// ========================================================================
// DetectionResult tests
// ========================================================================

#[test]
fn test_result_new() {
    let result = DetectionResult::new();
    assert!(result.is_empty());
    assert!(!result.has_errors());
    assert_eq!(result.total_count(), 0);
    assert_eq!(result.changed_count(), 0);
}

#[test]
fn test_result_add_added() {
    let mut result = DetectionResult::new();
    result.add_added(DetectedFile::added("new.rs"));

    assert!(!result.is_empty());
    assert_eq!(result.added_count(), 1);
    assert_eq!(result.changed_count(), 1);
}

#[test]
fn test_result_add_deleted() {
    let mut result = DetectionResult::new();
    result.add_deleted(DetectedFile::deleted("old.rs"));

    assert_eq!(result.deleted_count(), 1);
}

#[test]
fn test_result_add_modified() {
    let mut result = DetectionResult::new();
    result.add_modified(DetectedFile::modified("changed.rs"));

    assert_eq!(result.modified_count(), 1);
}

#[test]
fn test_result_add_moved() {
    let mut result = DetectionResult::new();
    result.add_moved(DetectedFile::moved("old.rs", "new.rs"));

    assert_eq!(result.moved_count(), 1);
}

#[test]
fn test_result_add_unchanged() {
    let mut result = DetectionResult::new();
    result.add_unchanged(DetectedFile::unchanged("same.rs"));

    assert_eq!(result.unchanged_count(), 1);
    assert_eq!(result.total_count(), 1);
    assert_eq!(result.changed_count(), 0); // unchanged doesn't count as changed
}

#[test]
fn test_result_add_error() {
    let mut result = DetectionResult::new();
    result.add_error("Something went wrong");

    assert!(result.has_errors());
    assert_eq!(result.errors().len(), 1);
}

#[test]
fn test_result_counters() {
    let mut result = DetectionResult::new();
    result.increment_scanned();
    result.increment_scanned();
    result.increment_skipped();

    assert_eq!(result.files_scanned(), 2);
    assert_eq!(result.files_skipped(), 1);
}

#[test]
fn test_result_getters() {
    let mut result = DetectionResult::new();
    result.add_added(DetectedFile::added("a.rs"));
    result.add_deleted(DetectedFile::deleted("d.rs"));
    result.add_modified(DetectedFile::modified("m.rs"));
    result.add_moved(DetectedFile::moved("o.rs", "n.rs"));
    result.add_unchanged(DetectedFile::unchanged("u.rs"));

    assert_eq!(result.added().len(), 1);
    assert_eq!(result.deleted().len(), 1);
    assert_eq!(result.modified().len(), 1);
    assert_eq!(result.moved().len(), 1);
    assert_eq!(result.unchanged().len(), 1);
}

#[test]
fn test_result_changed_files_iterator() {
    let mut result = DetectionResult::new();
    result.add_added(DetectedFile::added("a.rs"));
    result.add_deleted(DetectedFile::deleted("d.rs"));
    result.add_unchanged(DetectedFile::unchanged("u.rs"));

    let count = result.changed_files().count();
    assert_eq!(count, 2); // added + deleted
}

#[test]
fn test_result_all_files_iterator() {
    let mut result = DetectionResult::new();
    result.add_added(DetectedFile::added("a.rs"));
    result.add_unchanged(DetectedFile::unchanged("u.rs"));

    let count = result.all_files().count();
    assert_eq!(count, 2);
}

#[test]
fn test_result_merge() {
    let mut result1 = DetectionResult::new();
    result1.add_added(DetectedFile::added("a.rs"));
    result1.increment_scanned();

    let mut result2 = DetectionResult::new();
    result2.add_deleted(DetectedFile::deleted("d.rs"));
    result2.increment_scanned();
    result2.add_error("error");

    result1.merge(result2);

    assert_eq!(result1.added_count(), 1);
    assert_eq!(result1.deleted_count(), 1);
    assert_eq!(result1.files_scanned(), 2);
    assert!(result1.has_errors());
}

// ========================================================================
// detect_changes_simple tests
// ========================================================================

#[test]
fn test_detect_simple_empty() {
    let wc = Memory::new();
    let tracked: Vec<String> = vec![];
    let options = DetectionOptions::new();

    let result = detect_changes_simple(&wc, &tracked, &options);

    assert!(result.is_empty());
}

#[test]
fn test_detect_simple_added_files() {
    let wc = Memory::new();
    wc.add_file("new.rs", b"content");

    let tracked: Vec<String> = vec![];
    let options = DetectionOptions::new();

    let result = detect_changes_simple(&wc, &tracked, &options);

    assert_eq!(result.added_count(), 1);
    assert_eq!(result.added()[0].path, "new.rs");
}

#[test]
fn test_detect_simple_deleted_files() {
    let wc = Memory::new();

    let tracked = vec!["old.rs".to_string()];
    let options = DetectionOptions::new();

    let result = detect_changes_simple(&wc, &tracked, &options);

    assert_eq!(result.deleted_count(), 1);
    assert_eq!(result.deleted()[0].path, "old.rs");
}

#[test]
fn test_detect_simple_with_prefix() {
    let wc = Memory::new();
    wc.add_file("src/main.rs", b"content");
    wc.add_file("tests/test.rs", b"content");

    let tracked: Vec<String> = vec![];
    let options = DetectionOptions::new().prefix("src/");

    let result = detect_changes_simple(&wc, &tracked, &options);

    assert_eq!(result.added_count(), 1);
    assert_eq!(result.added()[0].path, "src/main.rs");
}

#[test]
fn test_detect_simple_unchanged_included() {
    let wc = Memory::new();
    wc.add_file("same.rs", b"content");

    let tracked = vec!["same.rs".to_string()];
    let options = DetectionOptions::new().include_unchanged(true);

    let result = detect_changes_simple(&wc, &tracked, &options);

    assert!(result.is_empty()); // is_empty checks changed files only
    assert_eq!(result.unchanged_count(), 1);
}

#[test]
fn test_detect_simple_unchanged_not_included() {
    let wc = Memory::new();
    wc.add_file("same.rs", b"content");

    let tracked = vec!["same.rs".to_string()];
    let options = DetectionOptions::new().include_unchanged(false);

    let result = detect_changes_simple(&wc, &tracked, &options);

    assert_eq!(result.unchanged_count(), 0);
}

#[test]
fn test_detect_simple_mixed_changes() {
    let wc = Memory::new();
    wc.add_file("new.rs", b"new");
    wc.add_file("existing.rs", b"existing");
    // old.rs is not in working copy

    let tracked = vec!["existing.rs".to_string(), "old.rs".to_string()];
    let options = DetectionOptions::new().include_unchanged(true);

    let result = detect_changes_simple(&wc, &tracked, &options);

    assert_eq!(result.added_count(), 1);
    assert_eq!(result.deleted_count(), 1);
    assert_eq!(result.unchanged_count(), 1);
}

#[test]
fn test_detect_simple_files_scanned() {
    let wc = Memory::new();
    wc.add_file("a.rs", b"a");
    wc.add_file("b.rs", b"b");

    let tracked: Vec<String> = vec![];
    let options = DetectionOptions::new();

    let result = detect_changes_simple(&wc, &tracked, &options);

    assert_eq!(result.files_scanned(), 2);
}

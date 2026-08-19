//! Tests for the record module.

use super::*;

use crate::status::{FileStatus, FileStatusEntry};
use atomic_core::change::{Change, ChangeHeader, Encoding, Provenance};
use atomic_core::diff::Algorithm;
use atomic_core::types::{Hash, Merkle};

// RecordOptions Tests

#[test]
fn test_options_new_returns_defaults() {
    let opts = RecordOptions::new();
    assert!(opts.get_paths().is_empty());
    assert!(!opts.all());
    assert_eq!(opts.algorithm(), Algorithm::Myers);
    assert_eq!(opts.default_encoding(), Encoding::Utf8);
    assert_eq!(opts.max_file_size(), RecordOptions::DEFAULT_MAX_FILE_SIZE);
    assert!(!opts.skip_binary());
    assert!(opts.get_record_empty_files());
    assert_eq!(
        opts.get_context_lines(),
        RecordOptions::DEFAULT_CONTEXT_LINES
    );
    assert!(opts.get_view().is_none());
    assert!(opts.get_message().is_none());
    assert!(opts.get_apply_after_record());
    assert!(opts.get_save_to_store());
    assert!(opts.get_detect_raw_renames());
}

#[test]
fn test_options_default() {
    let opts = RecordOptions::default();
    assert!(opts.get_paths().is_empty());
    assert!(!opts.all());
}

#[test]
fn test_options_paths() {
    let opts = RecordOptions::new().paths(vec!["src/main.rs", "src/lib.rs"]);
    assert_eq!(opts.get_paths().len(), 2);
    assert_eq!(opts.get_paths()[0], "src/main.rs");
}

#[test]
fn test_options_add_path() {
    let opts = RecordOptions::new()
        .add_path("src/main.rs")
        .add_path("src/lib.rs");
    assert_eq!(opts.get_paths().len(), 2);
}

#[test]
fn test_options_all() {
    let opts = RecordOptions::new().with_all(true);
    assert!(opts.all());
}

#[test]
fn test_options_algorithm() {
    let opts = RecordOptions::new().with_algorithm(Algorithm::Patience);
    assert_eq!(opts.algorithm(), Algorithm::Patience);
}

#[test]
fn test_options_default_encoding() {
    let opts = RecordOptions::new().with_default_encoding(Encoding::Binary);
    assert_eq!(opts.default_encoding(), Encoding::Binary);
}

#[test]
fn test_options_max_file_size() {
    let opts = RecordOptions::new().with_max_file_size(1024);
    assert_eq!(opts.max_file_size(), 1024);
}

#[test]
fn test_options_skip_binary() {
    let opts = RecordOptions::new().with_skip_binary(true);
    assert!(opts.skip_binary());
}

#[test]
fn test_options_record_empty_files() {
    let opts = RecordOptions::new().record_empty_files(true);
    assert!(opts.get_record_empty_files());
}

#[test]
fn test_options_context_lines() {
    let opts = RecordOptions::new().context_lines(5);
    assert_eq!(opts.get_context_lines(), 5);
}

#[test]
fn test_options_view() {
    let opts = RecordOptions::new().view("feature");
    assert_eq!(opts.get_view(), Some("feature"));
}

#[test]
fn test_options_message() {
    let opts = RecordOptions::new().message("Test message");
    assert_eq!(opts.get_message(), Some("Test message"));
}

#[test]
fn test_options_apply_after_record() {
    let opts = RecordOptions::new().apply_after_record(false);
    assert!(!opts.get_apply_after_record());
}

#[test]
fn test_options_save_to_store() {
    let opts = RecordOptions::new().save_to_store(false);
    assert!(!opts.get_save_to_store());
}

#[test]
fn test_options_detect_raw_renames() {
    let opts = RecordOptions::new().detect_raw_renames(false);
    assert!(!opts.get_detect_raw_renames());
}

#[test]
fn test_options_builder_chain() {
    let opts = RecordOptions::new()
        .paths(vec!["src/"])
        .with_all(false)
        .with_algorithm(Algorithm::Patience)
        .with_max_file_size(1024 * 1024)
        .with_skip_binary(true)
        .message("Test change")
        .view("feature");

    assert_eq!(opts.get_paths().len(), 1);
    assert!(!opts.all());
    assert_eq!(opts.algorithm(), Algorithm::Patience);
    assert!(opts.skip_binary());
    assert_eq!(opts.get_message(), Some("Test change"));
    assert_eq!(opts.get_view(), Some("feature"));
}

#[test]
fn test_options_should_include_all() {
    let opts = RecordOptions::new().with_all(true);
    assert!(opts.should_include("any/path/file.rs"));
}

#[test]
fn test_options_should_include_empty_paths() {
    let opts = RecordOptions::new();
    assert!(opts.should_include("any/path/file.rs"));
}

#[test]
fn test_options_should_include_specific_path() {
    let opts = RecordOptions::new().paths(vec!["src/main.rs"]);
    assert!(opts.should_include("src/main.rs"));
    assert!(!opts.should_include("src/lib.rs"));
}

#[test]
fn test_options_should_include_directory() {
    let opts = RecordOptions::new().paths(vec!["src"]);
    assert!(opts.should_include("src/main.rs"));
    assert!(opts.should_include("src/lib.rs"));
    assert!(!opts.should_include("tests/test.rs"));
}

#[test]
fn test_options_clone() {
    let opts1 = RecordOptions::new().message("test");
    let opts2 = opts1.clone();
    assert_eq!(opts2.get_message(), Some("test"));
}

#[test]
fn test_options_debug() {
    let opts = RecordOptions::new();
    let debug = format!("{:?}", opts);
    assert!(debug.contains("RecordOptions"));
}

// RecordError Tests

#[test]
fn test_error_nothing_to_record() {
    let err = RecordError::NothingToRecord;
    let msg = format!("{}", err);
    assert!(msg.contains("Nothing to record"));
}

#[test]
fn test_error_file_not_found() {
    let err = RecordError::FileNotFound {
        path: "test.rs".to_string(),
    };
    let msg = format!("{}", err);
    assert!(msg.contains("test.rs"));
}

#[test]
fn test_error_file_not_tracked() {
    let err = RecordError::FileNotTracked {
        path: "untracked.rs".to_string(),
    };
    let msg = format!("{}", err);
    assert!(msg.contains("untracked.rs"));
}

#[test]
fn test_error_file_too_large() {
    let err = RecordError::FileTooLarge {
        path: "big.bin".to_string(),
        size: 100_000_000,
        limit: 10_000_000,
    };
    let msg = format!("{}", err);
    assert!(msg.contains("big.bin"));
    assert!(msg.contains("100000000"));
}

// RecordStats Tests

#[test]
fn test_stats_new() {
    let stats = RecordStats::new();
    assert_eq!(stats.files_processed, 0);
    assert_eq!(stats.files_recorded, 0);
    assert_eq!(stats.hunks_created, 0);
}

#[test]
fn test_stats_has_changes() {
    let mut stats = RecordStats::new();
    assert!(!stats.has_changes());

    stats.files_recorded = 1;
    assert!(stats.has_changes());
}

#[test]
fn test_stats_total_atoms() {
    let mut stats = RecordStats::new();
    stats.vertices_added = 10;
    stats.edges_modified = 5;
    assert_eq!(stats.total_atoms(), 15);
}

#[test]
fn test_stats_has_errors() {
    let mut stats = RecordStats::new();
    assert!(!stats.has_errors());

    stats.errors = 1;
    assert!(stats.has_errors());
}

#[test]
fn test_stats_display() {
    let mut stats = RecordStats::new();
    stats.files_processed = 10;
    stats.files_recorded = 5;
    stats.files_skipped = 5;
    stats.hunks_created = 15;
    stats.vertices_added = 100;
    stats.edges_modified = 50;
    stats.content_bytes = 2048;
    stats.dependency_count = 2;
    stats.errors = 0;

    let display = format!("{}", stats);
    assert!(display.contains("5 file(s)"));
    assert!(display.contains("15 graph_op(s)"));
    assert!(display.contains("+100 vertices"));
    assert!(display.contains("~50 edges"));
    assert!(display.contains("2048 bytes"));
}

#[test]
fn test_stats_crdt_display() {
    let mut stats = RecordStats::new();
    stats.files_recorded = 2;
    stats.hunks_created = 3;
    stats.vertices_added = 10;
    stats.edges_modified = 5;
    stats.content_bytes = 512;
    stats.lines_added = 15;
    stats.lines_deleted = 3;
    stats.lines_modified = 2;
    stats.tokens_added = 45;
    stats.tokens_deleted = 8;
    stats.tokens_replaced = 4;

    let display = format!("{}", stats);
    // Basic stats
    assert!(display.contains("2 file(s)"));
    assert!(display.contains("3 graph_op(s)"));
    // CRDT stats
    assert!(display.contains("+15 -3 ~2 lines"));
    assert!(display.contains("+45 -8 ~4 tokens"));
}

#[test]
fn test_stats_total_line_changes() {
    let mut stats = RecordStats::new();
    stats.lines_added = 10;
    stats.lines_deleted = 5;
    stats.lines_modified = 3;
    assert_eq!(stats.total_line_changes(), 18);
}

#[test]
fn test_stats_total_token_ops() {
    let mut stats = RecordStats::new();
    stats.tokens_added = 20;
    stats.tokens_deleted = 8;
    stats.tokens_replaced = 2;
    assert_eq!(stats.total_token_ops(), 30);
}

#[test]
fn test_stats_has_crdt_stats() {
    let mut stats = RecordStats::new();
    assert!(!stats.has_crdt_stats());

    stats.lines_added = 1;
    assert!(stats.has_crdt_stats());

    let mut stats2 = RecordStats::new();
    stats2.tokens_added = 5;
    assert!(stats2.has_crdt_stats());
}

// RecordOutcome Tests

#[test]
fn test_outcome_new() {
    let header = ChangeHeader::builder().message("Test").build();
    let change = Change::empty(header);
    let hash = Hash::of(b"test");
    let stats = RecordStats::new();

    let outcome = RecordOutcome::new(change, hash, stats);
    assert!(!outcome.was_saved());
    assert!(!outcome.was_applied());
    assert!(outcome.new_state().is_none());
}

#[test]
fn test_outcome_set_saved() {
    let header = ChangeHeader::builder().message("Test").build();
    let change = Change::empty(header);
    let hash = Hash::of(b"test");
    let stats = RecordStats::new();

    let mut outcome = RecordOutcome::new(change, hash, stats);
    outcome.set_saved(true);
    assert!(outcome.was_saved());
}

#[test]
fn test_outcome_set_applied() {
    let header = ChangeHeader::builder().message("Test").build();
    let change = Change::empty(header);
    let hash = Hash::of(b"test");
    let stats = RecordStats::new();

    let mut outcome = RecordOutcome::new(change, hash, stats);
    let state = Merkle::of(b"state");
    outcome.set_applied(state);
    assert!(outcome.was_applied());
    assert_eq!(outcome.new_state(), Some(state));
}

#[test]
fn test_outcome_add_files() {
    let header = ChangeHeader::builder().message("Test").build();
    let change = Change::empty(header);
    let hash = Hash::of(b"test");
    let stats = RecordStats::new();

    let mut outcome = RecordOutcome::new(change, hash, stats);
    outcome.add_recorded_file("src/main.rs".to_string());
    outcome.add_skipped_file("src/test.rs".to_string());

    assert_eq!(outcome.recorded_files().len(), 1);
    assert_eq!(outcome.skipped_files().len(), 1);
}

#[test]
fn test_outcome_add_error() {
    let header = ChangeHeader::builder().message("Test").build();
    let change = Change::empty(header);
    let hash = Hash::of(b"test");
    let stats = RecordStats::new();

    let mut outcome = RecordOutcome::new(change, hash, stats);
    outcome.add_error("file.rs".to_string(), "read error".to_string());

    assert!(outcome.has_errors());
    assert_eq!(outcome.errors().len(), 1);
}

#[test]
fn test_outcome_display() {
    let header = ChangeHeader::builder().message("Test").build();
    let change = Change::empty(header);
    let hash = Hash::of(b"test");
    let stats = RecordStats::new();

    let mut outcome = RecordOutcome::new(change, hash, stats);
    outcome.set_saved(true);
    outcome.set_applied(Merkle::of(b"state"));

    let display = format!("{}", outcome);
    assert!(display.contains("Recorded change"));
    assert!(display.contains("[saved]"));
    assert!(display.contains("[applied]"));
}

#[test]
fn test_outcome_into_change() {
    let header = ChangeHeader::builder().message("Take me").build();
    let change = Change::empty(header);
    let hash = Hash::of(b"test");
    let stats = RecordStats::new();

    let outcome = RecordOutcome::new(change, hash, stats);
    let taken = outcome.into_change();
    assert_eq!(taken.message(), "Take me");
}

// Helper Function Tests

#[test]
fn test_build_header_with_options_message() {
    let header = ChangeHeader::builder().build(); // Empty message
    let options = RecordOptions::new().message("From options");
    let result = build_header(header, &options);
    assert_eq!(result.message, "From options");
}

#[test]
fn test_build_header_preserves_header_message() {
    let header = ChangeHeader::builder().message("From header").build();
    let options = RecordOptions::new().message("From options");
    let result = build_header(header, &options);
    assert_eq!(result.message, "From header");
}

#[test]
fn test_filter_files_empty() {
    let files: Vec<FileStatusEntry> = vec![];
    let options = RecordOptions::new();
    let filtered = filter_files(&files, &options);
    assert!(filtered.is_empty());
}

#[test]
fn test_filter_files_with_modified() {
    use std::path::PathBuf;

    let files = vec![
        FileStatusEntry::new(PathBuf::from("src/main.rs"), FileStatus::Modified),
        FileStatusEntry::new(PathBuf::from("README.md"), FileStatus::Clean),
        FileStatusEntry::new(PathBuf::from("src/lib.rs"), FileStatus::Added),
    ];
    let options = RecordOptions::new();
    let filtered = filter_files(&files, &options);
    assert_eq!(filtered.len(), 2);
}

// Provenance Tests

#[test]
fn test_options_provenance_default() {
    let opts = RecordOptions::new();
    assert!(!opts.has_provenance());
    assert!(opts.get_provenance().is_empty());
}

#[test]
fn test_options_provenance_add_single() {
    use atomic_core::change::{AITool, AIVendor, SuggestionType};

    let prov = Provenance::builder()
        .vendor(AIVendor::Anthropic)
        .model("claude-sonnet-4-20250514")
        .tool(AITool::Editor("zed".to_string()))
        .suggestion_type(SuggestionType::Collaborative)
        .build();

    let opts = RecordOptions::new().add_provenance(prov);
    assert!(opts.has_provenance());
    assert_eq!(opts.get_provenance().len(), 1);
    assert_eq!(opts.get_provenance()[0].vendor, AIVendor::Anthropic);
}

#[test]
fn test_options_provenance_set_vec() {
    use atomic_core::change::AIVendor;

    let prov1 = Provenance::builder()
        .vendor(AIVendor::Anthropic)
        .model("claude-sonnet-4-20250514")
        .build();

    let prov2 = Provenance::builder()
        .vendor(AIVendor::OpenAI)
        .model("gpt-4")
        .build();

    let opts = RecordOptions::new().provenance(vec![prov1, prov2]);
    assert!(opts.has_provenance());
    assert_eq!(opts.get_provenance().len(), 2);
}

#[test]
fn test_options_provenance_builder_chain() {
    use atomic_core::change::{AITool, AIVendor, SuggestionType};

    let prov = Provenance::builder()
        .vendor(AIVendor::Anthropic)
        .model("claude-sonnet-4-20250514")
        .tool(AITool::Cli("atomic".to_string()))
        .suggestion_type(SuggestionType::Complete)
        .input_tokens(1000)
        .output_tokens(500)
        .cost_usd(0.015)
        .build();

    let opts = RecordOptions::new()
        .message("AI-assisted change")
        .with_all(true)
        .add_provenance(prov);

    assert!(opts.has_provenance());
    assert_eq!(opts.get_message(), Some("AI-assisted change"));
    assert!(opts.all());
}

#[test]
fn test_options_to_assembly_options_includes_provenance() {
    use atomic_core::change::AIVendor;

    let prov = Provenance::builder()
        .vendor(AIVendor::Anthropic)
        .model("claude-sonnet-4-20250514")
        .build();

    let record_opts = RecordOptions::new().add_provenance(prov);
    let assembly_opts = record_opts.to_assembly_options();

    assert!(assembly_opts.has_provenance());
    assert_eq!(assembly_opts.get_provenance().len(), 1);
    assert_eq!(
        assembly_opts.get_provenance()[0].vendor,
        AIVendor::Anthropic
    );
}

#[test]
fn test_options_clone_preserves_provenance() {
    use atomic_core::change::AIVendor;

    let prov = Provenance::builder()
        .vendor(AIVendor::OpenAI)
        .model("gpt-4")
        .build();

    let opts1 = RecordOptions::new().add_provenance(prov);
    let opts2 = opts1.clone();

    assert!(opts2.has_provenance());
    assert_eq!(opts2.get_provenance().len(), 1);
    assert_eq!(opts2.get_provenance()[0].vendor, AIVendor::OpenAI);
}

//! Tests for the parallel recording pipeline.

use super::*;
use atomic_core::types::{ChangePosition, Inode, NodeId, Position};
use std::path::PathBuf;

// ── FileRecordInput ────────────────────────────────────────────

#[test]
fn test_input_added() {
    let input = FileRecordInput::added("src/main.rs".into(), "/repo/src/main.rs".into());
    assert!(input.is_added());
    assert!(!input.is_modified());
    assert!(!input.is_deleted());
    assert!(!input.is_directory());
    assert!(input.inode.is_none());
    assert!(input.old_content.is_empty());
}

#[test]
fn test_input_modified() {
    let inode = Inode::new(42);
    let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
    let input = FileRecordInput::modified(
        "src/lib.rs".into(),
        "/repo/src/lib.rs".into(),
        b"old content".to_vec(),
        inode,
        pos,
    );
    assert!(!input.is_added());
    assert!(input.is_modified());
    assert!(!input.is_deleted());
    assert!(input.inode.is_some());
    assert_eq!(input.old_content, b"old content");
}

#[test]
fn test_input_deleted() {
    let inode = Inode::new(99);
    let pos = Position::new(NodeId::new(2), ChangePosition::new(0));
    let input = FileRecordInput::deleted("old_file.rs".into(), inode, pos);
    assert!(!input.is_added());
    assert!(!input.is_modified());
    assert!(input.is_deleted());
    assert!(input.inode.is_some());
}

#[test]
fn test_input_directory_added() {
    let input = FileRecordInput::directory_added("src/".into());
    assert!(input.is_directory());
    assert!(!input.is_added());
}

#[test]
fn test_input_directory_deleted() {
    let inode = Inode::new(10);
    let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
    let input = FileRecordInput::directory_deleted("old_dir/".into(), inode, pos);
    assert!(input.is_directory());
    assert!(!input.is_deleted()); // is_deleted is for files only
}

#[test]
fn test_input_display() {
    let input = FileRecordInput::added("src/main.rs".into(), "/repo/src/main.rs".into());
    let display = format!("{}", input);
    assert!(display.contains("[add]"));
    assert!(display.contains("src/main.rs"));
}

#[test]
fn test_input_display_modified() {
    let inode = Inode::new(1);
    let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
    let input =
        FileRecordInput::modified("lib.rs".into(), "/repo/lib.rs".into(), vec![], inode, pos);
    let display = format!("{}", input);
    assert!(display.contains("[mod]"));
}

#[test]
fn test_input_display_deleted() {
    let inode = Inode::new(1);
    let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
    let input = FileRecordInput::deleted("gone.rs".into(), inode, pos);
    let display = format!("{}", input);
    assert!(display.contains("[del]"));
}

// ── FileRecordOutput ───────────────────────────────────────────

#[test]
fn test_output_skipped() {
    let output = FileRecordOutput::skipped("test.rs".into(), FileRecordKind::Added);
    assert!(output.was_skipped());
    assert!(!output.has_recording());
    assert!(output.recorded.is_none());
}

// ── ParallelRecordOptions ──────────────────────────────────────

#[test]
fn test_options_default() {
    let opts = ParallelRecordOptions::default();
    assert!(opts.parallel);
    assert_eq!(opts.parallel_threshold, 4);
}

#[test]
fn test_options_sequential() {
    let opts = ParallelRecordOptions::sequential();
    assert!(!opts.parallel);
}

#[test]
fn test_options_should_parallelize() {
    let opts = ParallelRecordOptions::default();
    assert!(!opts.should_parallelize(1));
    assert!(!opts.should_parallelize(3));
    assert!(opts.should_parallelize(4));
    assert!(opts.should_parallelize(100));
}

#[test]
fn test_options_sequential_never_parallelizes() {
    let opts = ParallelRecordOptions::sequential();
    assert!(!opts.should_parallelize(1000));
}

// ── ParallelRecordStats ────────────────────────────────────────

#[test]
fn test_parallel_stats_default() {
    let stats = ParallelRecordStats::default();
    assert_eq!(stats.files_processed, 0);
    assert_eq!(stats.files_recorded, 0);
    assert_eq!(stats.files_skipped, 0);
    assert_eq!(stats.files_errored, 0);
    assert!(!stats.used_parallel);
}

#[test]
fn test_parallel_stats_display_sequential() {
    let stats = ParallelRecordStats {
        files_processed: 10,
        files_recorded: 8,
        files_skipped: 2,
        files_errored: 0,
        used_parallel: false,
        wall_time_ms: 500,
        cpu_time_ms: 500,
        effective_parallelism: 1.0,
    };
    let display = format!("{}", stats);
    assert!(display.contains("10 files"));
    assert!(display.contains("8 recorded"));
    assert!(display.contains("2 skipped"));
    assert!(display.contains("sequential"));
}

#[test]
fn test_parallel_stats_display_parallel() {
    let stats = ParallelRecordStats {
        files_processed: 100,
        files_recorded: 95,
        files_skipped: 5,
        files_errored: 0,
        used_parallel: true,
        wall_time_ms: 500,
        cpu_time_ms: 2000,
        effective_parallelism: 4.0,
    };
    let display = format!("{}", stats);
    assert!(display.contains("100 files"));
    assert!(display.contains("parallel"));
    assert!(display.contains("4.0x"));
}

// ── parallel_record_files with real files ──────────────────────

#[test]
fn test_parallel_record_empty_inputs() {
    let (results, stats) = parallel_record_files(&[], &ParallelRecordOptions::default());
    assert!(results.is_empty());
    assert_eq!(stats.files_processed, 0);
    assert!(!stats.used_parallel); // below threshold
}

#[test]
fn test_parallel_record_with_temp_files() {
    let dir = tempfile::tempdir().unwrap();

    // Create some test files
    let file1 = dir.path().join("hello.txt");
    std::fs::write(&file1, "Hello, World!\n").unwrap();

    let file2 = dir.path().join("readme.md");
    std::fs::write(&file2, "# README\n\nThis is a test.\n").unwrap();

    let inputs = vec![
        FileRecordInput::added("hello.txt".into(), file1),
        FileRecordInput::added("readme.md".into(), file2),
    ];

    let opts = ParallelRecordOptions::default();
    let (results, stats) = parallel_record_files(&inputs, &opts);

    assert_eq!(results.len(), 2);
    assert_eq!(stats.files_processed, 2);
    assert!(!stats.used_parallel); // 2 files < threshold of 4

    // Both files should be recorded successfully
    for result in &results {
        let output = result.as_ref().expect("should succeed");
        assert!(output.has_recording());
        assert!(!output.was_skipped());
        assert!(output.stats.content_bytes > 0);
    }
}

#[test]
fn test_parallel_record_sequential_mode() {
    let dir = tempfile::tempdir().unwrap();
    let file1 = dir.path().join("test.txt");
    std::fs::write(&file1, "test content\n").unwrap();

    let inputs = vec![FileRecordInput::added("test.txt".into(), file1)];

    let opts = ParallelRecordOptions::sequential();
    let (results, stats) = parallel_record_files(&inputs, &opts);

    assert_eq!(results.len(), 1);
    assert!(!stats.used_parallel);
}

#[test]
fn test_parallel_record_handles_missing_file() {
    let inputs = vec![FileRecordInput::added(
        "nonexistent.txt".into(),
        PathBuf::from("/tmp/nonexistent_file_12345.txt"),
    )];

    let (results, stats) = parallel_record_files(&inputs, &ParallelRecordOptions::default());

    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
    assert_eq!(stats.files_errored, 1);
}

#[test]
fn test_parallel_record_directory_added() {
    let inputs = vec![FileRecordInput::directory_added("src/".into())];

    let (results, _stats) = parallel_record_files(&inputs, &ParallelRecordOptions::default());

    assert_eq!(results.len(), 1);
    let output = results[0].as_ref().expect("should succeed");
    assert!(output.has_recording());
    assert_eq!(output.stats.vertices_added, 2);
}

#[test]
fn test_parallel_record_many_files_uses_rayon() {
    let dir = tempfile::tempdir().unwrap();

    // Create enough files to trigger parallel processing
    let mut inputs = Vec::new();
    for i in 0..10 {
        let file = dir.path().join(format!("file_{}.txt", i));
        std::fs::write(&file, format!("content of file {}\n", i)).unwrap();
        inputs.push(FileRecordInput::added(format!("file_{}.txt", i), file));
    }

    let opts = ParallelRecordOptions::default();
    let (results, stats) = parallel_record_files(&inputs, &opts);

    assert_eq!(results.len(), 10);
    assert_eq!(stats.files_processed, 10);
    assert!(stats.used_parallel); // 10 files >= threshold of 4

    // All files should be recorded
    let recorded = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(recorded, 10);
}

// ── merge_parallel_results ─────────────────────────────────────

#[test]
fn test_merge_empty_results() {
    let merged = merge_parallel_results(vec![]);
    assert!(merged.recorded_files.is_empty());
    assert!(merged.recorded_paths.is_empty());
    assert!(merged.skipped_paths.is_empty());
    assert!(merged.deleted_paths.is_empty());
    assert!(merged.errors.is_empty());
}

#[test]
fn test_merge_with_skipped() {
    let results = vec![Ok(FileRecordOutput::skipped(
        "test.rs".into(),
        FileRecordKind::Added,
    ))];

    let merged = merge_parallel_results(results);
    assert!(merged.recorded_files.is_empty());
    assert_eq!(merged.skipped_paths.len(), 1);
    assert_eq!(merged.skipped_paths[0], "test.rs");
}

#[test]
fn test_merge_with_errors() {
    let results: Vec<Result<FileRecordOutput, String>> = vec![Err("something went wrong".into())];

    let merged = merge_parallel_results(results);
    assert!(merged.recorded_files.is_empty());
    assert_eq!(merged.errors.len(), 1);
}

#[test]
fn test_merge_with_directory() {
    use atomic_core::record::workflow::record::RecordedFile;

    let results = vec![Ok(FileRecordOutput {
        path: "src/".into(),
        kind: FileRecordKind::DirectoryAdded,
        recorded: Some(RecordedFile::new_directory("src/")),
        skipped: false,
        stats: FileRecordStats {
            vertices_added: 2,
            ..Default::default()
        },
    })];

    let merged = merge_parallel_results(results);
    assert_eq!(merged.recorded_files.len(), 1);
    assert_eq!(merged.stats.directories_recorded, 1);
    assert!(merged.recorded_paths[0].contains("directory"));
}

#[test]
fn test_merge_with_deleted() {
    use atomic_core::record::workflow::record::RecordedFile;

    let results = vec![Ok(FileRecordOutput {
        path: "old.rs".into(),
        kind: FileRecordKind::Deleted,
        recorded: Some(RecordedFile::new_deleted_directory("old.rs")),
        skipped: false,
        stats: FileRecordStats {
            edges_modified: 1,
            ..Default::default()
        },
    })];

    let merged = merge_parallel_results(results);
    assert_eq!(merged.deleted_paths.len(), 1);
    assert_eq!(merged.deleted_paths[0], "old.rs");
}

#[test]
fn test_merge_accumulates_stats() {
    use atomic_core::record::workflow::record::RecordedFile;

    let make_output =
        |path: &str, lines: usize, tokens: usize| -> Result<FileRecordOutput, String> {
            Ok(FileRecordOutput {
                path: path.into(),
                kind: FileRecordKind::Added,
                recorded: Some(RecordedFile::new(path)),
                skipped: false,
                stats: FileRecordStats {
                    hunks_created: 1,
                    vertices_added: 3,
                    content_bytes: 100,
                    lines_added: lines,
                    tokens_added: tokens,
                    ..Default::default()
                },
            })
        };

    let results = vec![
        make_output("a.rs", 10, 50),
        make_output("b.rs", 20, 100),
        make_output("c.rs", 30, 150),
    ];

    let merged = merge_parallel_results(results);
    assert_eq!(merged.stats.files_recorded, 3);
    assert_eq!(merged.stats.hunks_created, 3);
    assert_eq!(merged.stats.vertices_added, 9);
    assert_eq!(merged.stats.content_bytes, 300);
    assert_eq!(merged.stats.lines_added, 60);
    assert_eq!(merged.stats.tokens_added, 300);
}

// ── MergedStats display ────────────────────────────────────────

#[test]
fn test_merged_stats_display() {
    let stats = MergedStats {
        files_recorded: 10,
        hunks_created: 25,
        vertices_added: 30,
        edges_modified: 5,
        content_bytes: 50000,
        lines_added: 100,
        lines_deleted: 20,
        lines_modified: 5,
        tokens_added: 500,
        tokens_deleted: 100,
        tokens_replaced: 10,
        ..Default::default()
    };
    let display = format!("{}", stats);
    assert!(display.contains("10 files"));
    assert!(display.contains("25 hunks"));
    assert!(display.contains("lines"));
    assert!(display.contains("tokens"));
}

#[test]
fn test_merged_stats_display_no_crdt() {
    let stats = MergedStats {
        files_recorded: 3,
        hunks_created: 3,
        vertices_added: 9,
        edges_modified: 0,
        content_bytes: 1000,
        ..Default::default()
    };
    let display = format!("{}", stats);
    assert!(display.contains("3 files"));
    assert!(!display.contains("lines")); // no line stats to show
    assert!(!display.contains("tokens")); // no token stats to show
}

// ── FileRecordStats ────────────────────────────────────────────

#[test]
fn test_file_record_stats_default() {
    let stats = FileRecordStats::default();
    assert_eq!(stats.hunks_created, 0);
    assert_eq!(stats.processing_time_ms, 0);
}

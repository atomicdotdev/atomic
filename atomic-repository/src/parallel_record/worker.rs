//! Per-file processing functions for the parallel recording pipeline.
//!
//! These functions run on rayon worker threads during Phase 2. Each function
//! processes a single file in isolation with no shared mutable state.

use atomic_core::output::memory::Memory;
use atomic_core::record::workflow::detect::DetectedFile;
use atomic_core::record::workflow::record::{
    record_added_file, record_deleted_file, record_modified_file, RecordedFile, RecordingOptions,
};
use std::time::Instant;

use super::{FileRecordInput, FileRecordKind, FileRecordOutput, FileRecordStats};

/// Process a single file on a rayon worker thread.
///
/// This function is the per-file workhorse. It reads the file from disk,
/// diffs against old content, tokenizes, and builds hunks. It is designed
/// to be called from a rayon parallel iterator and must not access any
/// shared mutable state.
pub fn process_single_file(
    input: &FileRecordInput,
    options: &RecordingOptions,
) -> Result<FileRecordOutput, String> {
    let file_start = Instant::now();

    let result = match input.kind {
        FileRecordKind::Added => process_added_file(input, options),
        FileRecordKind::Modified => process_modified_file(input, options),
        FileRecordKind::Deleted => process_deleted_file(input, options),
        FileRecordKind::DirectoryAdded => process_directory_added(input),
        FileRecordKind::DirectoryDeleted => process_directory_deleted(input),
    };

    // Attach timing to the result
    let elapsed = file_start.elapsed().as_millis() as u64;
    result.map(|mut output| {
        output.stats.processing_time_ms = elapsed;
        output
    })
}

/// Process an added file: read content, detect encoding, create hunks, tokenize.
fn process_added_file(
    input: &FileRecordInput,
    options: &RecordingOptions,
) -> Result<FileRecordOutput, String> {
    // Read the file content from disk
    let content = std::fs::read(&input.full_path)
        .map_err(|e| format!("Failed to read {}: {}", input.path, e))?;

    // Check size limits
    if options.exceeds_max_size(content.len()) {
        if options.get_skip_binary() {
            return Ok(FileRecordOutput::skipped(
                input.path.clone(),
                FileRecordKind::Added,
            ));
        } else {
            return Err(format!(
                "File {} exceeds maximum size ({} bytes)",
                input.path,
                content.len(),
            ));
        }
    }

    // Create a per-file memory working copy (no shared state)
    let memory_wc = Memory::new();
    memory_wc.add_file(&input.path, &content);

    let detected = DetectedFile::added(&input.path);

    match record_added_file(&memory_wc, &detected, options) {
        Ok(recorded) => {
            if recorded.is_empty() {
                return Ok(FileRecordOutput::skipped(
                    input.path.clone(),
                    FileRecordKind::Added,
                ));
            }

            let crdt_stats = recorded.crdt_stats();
            let file_stats = FileRecordStats {
                hunks_created: recorded.hunk_count(),
                content_bytes: recorded.content_len() as u64,
                vertices_added: 3, // name + inode + content
                lines_added: crdt_stats.map_or(0, |s| s.lines_added),
                lines_deleted: crdt_stats.map_or(0, |s| s.lines_deleted),
                lines_modified: crdt_stats.map_or(0, |s| s.lines_modified),
                tokens_added: crdt_stats.map_or(0, |s| s.tokens_added),
                tokens_deleted: crdt_stats.map_or(0, |s| s.tokens_deleted),
                tokens_replaced: crdt_stats.map_or(0, |s| s.tokens_replaced),
                ..FileRecordStats::default()
            };

            Ok(FileRecordOutput {
                path: input.path.clone(),
                kind: FileRecordKind::Added,
                recorded: Some(recorded),
                skipped: false,
                stats: file_stats,
            })
        }
        Err(e) => Err(format!("Failed to record {}: {}", input.path, e)),
    }
}

/// Process a modified file: read new content, diff against old, create hunks.
fn process_modified_file(
    input: &FileRecordInput,
    options: &RecordingOptions,
) -> Result<FileRecordOutput, String> {
    // Read the new content from disk
    let new_content = std::fs::read(&input.full_path)
        .map_err(|e| format!("Failed to read {}: {}", input.path, e))?;

    // Check if content actually changed
    if input.old_content == new_content {
        return Ok(FileRecordOutput::skipped(
            input.path.clone(),
            FileRecordKind::Modified,
        ));
    }

    // Create a per-file memory working copy
    let memory_wc = Memory::new();
    memory_wc.add_file(&input.path, &new_content);

    let mut detected = DetectedFile::modified(&input.path);
    detected.inode = input.inode;
    detected.position = input.position;

    // The parallel worker has no pristine txn, so existing_branches is None.
    // CRDT Delete/Modify ops use placeholders here — acceptable for the
    // git-import path which overrides CRDT ops via build_crdt_ops_from_git_diff
    // at write_commit time.
    match record_modified_file(
        &memory_wc,
        &detected,
        &input.old_content,
        None,
        options,
        None,
    ) {
        Ok(recorded) => {
            if recorded.is_empty() {
                return Ok(FileRecordOutput::skipped(
                    input.path.clone(),
                    FileRecordKind::Modified,
                ));
            }

            let mut vertices_added: u64 = 0;
            let mut edges_modified: u64 = 0;
            for graph_op in recorded.hunks() {
                if graph_op.is_edit() {
                    vertices_added += 1;
                } else if graph_op.is_replace() {
                    vertices_added += 1;
                    edges_modified += 1;
                } else if graph_op.is_delete() {
                    edges_modified += 1;
                }
            }
            let crdt_stats = recorded.crdt_stats();
            let file_stats = FileRecordStats {
                hunks_created: recorded.hunk_count(),
                content_bytes: recorded.content_len() as u64,
                vertices_added,
                edges_modified,
                lines_added: crdt_stats.map_or(0, |s| s.lines_added),
                lines_deleted: crdt_stats.map_or(0, |s| s.lines_deleted),
                lines_modified: crdt_stats.map_or(0, |s| s.lines_modified),
                tokens_added: crdt_stats.map_or(0, |s| s.tokens_added),
                tokens_deleted: crdt_stats.map_or(0, |s| s.tokens_deleted),
                tokens_replaced: crdt_stats.map_or(0, |s| s.tokens_replaced),
                ..FileRecordStats::default()
            };

            Ok(FileRecordOutput {
                path: input.path.clone(),
                kind: FileRecordKind::Modified,
                recorded: Some(recorded),
                skipped: false,
                stats: file_stats,
            })
        }
        Err(e) => Err(format!("Failed to record {}: {}", input.path, e)),
    }
}

/// Process a deleted file: create deletion hunks.
fn process_deleted_file(
    input: &FileRecordInput,
    options: &RecordingOptions,
) -> Result<FileRecordOutput, String> {
    let mut detected = DetectedFile::deleted(&input.path);
    detected.inode = input.inode;
    detected.position = input.position;

    match record_deleted_file(&detected, options) {
        Ok(recorded) => {
            let crdt_stats = recorded.crdt_stats();
            let file_stats = FileRecordStats {
                hunks_created: recorded.hunk_count(),
                edges_modified: 1,
                lines_added: crdt_stats.map_or(0, |s| s.lines_added),
                lines_deleted: crdt_stats.map_or(0, |s| s.lines_deleted),
                lines_modified: crdt_stats.map_or(0, |s| s.lines_modified),
                tokens_added: crdt_stats.map_or(0, |s| s.tokens_added),
                tokens_deleted: crdt_stats.map_or(0, |s| s.tokens_deleted),
                tokens_replaced: crdt_stats.map_or(0, |s| s.tokens_replaced),
                ..FileRecordStats::default()
            };

            Ok(FileRecordOutput {
                path: input.path.clone(),
                kind: FileRecordKind::Deleted,
                recorded: Some(recorded),
                skipped: false,
                stats: file_stats,
            })
        }
        Err(e) => Err(format!(
            "Failed to record deletion of {}: {}",
            input.path, e
        )),
    }
}

/// Process an added directory.
fn process_directory_added(input: &FileRecordInput) -> Result<FileRecordOutput, String> {
    let recorded = RecordedFile::new_directory(&input.path);

    let file_stats = FileRecordStats {
        vertices_added: 2, // name + inode
        ..FileRecordStats::default()
    };

    Ok(FileRecordOutput {
        path: input.path.clone(),
        kind: FileRecordKind::DirectoryAdded,
        recorded: Some(recorded),
        skipped: false,
        stats: file_stats,
    })
}

/// Process a deleted directory.
fn process_directory_deleted(input: &FileRecordInput) -> Result<FileRecordOutput, String> {
    let mut recorded = RecordedFile::new_deleted_directory(&input.path);

    if let Some(inode) = input.inode {
        recorded.set_inode(inode);
    }
    if let Some(position) = input.position {
        recorded.set_position(position);
    }

    let file_stats = FileRecordStats {
        edges_modified: 1,
        ..FileRecordStats::default()
    };

    Ok(FileRecordOutput {
        path: input.path.clone(),
        kind: FileRecordKind::DirectoryDeleted,
        recorded: Some(recorded),
        skipped: false,
        stats: file_stats,
    })
}

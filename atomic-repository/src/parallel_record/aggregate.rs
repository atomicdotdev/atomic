//! Result aggregation for the parallel recording pipeline.
//!
//! This module provides [`merge_parallel_results`] which collects per-file
//! results from Phase 2 and produces the aggregate data structures needed
//! by `Repository::record()` in Phase 3.

use atomic_core::record::workflow::record::RecordedFile;
use std::fmt;

use super::{FileRecordKind, FileRecordOutput};

/// Merge parallel results into the aggregate vectors needed by `Repository::record()`.
///
/// This extracts the `RecordedFile` values from the parallel results and
/// separates them into recorded/skipped/error categories, matching the
/// existing record flow's data structures.
///
/// # Arguments
///
/// * `results` - The per-file results from [`super::parallel_record_files`].
///
/// # Returns
///
/// A [`MergedRecordResults`] containing recorded files, paths, errors, and stats.
pub fn merge_parallel_results(
    results: Vec<Result<FileRecordOutput, String>>,
) -> MergedRecordResults {
    let mut recorded_files: Vec<RecordedFile> = Vec::new();
    let mut recorded_paths = Vec::new();
    let mut skipped_paths = Vec::new();
    let mut deleted_paths = Vec::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    let mut files_recorded_count = 0usize;
    let mut total_hunks = 0usize;
    let mut total_vertices = 0u64;
    let mut total_edges = 0u64;
    let mut total_content_bytes = 0u64;
    let mut total_lines_added = 0usize;
    let mut total_lines_deleted = 0usize;
    let mut total_lines_modified = 0usize;
    let mut total_tokens_added = 0usize;
    let mut total_tokens_deleted = 0usize;
    let mut total_tokens_replaced = 0usize;
    let mut directories_recorded = 0usize;

    for result in results {
        match result {
            Ok(output) => {
                if output.skipped {
                    skipped_paths.push(output.path.clone());
                    continue;
                }

                if let Some(recorded) = output.recorded {
                    // Track paths by kind
                    match output.kind {
                        FileRecordKind::Deleted | FileRecordKind::DirectoryDeleted => {
                            deleted_paths.push(output.path.clone());
                            recorded_paths.push(output.path.clone());
                        }
                        FileRecordKind::DirectoryAdded => {
                            directories_recorded += 1;
                            recorded_paths.push(format!("{}/ (directory)", output.path));
                        }
                        _ => {
                            recorded_paths.push(output.path.clone());
                        }
                    }

                    // Accumulate stats
                    total_hunks += output.stats.hunks_created;
                    total_vertices += output.stats.vertices_added;
                    total_edges += output.stats.edges_modified;
                    total_content_bytes += output.stats.content_bytes;
                    total_lines_added += output.stats.lines_added;
                    total_lines_deleted += output.stats.lines_deleted;
                    total_lines_modified += output.stats.lines_modified;
                    total_tokens_added += output.stats.tokens_added;
                    total_tokens_deleted += output.stats.tokens_deleted;
                    total_tokens_replaced += output.stats.tokens_replaced;

                    files_recorded_count += 1;
                    recorded_files.push(recorded);
                } else {
                    skipped_paths.push(output.path.clone());
                }
            }
            Err(e) => {
                // Extract path from error message if possible, else use "unknown"
                errors.push(("unknown".to_string(), e));
            }
        }
    }

    MergedRecordResults {
        recorded_paths,
        skipped_paths,
        deleted_paths,
        errors,
        stats: MergedStats {
            files_recorded: files_recorded_count,
            directories_recorded,
            hunks_created: total_hunks,
            vertices_added: total_vertices,
            edges_modified: total_edges,
            content_bytes: total_content_bytes,
            lines_added: total_lines_added,
            lines_deleted: total_lines_deleted,
            lines_modified: total_lines_modified,
            tokens_added: total_tokens_added,
            tokens_deleted: total_tokens_deleted,
            tokens_replaced: total_tokens_replaced,
        },
        recorded_files,
    }
}

/// Merged results from [`merge_parallel_results`].
///
/// This provides the same data structures that `Repository::record()`
/// uses internally, making it easy to integrate the parallel pipeline.
pub struct MergedRecordResults {
    /// Files that produced recordings (with hunks).
    pub recorded_files: Vec<RecordedFile>,

    /// Paths of files that were recorded.
    pub recorded_paths: Vec<String>,

    /// Paths of files that were skipped.
    pub skipped_paths: Vec<String>,

    /// Paths of files that were deleted.
    pub deleted_paths: Vec<String>,

    /// Errors encountered during processing: (path, error_message).
    pub errors: Vec<(String, String)>,

    /// Aggregate statistics.
    pub stats: MergedStats,
}

/// Aggregate statistics from merged parallel results.
#[derive(Debug, Clone, Default)]
pub struct MergedStats {
    /// Number of files that produced recordings.
    pub files_recorded: usize,

    /// Number of directories recorded.
    pub directories_recorded: usize,

    /// Total hunks created across all files.
    pub hunks_created: usize,

    /// Total vertices added.
    pub vertices_added: u64,

    /// Total edges modified.
    pub edges_modified: u64,

    /// Total content bytes.
    pub content_bytes: u64,

    /// Total lines added.
    pub lines_added: usize,

    /// Total lines deleted.
    pub lines_deleted: usize,

    /// Total lines modified.
    pub lines_modified: usize,

    /// Total tokens added.
    pub tokens_added: usize,

    /// Total tokens deleted.
    pub tokens_deleted: usize,

    /// Total tokens replaced.
    pub tokens_replaced: usize,
}

impl fmt::Display for MergedStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} files, {} hunks, +{} vertices, ~{} edges, {} bytes",
            self.files_recorded,
            self.hunks_created,
            self.vertices_added,
            self.edges_modified,
            self.content_bytes,
        )?;

        if self.lines_added > 0 || self.lines_deleted > 0 {
            write!(
                f,
                ", {} lines (+{} -{} ~{})",
                self.lines_added + self.lines_deleted + self.lines_modified,
                self.lines_added,
                self.lines_deleted,
                self.lines_modified,
            )?;
        }

        if self.tokens_added > 0 || self.tokens_deleted > 0 {
            write!(
                f,
                ", {} tokens (+{} -{} ~{})",
                self.tokens_added + self.tokens_deleted + self.tokens_replaced,
                self.tokens_added,
                self.tokens_deleted,
                self.tokens_replaced,
            )?;
        }

        Ok(())
    }
}

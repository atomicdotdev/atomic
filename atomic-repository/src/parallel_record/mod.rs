//! Parallel recording pipeline using rayon for concurrent per-file processing.
//!
//! This module provides [`parallel_record_files`], which distributes the expensive
//! per-file work (I/O, diffing, tokenization, CRDT generation) across all available
//! CPU cores using rayon's work-stealing thread pool.
//!
//! The recording pipeline has three phases:
//!
//! 1. **Pre-pass** (sequential): Look up inodes/positions, retrieve old content
//! 2. **Per-file processing** (parallel via rayon): Read, diff, tokenize, build hunks
//! 3. **Merge** (sequential): Collect results, accumulate stats, assemble change

pub mod aggregate;
pub mod worker;

#[cfg(test)]
mod tests;

use atomic_core::record::workflow::record::RecordingOptions;
use atomic_core::types::{Inode, NodeId, Position};
use std::fmt;
use std::path::PathBuf;
use std::time::Instant;

// Re-export worker and aggregate items for convenience
pub use aggregate::{merge_parallel_results, MergedRecordResults, MergedStats};
pub use worker::process_single_file;

// ═══════════════════════════════════════════════════════════════════════
// FileRecordInput — per-file input for parallel processing
// ═══════════════════════════════════════════════════════════════════════

/// Input descriptor for recording a single file.
///
/// This struct carries all the data needed to process one file in isolation
/// on a rayon worker thread. It is built during the sequential pre-pass
/// (Phase 1) and consumed during the parallel processing pass (Phase 2).
///
/// # Thread Safety
///
/// `FileRecordInput` is `Send` because it contains only owned data.
#[derive(Debug, Clone)]
pub struct FileRecordInput {
    /// Relative path within the repository (e.g., "src/main.rs").
    pub path: String,

    /// Absolute path on disk for reading the file content.
    pub full_path: PathBuf,

    /// The kind of change detected for this file.
    pub kind: FileRecordKind,

    /// Old content from the pristine graph (for modified files).
    /// Empty for added files, irrelevant for deleted files.
    pub old_content: Vec<u8>,

    /// The file's inode in the pristine (if known).
    /// Required for modified and deleted files; `None` for added files.
    pub inode: Option<Inode>,

    /// The file's graph position in the pristine (if known).
    /// Required for modified and deleted files; `None` for added files.
    pub position: Option<Position<NodeId>>,
}

/// The kind of change detected for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRecordKind {
    /// File was added (new file).
    Added,
    /// File was modified (content changed).
    Modified,
    /// File was deleted.
    Deleted,
    /// Directory was added.
    DirectoryAdded,
    /// Directory was deleted.
    DirectoryDeleted,
}

impl FileRecordInput {
    /// Create an input for a newly added file.
    pub fn added(path: String, full_path: PathBuf) -> Self {
        Self {
            path,
            full_path,
            kind: FileRecordKind::Added,
            old_content: Vec::new(),
            inode: None,
            position: None,
        }
    }

    /// Create an input for a modified file.
    pub fn modified(
        path: String,
        full_path: PathBuf,
        old_content: Vec<u8>,
        inode: Inode,
        position: Position<NodeId>,
    ) -> Self {
        Self {
            path,
            full_path,
            kind: FileRecordKind::Modified,
            old_content,
            inode: Some(inode),
            position: Some(position),
        }
    }

    /// Create an input for a deleted file.
    pub fn deleted(path: String, inode: Inode, position: Position<NodeId>) -> Self {
        Self {
            path,
            full_path: PathBuf::new(),
            kind: FileRecordKind::Deleted,
            old_content: Vec::new(),
            inode: Some(inode),
            position: Some(position),
        }
    }

    /// Create an input for an added directory.
    pub fn directory_added(path: String) -> Self {
        Self {
            path,
            full_path: PathBuf::new(),
            kind: FileRecordKind::DirectoryAdded,
            old_content: Vec::new(),
            inode: None,
            position: None,
        }
    }

    /// Create an input for a deleted directory.
    pub fn directory_deleted(path: String, inode: Inode, position: Position<NodeId>) -> Self {
        Self {
            path,
            full_path: PathBuf::new(),
            kind: FileRecordKind::DirectoryDeleted,
            old_content: Vec::new(),
            inode: Some(inode),
            position: Some(position),
        }
    }

    /// Returns `true` if this is an added file (not directory).
    pub fn is_added(&self) -> bool {
        self.kind == FileRecordKind::Added
    }

    /// Returns `true` if this is a modified file.
    pub fn is_modified(&self) -> bool {
        self.kind == FileRecordKind::Modified
    }

    /// Returns `true` if this is a deleted file.
    pub fn is_deleted(&self) -> bool {
        self.kind == FileRecordKind::Deleted
    }

    /// Returns `true` if this is a directory operation.
    pub fn is_directory(&self) -> bool {
        matches!(
            self.kind,
            FileRecordKind::DirectoryAdded | FileRecordKind::DirectoryDeleted
        )
    }
}

impl fmt::Display for FileRecordInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            FileRecordKind::Added => "add",
            FileRecordKind::Modified => "mod",
            FileRecordKind::Deleted => "del",
            FileRecordKind::DirectoryAdded => "dir+",
            FileRecordKind::DirectoryDeleted => "dir-",
        };
        write!(f, "[{}] {}", kind, self.path)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// FileRecordOutput — per-file output from parallel processing
// ═══════════════════════════════════════════════════════════════════════

/// Output from recording a single file.
///
/// Produced by the parallel processing pass (Phase 2) and consumed by
/// the merge pass (Phase 3) to assemble the final change.
#[derive(Debug)]
pub struct FileRecordOutput {
    /// The relative path of the file.
    pub path: String,

    /// The kind of change.
    pub kind: FileRecordKind,

    /// The recorded file with hunks and CRDT ops.
    ///
    /// `None` if the file was skipped (e.g., content unchanged, empty, binary).
    pub recorded: Option<atomic_core::record::workflow::record::RecordedFile>,

    /// Whether this file was skipped (not an error, just no changes).
    pub skipped: bool,

    /// Per-file statistics.
    pub stats: FileRecordStats,
}

/// Per-file statistics from the recording process.
#[derive(Debug, Clone, Default)]
pub struct FileRecordStats {
    /// Number of hunks created for this file.
    pub hunks_created: usize,

    /// Number of vertices added.
    pub vertices_added: u64,

    /// Number of edges modified.
    pub edges_modified: u64,

    /// Content bytes in this file.
    pub content_bytes: u64,

    /// Lines added (from CRDT stats).
    pub lines_added: usize,

    /// Lines deleted (from CRDT stats).
    pub lines_deleted: usize,

    /// Lines modified (from CRDT stats).
    pub lines_modified: usize,

    /// Tokens added (from CRDT stats).
    pub tokens_added: usize,

    /// Tokens deleted (from CRDT stats).
    pub tokens_deleted: usize,

    /// Tokens replaced (from CRDT stats).
    pub tokens_replaced: usize,

    /// Time spent processing this file in milliseconds.
    pub processing_time_ms: u64,
}

impl FileRecordOutput {
    /// Create an output for a skipped file.
    pub(crate) fn skipped(path: String, kind: FileRecordKind) -> Self {
        Self {
            path,
            kind,
            recorded: None,
            skipped: true,
            stats: FileRecordStats::default(),
        }
    }

    /// Returns `true` if this file produced a recording (was not skipped).
    pub fn has_recording(&self) -> bool {
        self.recorded.is_some()
    }

    /// Returns `true` if this file was skipped.
    pub fn was_skipped(&self) -> bool {
        self.skipped
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ParallelRecordOptions — configuration for parallel recording
// ═══════════════════════════════════════════════════════════════════════

/// Configuration for the parallel recording pipeline.
///
/// This wraps the core `RecordingOptions` and adds parallelism-specific
/// settings.
///
/// # Examples
///
/// ```rust
/// use atomic_repository::parallel_record::ParallelRecordOptions;
///
/// let opts = ParallelRecordOptions::default();
/// assert!(opts.parallel);
///
/// // Force sequential processing (useful for debugging)
/// let opts = ParallelRecordOptions::sequential();
/// assert!(!opts.parallel);
/// ```
#[derive(Debug, Clone)]
pub struct ParallelRecordOptions {
    /// Whether to use rayon parallel processing.
    ///
    /// When `false`, files are processed sequentially (useful for debugging
    /// or when rayon overhead exceeds the benefit for small changesets).
    ///
    /// Default: `true`.
    pub parallel: bool,

    /// Minimum number of files before switching to parallel mode.
    ///
    /// For very small changesets (1-3 files), the overhead of rayon's
    /// thread pool setup may exceed the benefit. Files below this threshold
    /// are processed sequentially even when `parallel` is `true`.
    ///
    /// Default: `4`.
    pub parallel_threshold: usize,

    /// The core recording options (encoding, size limits, algorithm, etc.).
    pub core_options: RecordingOptions,
}

impl ParallelRecordOptions {
    /// Create options with default settings and the given core options.
    pub fn with_core_options(core_options: RecordingOptions) -> Self {
        Self {
            parallel: true,
            parallel_threshold: 4,
            core_options,
        }
    }

    /// Force sequential processing (no rayon).
    pub fn sequential() -> Self {
        Self {
            parallel: false,
            parallel_threshold: usize::MAX,
            core_options: RecordingOptions::new(),
        }
    }

    /// Returns `true` if parallel processing should be used for the given file count.
    pub fn should_parallelize(&self, file_count: usize) -> bool {
        self.parallel && file_count >= self.parallel_threshold
    }
}

impl Default for ParallelRecordOptions {
    fn default() -> Self {
        Self {
            parallel: true,
            parallel_threshold: 4,
            core_options: RecordingOptions::new(),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// ParallelRecordStats — aggregate statistics
// ═══════════════════════════════════════════════════════════════════════

/// Aggregate statistics from a parallel recording operation.
///
/// Returned by [`parallel_record_files`] alongside the per-file results.
#[derive(Debug, Clone, Default)]
pub struct ParallelRecordStats {
    /// Total number of files processed (input count).
    pub files_processed: usize,

    /// Number of files that produced recordings.
    pub files_recorded: usize,

    /// Number of files skipped (no changes, empty, binary, etc.).
    pub files_skipped: usize,

    /// Number of files that had errors.
    pub files_errored: usize,

    /// Whether parallel processing was used.
    pub used_parallel: bool,

    /// Wall-clock time for the parallel processing pass in milliseconds.
    pub wall_time_ms: u64,

    /// Sum of per-file processing times in milliseconds.
    ///
    /// This can exceed `wall_time_ms` because files are processed on
    /// multiple cores simultaneously. The ratio `cpu_time_ms / wall_time_ms`
    /// indicates the effective parallelism achieved.
    pub cpu_time_ms: u64,

    /// Effective parallelism (cpu_time / wall_time).
    ///
    /// A value of 4.0 means the work of 4 sequential seconds was completed
    /// in 1 wall-clock second across multiple cores.
    pub effective_parallelism: f64,
}

impl fmt::Display for ParallelRecordStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} files processed ({} recorded, {} skipped, {} errors) in {}ms",
            self.files_processed,
            self.files_recorded,
            self.files_skipped,
            self.files_errored,
            self.wall_time_ms,
        )?;

        if self.used_parallel {
            write!(
                f,
                " [parallel, {:.1}x speedup, {}ms CPU]",
                self.effective_parallelism, self.cpu_time_ms,
            )?;
        } else {
            write!(f, " [sequential]")?;
        }

        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// parallel_record_files — the main parallel processing function
// ═══════════════════════════════════════════════════════════════════════

/// Process files in parallel using rayon, returning per-file results.
///
/// This is the core of the parallel recording pipeline (Phase 2). It takes
/// a list of [`FileRecordInput`] descriptors (built during the sequential
/// pre-pass) and processes each file independently on a rayon worker thread.
///
/// # Automatic Sequential Fallback
///
/// If `options.should_parallelize(inputs.len())` returns `false` (e.g., fewer
/// than 4 files), processing falls back to sequential iteration to avoid
/// rayon overhead.
pub fn parallel_record_files(
    inputs: &[FileRecordInput],
    options: &ParallelRecordOptions,
) -> (Vec<Result<FileRecordOutput, String>>, ParallelRecordStats) {
    use rayon::prelude::*;

    let start = Instant::now();
    let use_parallel = options.should_parallelize(inputs.len());

    let results: Vec<Result<FileRecordOutput, String>> = if use_parallel {
        inputs
            .par_iter()
            .map(|input| process_single_file(input, &options.core_options))
            .collect()
    } else {
        inputs
            .iter()
            .map(|input| process_single_file(input, &options.core_options))
            .collect()
    };

    let wall_time_ms = start.elapsed().as_millis() as u64;

    // Compute aggregate stats
    let mut stats = ParallelRecordStats {
        files_processed: inputs.len(),
        used_parallel: use_parallel,
        wall_time_ms,
        ..Default::default()
    };

    let mut cpu_time_ms = 0u64;
    for result in &results {
        match result {
            Ok(output) => {
                cpu_time_ms += output.stats.processing_time_ms;
                if output.skipped {
                    stats.files_skipped += 1;
                } else if output.recorded.is_some() {
                    stats.files_recorded += 1;
                } else {
                    stats.files_skipped += 1;
                }
            }
            Err(_) => {
                stats.files_errored += 1;
            }
        }
    }

    stats.cpu_time_ms = cpu_time_ms;
    stats.effective_parallelism = if wall_time_ms > 0 {
        cpu_time_ms as f64 / wall_time_ms as f64
    } else {
        1.0
    };

    (results, stats)
}

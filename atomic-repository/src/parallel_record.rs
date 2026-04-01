//! Parallel recording pipeline using rayon for concurrent per-file processing.
//!
//! This module provides [`parallel_record_files`], which distributes the expensive
//! per-file work (I/O, diffing, tokenization, CRDT generation) across all available
//! CPU cores using rayon's work-stealing thread pool.
//!
//! # Architecture
//!
//! The recording pipeline has three phases:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │  Phase 1: PRE-PASS (sequential)                                         │
//! │                                                                          │
//! │  For each file in files_to_record:                                       │
//! │    - Look up inode/position from pristine (requires read txn)            │
//! │    - Retrieve old content from graph (for modified files)                │
//! │    - Build a FileRecordInput descriptor                                  │
//! │                                                                          │
//! │  This phase is sequential because it accesses the pristine database      │
//! │  through a shared read transaction.                                      │
//! ├──────────────────────────────────────────────────────────────────────────┤
//! │  Phase 2: PER-FILE PROCESSING (parallel — rayon)                         │
//! │                                                                          │
//! │  ┌──────────────┐ ┌──────────────┐ ┌──────────────┐                     │
//! │  │  Thread 1    │ │  Thread 2    │ │  Thread N    │                     │
//! │  │              │ │              │ │              │                     │
//! │  │  Read file   │ │  Read file   │ │  Read file   │                     │
//! │  │  Detect enc  │ │  Detect enc  │ │  Detect enc  │                     │
//! │  │  Diff old/new│ │  Diff old/new│ │  Diff old/new│                     │
//! │  │  Tokenize    │ │  Tokenize    │ │  Tokenize    │                     │
//! │  │  Build hunks │ │  Build hunks │ │  Build hunks │                     │
//! │  │  Build CRDT  │ │  Build CRDT  │ │  Build CRDT  │                     │
//! │  └──────┬───────┘ └──────┬───────┘ └──────┬───────┘                     │
//! │         │                │                │                              │
//! │         └────────────────┼────────────────┘                              │
//! │                          │                                               │
//! │                          ▼                                               │
//! │              Vec<FileRecordResult>                                        │
//! ├──────────────────────────────────────────────────────────────────────────┤
//! │  Phase 3: MERGE (sequential)                                             │
//! │                                                                          │
//! │  - Collect parallel results into recorded_files                          │
//! │  - Accumulate stats                                                      │
//! │  - Assemble change (globalize, build hash table, serialize)              │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Performance
//!
//! The key insight is that per-file work is **embarrassingly parallel** — each
//! file's diffing and tokenization is completely independent. The only sequential
//! parts are pristine lookups (Phase 1) and change assembly (Phase 3), which
//! are typically fast compared to the per-file processing.
//!
//! For the initial record of the `atomic` project (261 files, 227K lines, 1.5M tokens):
//! - Phase 1 (pristine lookups): ~10ms
//! - Phase 2 (per-file processing): ~3s sequential → ~0.5s on 8 cores
//! - Phase 3 (assembly + serialization): ~200ms
//!
//! # Thread Safety
//!
//! - `FileRecordInput` is `Send` — it carries owned data for each file.
//! - `FileRecordResult` is `Send` — it carries the recording output.
//! - The rayon parallel iterator handles work distribution automatically.
//! - No shared mutable state during Phase 2.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_repository::parallel_record::{
//!     parallel_record_files, FileRecordInput, ParallelRecordOptions,
//! };
//!
//! let inputs: Vec<FileRecordInput> = /* ... build from status entries ... */;
//! let options = ParallelRecordOptions::default();
//!
//! let results = parallel_record_files(&inputs, &options);
//!
//! for result in &results {
//!     match result {
//!         Ok(output) => println!("{}: {} hunks", output.path, output.hunk_count),
//!         Err(e) => eprintln!("Error: {}", e),
//!     }
//! }
//! ```

use atomic_core::output::memory::Memory;
use atomic_core::record::workflow::detect::DetectedFile;
use atomic_core::record::workflow::record::{
    record_added_file, record_deleted_file, record_modified_file, RecordedFile, RecordingOptions,
};
use atomic_core::types::{Inode, NodeId, Position};
use rayon::prelude::*;
use std::fmt;
use std::path::PathBuf;
use std::time::Instant;

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
/// `FileRecordInput` is `Send` because it contains only owned data:
/// - `path`: Owned `String`
/// - `full_path`: Owned `PathBuf`
/// - `old_content`: Owned `Vec<u8>` (retrieved from pristine in pre-pass)
/// - `inode`/`position`: `Copy` types
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

    /// New content provided directly by the caller.
    ///
    /// When `Some`, the worker uses this instead of reading from
    /// `full_path`.  This is the fast path for git import where
    /// the content is already in memory from `git show`.
    pub new_content: Option<Vec<u8>>,

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
            new_content: None,
            inode: None,
            position: None,
        }
    }

    /// Create an input for a newly added file with content already in memory.
    pub fn added_with_content(path: String, content: Vec<u8>) -> Self {
        Self {
            path,
            full_path: PathBuf::new(),
            kind: FileRecordKind::Added,
            old_content: Vec::new(),
            new_content: Some(content),
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
            new_content: None,
            inode: Some(inode),
            position: Some(position),
        }
    }

    /// Create an input for a modified file with new content already in memory.
    pub fn modified_with_content(
        path: String,
        old_content: Vec<u8>,
        new_content: Vec<u8>,
        inode: Inode,
        position: Position<NodeId>,
    ) -> Self {
        Self {
            path,
            full_path: PathBuf::new(),
            kind: FileRecordKind::Modified,
            old_content,
            new_content: Some(new_content),
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
            new_content: None,
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
            new_content: None,
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
            new_content: None,
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
    pub recorded: Option<RecordedFile>,

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
    fn skipped(path: String, kind: FileRecordKind) -> Self {
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
/// # Per-File Processing
///
/// For each file, the processing includes:
/// 1. **Read content** from disk (for added/modified files)
/// 2. **Detect encoding** (UTF-8, binary, etc.)
/// 3. **Diff old vs new** (for modified files — Myers or Patience algorithm)
/// 4. **Tokenize content** and build CRDT ops (Trunk → Branch → Leaf)
/// 5. **Create hunks** (graph operations for the change)
///
/// Steps 3-4 are the most expensive and benefit most from parallelism.
///
/// # Automatic Sequential Fallback
///
/// If `options.should_parallelize(inputs.len())` returns `false` (e.g., fewer
/// than 4 files), processing falls back to sequential iteration to avoid
/// rayon overhead.
///
/// # Arguments
///
/// * `inputs` - Per-file input descriptors from the pre-pass.
/// * `options` - Parallel recording configuration.
///
/// # Returns
///
/// A tuple of `(results, stats)` where:
/// - `results` is a `Vec` of per-file outcomes (one per input, in order)
/// - `stats` is the aggregate processing statistics
///
/// # Examples
///
/// ```rust,ignore
/// let inputs = vec![
///     FileRecordInput::added("src/main.rs".into(), "/repo/src/main.rs".into()),
///     FileRecordInput::added("src/lib.rs".into(), "/repo/src/lib.rs".into()),
/// ];
///
/// let (results, stats) = parallel_record_files(&inputs, &ParallelRecordOptions::default());
/// println!("Recorded {} files in {}ms", stats.files_recorded, stats.wall_time_ms);
/// ```
pub fn parallel_record_files(
    inputs: &[FileRecordInput],
    options: &ParallelRecordOptions,
) -> (Vec<Result<FileRecordOutput, String>>, ParallelRecordStats) {
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

// ═══════════════════════════════════════════════════════════════════════
// process_single_file — per-file processing (runs on rayon thread)
// ═══════════════════════════════════════════════════════════════════════

/// Process a single file on a rayon worker thread.
///
/// This function is the per-file workhorse. It reads the file from disk,
/// diffs against old content, tokenizes, and builds hunks. It is designed
/// to be called from a rayon parallel iterator and must not access any
/// shared mutable state.
fn process_single_file(
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
    // Use in-memory content if provided, otherwise read from disk.
    let content = if let Some(ref c) = input.new_content {
        c.clone()
    } else {
        std::fs::read(&input.full_path)
            .map_err(|e| format!("Failed to read {}: {}", input.path, e))?
    };

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
    // Use in-memory content if provided, otherwise read from disk.
    let new_content = if let Some(ref c) = input.new_content {
        c.clone()
    } else {
        std::fs::read(&input.full_path)
            .map_err(|e| format!("Failed to read {}: {}", input.path, e))?
    };

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

    match record_modified_file(&memory_wc, &detected, &input.old_content, options) {
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

// ═══════════════════════════════════════════════════════════════════════
// Merge helpers — for Phase 3 integration
// ═══════════════════════════════════════════════════════════════════════

/// Merge parallel results into the aggregate vectors needed by `Repository::record()`.
///
/// This extracts the `RecordedFile` values from the parallel results and
/// separates them into recorded/skipped/error categories, matching the
/// existing record flow's data structures.
///
/// # Arguments
///
/// * `results` - The per-file results from [`parallel_record_files`].
///
/// # Returns
///
/// A tuple of `(recorded_files, recorded_paths, skipped_paths, deleted_paths, errors, stats)`.
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

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

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
        let pos = Position::new(NodeId::new(1), atomic_core::types::ChangePosition::new(0));
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
        let pos = Position::new(NodeId::new(2), atomic_core::types::ChangePosition::new(0));
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
        let pos = Position::new(NodeId::new(1), atomic_core::types::ChangePosition::new(0));
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
        let pos = Position::new(NodeId::new(1), atomic_core::types::ChangePosition::new(0));
        let input =
            FileRecordInput::modified("lib.rs".into(), "/repo/lib.rs".into(), vec![], inode, pos);
        let display = format!("{}", input);
        assert!(display.contains("[mod]"));
    }

    #[test]
    fn test_input_display_deleted() {
        let inode = Inode::new(1);
        let pos = Position::new(NodeId::new(1), atomic_core::types::ChangePosition::new(0));
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
        let results: Vec<Result<FileRecordOutput, String>> =
            vec![Err("something went wrong".into())];

        let merged = merge_parallel_results(results);
        assert!(merged.recorded_files.is_empty());
        assert_eq!(merged.errors.len(), 1);
    }

    #[test]
    fn test_merge_with_directory() {
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
}

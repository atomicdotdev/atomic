//! Recording functions to build changes from detected files.
//!
//! This module provides the functionality to convert detected file changes
//! into recorded hunks that can be assembled into a [`Change`]. It bridges
//! the gap between detection (what changed) and the final change object
//! (the serializable representation).
//!
//! # CRDT Integration
//!
//! **By default**, all recording functions automatically generate CRDT operations
//! (Trunk → Branch → Leaf) alongside the traditional hunks. This enables:
//!
//! - **Token-level diff**: Fine-grained change tracking at the token level
//! - **Conflict-free merging**: CRDT operations can be applied in any order
//! - **Accurate blame**: Token-level attribution for code changes
//!
//! Access CRDT data via [`RecordedFile::crdt_ops()`] and [`RecordedFile::crdt_stats()`].
//!
//! # Overview
//!
//! Recording is the process of converting detected changes into hunks:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        Recording Pipeline                                │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Input: Detection Results                                               │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ DetectedFile { kind: Added, path: "new.rs", ... }                │  │
//! │  │ DetectedFile { kind: Modified, path: "lib.rs", diff_ops: [...] } │  │
//! │  │ DetectedFile { kind: Deleted, path: "old.rs", ... }              │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  Recording Process (Parallel Outputs)                                   │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ 1. For each detected file:                                       │  │
//! │  │    • Read working copy content (for adds/modifies)               │  │
//! │  │    • Retrieve pristine content (for modifies/deletes)            │  │
//! │  │    • Build traditional hunks (BuiltHunk)                         │  │
//! │  │    • Build CRDT operations (FileOps with TrunkOp/BranchOp/LeafOp)│  │
//! │  │ 2. Track content offsets in the change's content buffer          │  │
//! │  │ 3. Track inode updates for local application                     │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                              │                                          │
//! │                              ▼                                          │
//! │  Output: RecordingResult                                                │
//! │  ┌──────────────────────────────────────────────────────────────────┐  │
//! │  │ files: Vec<RecordedFile>                                         │  │
//! │  │   └─ hunks: Vec<BuiltHunk>       (traditional change hunks)      │  │
//! │  │   └─ crdt_ops: Option<FileOps>   (token-level CRDT operations)   │  │
//! │  │   └─ crdt_stats: CrdtBuildStats  (tokenization statistics)       │  │
//! │  │ stats: RecordingStats                                            │  │
//! │  └──────────────────────────────────────────────────────────────────┘  │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ## Basic Recording
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::record::{record_detected_files, RecordingOptions};
//!
//! // Assuming we have detection results
//! let detected = detect_changes_simple(&wc, &tracked, &options);
//!
//! // Record all detected changes
//! let recording_options = RecordingOptions::new();
//! let result = record_detected_files(
//!     &working_copy,
//!     &changes,
//!     detected.changed_files(),
//!     &recording_options,
//! )?;
//!
//! println!("Recorded {} hunks", result.hunk_count());
//! println!("Content size: {} bytes", result.content_len());
//! ```
//!
//! ## Recording with Filtering
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::record::{record_file, RecordingOptions};
//!
//! // Record only specific files
//! let options = RecordingOptions::new().max_file_size(1024 * 1024);
//!
//! for file in detected.modified() {
//!     if should_record(&file) {
//!         let result = record_file(&working_copy, &changes, &file, &options)?;
//!         // Process result...
//!     }
//! }
//! ```
//!
//! # Recording Different Change Types
//!
//! ## Added Files
//!
//! For newly added files, we:
//! 1. Read the entire content from the working copy
//! 2. Detect the encoding (text vs binary)
//! 3. Create a `FileAdd` graph_op with the content
//!
//! ## Deleted Files
//!
//! For deleted files, we:
//! 1. Look up the file's position in the pristine graph
//! 2. Create a `FileDel` graph_op marking the edges as deleted
//!
//! ## Modified Files
//!
//! For modified files, we:
//! 1. Retrieve the pristine content
//! 2. Read the working copy content
//! 3. Generate diff operations
//! 4. Convert diffs to `Edit` or `Replacement` hunks
//!
//! ## Moved Files
//!
//! For moved/renamed files, we:
//! 1. Create a `FileMove` graph_op with old and new paths
//! 2. Handle any content changes as modifications
//!
//! # Performance Considerations
//!
//! - Content is read lazily when needed
//! - Large files can be skipped or handled specially
//! - The content buffer grows as needed (consider pre-allocation for large changes)
//!
//! # Error Handling
//!
//! Recording can fail for several reasons:
//!
//! - IO errors when reading files
//! - Encoding errors for binary files treated as text
//! - Missing files in the working copy
//!
//! Errors are collected in the result rather than failing immediately,
//! allowing partial recording to succeed.
//!
//! [`Change`]: crate::change::Change

use crate::change::{Encoding, FileOps, Local};

/// A single line from a git diff, captured during Phase 1 of git import.
///
/// This is a plain data carrier, independent of any git library.
/// Origin values:
///   `+`  — line was added in the new file
///   `-`  — line was deleted from the old file
///   ` `  — context line (unchanged)
///
/// Used by [`build_crdt_ops_from_git_diff`] to build BranchOps that
/// exactly match `git diff` output.
#[derive(Debug, Clone)]
pub struct GitDiffLine {
    /// `+`, `-`, or ` `
    pub origin: char,
    /// Raw line bytes (may include trailing `\n`)
    pub content: Vec<u8>,
    /// 1-based line number in the old file (`None` for added lines)
    pub old_lineno: Option<u32>,
    /// 1-based line number in the new file (`None` for deleted lines)
    pub new_lineno: Option<u32>,
}
use crate::crdt::{BranchId, TrunkId};
use crate::diff::Algorithm;
use crate::output::WorkingCopyRead;
use crate::types::{Inode, NodeId, Position};

use super::compare::{compare_content, detect_encoding};
use super::crdt::{ContentTokenizer, CrdtBuildStats, CrdtChangeBuilder};
use super::detect::{DetectedFile, DetectionKind};
use super::graph_op::{BuiltHunk, BuiltHunkKind, HunkBuildOptions, HunkBuilder};

// ============================================================================
// RECORDING OPTIONS
// ============================================================================

/// Options controlling the recording process.
///
/// These options allow fine-tuning how detected changes are converted
/// into recorded hunks.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::record::RecordingOptions;
/// use atomic_core::diff::Algorithm;
/// use atomic_core::change::Encoding;
///
/// let options = RecordingOptions::new()
///     .algorithm(Algorithm::Patience)
///     .default_encoding(Encoding::Utf8)
///     .max_file_size(10 * 1024 * 1024)
///     .skip_binary(false);
///
/// assert_eq!(options.get_algorithm(), Algorithm::Patience);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingOptions {
    /// Diff algorithm to use for content comparison.
    algorithm: Algorithm,

    /// Default encoding for new files.
    ///
    /// This is used when encoding cannot be auto-detected.
    default_encoding: Option<Encoding>,

    /// Maximum file size to record (in bytes).
    ///
    /// Files larger than this are skipped or marked as binary.
    max_file_size: Option<usize>,

    /// Whether to skip binary files entirely.
    ///
    /// When true, binary files are not recorded. When false, they
    /// are recorded but without diff information.
    skip_binary: bool,

    /// Whether to record empty files.
    record_empty_files: bool,

    /// Number of context lines for hunks.
    context_lines: usize,
}

impl RecordingOptions {
    /// Default maximum file size (50 MB).
    pub const DEFAULT_MAX_FILE_SIZE: usize = 50 * 1024 * 1024;

    /// Default number of context lines.
    pub const DEFAULT_CONTEXT_LINES: usize = 3;

    /// Create new options with defaults.
    ///
    /// Default values:
    /// - `algorithm`: Myers
    /// - `default_encoding`: None (auto-detect)
    /// - `max_file_size`: 50 MB
    /// - `skip_binary`: false
    /// - `record_empty_files`: true
    /// - `context_lines`: 3
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new();
    /// assert!(!options.get_skip_binary());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the diff algorithm.
    ///
    /// # Arguments
    ///
    /// * `algorithm` - The algorithm to use for diffing
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    /// use atomic_core::diff::Algorithm;
    ///
    /// let options = RecordingOptions::new().algorithm(Algorithm::Patience);
    /// assert_eq!(options.get_algorithm(), Algorithm::Patience);
    /// ```
    #[must_use]
    pub fn algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Set the default encoding.
    ///
    /// # Arguments
    ///
    /// * `encoding` - The default encoding for new files
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    /// use atomic_core::change::Encoding;
    ///
    /// let options = RecordingOptions::new().default_encoding(Encoding::Utf8);
    /// assert_eq!(options.get_default_encoding(), Some(Encoding::Utf8));
    /// ```
    #[must_use]
    pub fn default_encoding(mut self, encoding: Encoding) -> Self {
        self.default_encoding = Some(encoding);
        self
    }

    /// Set the maximum file size.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum size in bytes
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new().max_file_size(1024 * 1024);
    /// assert_eq!(options.get_max_file_size(), Some(1024 * 1024));
    /// ```
    #[must_use]
    pub fn max_file_size(mut self, size: usize) -> Self {
        self.max_file_size = Some(size);
        self
    }

    /// Set whether to skip binary files.
    ///
    /// # Arguments
    ///
    /// * `skip` - Whether to skip binary files
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new().skip_binary(true);
    /// assert!(options.get_skip_binary());
    /// ```
    #[must_use]
    pub fn skip_binary(mut self, skip: bool) -> Self {
        self.skip_binary = skip;
        self
    }

    /// Set whether to record empty files.
    ///
    /// # Arguments
    ///
    /// * `record` - Whether to record empty files
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new().record_empty_files(false);
    /// assert!(!options.get_record_empty_files());
    /// ```
    #[must_use]
    pub fn record_empty_files(mut self, record: bool) -> Self {
        self.record_empty_files = record;
        self
    }

    /// Set the number of context lines.
    ///
    /// # Arguments
    ///
    /// * `lines` - Number of context lines
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingOptions;
    ///
    /// let options = RecordingOptions::new().context_lines(5);
    /// assert_eq!(options.get_context_lines(), 5);
    /// ```
    #[must_use]
    pub fn context_lines(mut self, lines: usize) -> Self {
        self.context_lines = lines;
        self
    }

    /// Get the algorithm setting.
    #[must_use]
    pub fn get_algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Get the default encoding setting.
    #[must_use]
    pub fn get_default_encoding(&self) -> Option<Encoding> {
        self.default_encoding
    }

    /// Get the max file size setting.
    #[must_use]
    pub fn get_max_file_size(&self) -> Option<usize> {
        self.max_file_size
    }

    /// Get the skip binary setting.
    #[must_use]
    pub fn get_skip_binary(&self) -> bool {
        self.skip_binary
    }

    /// Get the record empty files setting.
    #[must_use]
    pub fn get_record_empty_files(&self) -> bool {
        self.record_empty_files
    }

    /// Get the context lines setting.
    #[must_use]
    pub fn get_context_lines(&self) -> usize {
        self.context_lines
    }

    /// Check if a file size exceeds the maximum.
    #[must_use]
    pub fn exceeds_max_size(&self, size: usize) -> bool {
        self.max_file_size.is_some_and(|max| size > max)
    }

    /// Convert to graph_op build options.
    #[must_use]
    pub fn to_hunk_options(&self) -> HunkBuildOptions {
        let mut opts = HunkBuildOptions::new().context_lines(self.context_lines);
        if let Some(enc) = self.default_encoding {
            opts = opts.encoding(enc);
        }
        opts
    }
}

impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Myers,
            default_encoding: None,
            max_file_size: Some(Self::DEFAULT_MAX_FILE_SIZE),
            skip_binary: false,
            record_empty_files: true,
            context_lines: Self::DEFAULT_CONTEXT_LINES,
        }
    }
}

// ============================================================================
// RECORDING STATS
// ============================================================================

/// Statistics about the recording process.
///
/// Tracks various metrics during recording for reporting and diagnostics.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::record::RecordingStats;
///
/// let stats = RecordingStats::new();
/// assert_eq!(stats.files_recorded, 0);
/// assert_eq!(stats.hunks_created, 0);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordingStats {
    /// Number of files processed.
    pub files_recorded: usize,

    /// Number of hunks created.
    pub hunks_created: usize,

    /// Number of files skipped.
    pub files_skipped: usize,

    /// Number of binary files encountered.
    pub binary_files: usize,

    /// Number of files that exceeded size limits.
    pub oversized_files: usize,

    /// Total bytes of content recorded.
    pub bytes_recorded: usize,

    /// Number of lines added.
    pub lines_added: usize,

    /// Number of lines deleted.
    pub lines_deleted: usize,

    /// Number of errors encountered.
    pub errors: usize,
}

impl RecordingStats {
    /// Create new empty statistics.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingStats;
    ///
    /// let stats = RecordingStats::new();
    /// assert_eq!(stats.total_files(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get total files processed (recorded + skipped).
    #[must_use]
    pub fn total_files(&self) -> usize {
        self.files_recorded + self.files_skipped
    }

    /// Get total line changes (added + deleted).
    #[must_use]
    pub fn total_line_changes(&self) -> usize {
        self.lines_added + self.lines_deleted
    }

    /// Check if any errors occurred.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }

    /// Merge statistics from another recording.
    pub fn merge(&mut self, other: &RecordingStats) {
        self.files_recorded += other.files_recorded;
        self.hunks_created += other.hunks_created;
        self.files_skipped += other.files_skipped;
        self.binary_files += other.binary_files;
        self.oversized_files += other.oversized_files;
        self.bytes_recorded += other.bytes_recorded;
        self.lines_added += other.lines_added;
        self.lines_deleted += other.lines_deleted;
        self.errors += other.errors;
    }
}

// ============================================================================
// RECORDED FILE
// ============================================================================

/// Information about a recorded file.
///
/// Contains both traditional hunks and CRDT operations generated for a single file.
/// CRDT operations are automatically generated during recording to enable token-level
/// diff and conflict-free merging.
///
/// # CRDT Integration
///
/// Each recorded file includes:
/// - **Traditional hunks** ([`BuiltHunk`]): For compatibility with the existing change format
/// - **CRDT operations** ([`FileOps`]): Token-level operations (Trunk → Branch → Leaf)
/// - **CRDT statistics** ([`CrdtBuildStats`]): Metrics about tokenization
///
/// Access CRDT data via [`crdt_ops()`](Self::crdt_ops) and [`crdt_stats()`](Self::crdt_stats).
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::record::RecordedFile;
///
/// let recorded = RecordedFile::new("src/main.rs");
/// assert_eq!(recorded.path(), "src/main.rs");
/// assert!(recorded.is_empty());
/// assert!(!recorded.has_crdt_ops()); // CRDT ops are added during recording
/// ```
#[derive(Debug, Clone)]
pub struct RecordedFile {
    /// Path of the file.
    path: String,

    /// Previous path (for moves/renames).
    old_path: Option<String>,

    /// Hunks generated for this file.
    hunks: Vec<BuiltHunk>,

    /// Content data (for new files or modifications).
    content: Vec<u8>,

    /// Detected encoding.
    encoding: Option<Encoding>,

    /// The kind of change (from detection).
    kind: Option<DetectionKind>,

    /// Position in the graph (for tracked files).
    position: Option<Position<NodeId>>,

    /// Inode (for tracked files).
    inode: Option<Inode>,

    /// Number of lines in the old (pristine) content.
    /// Used during globalization to determine if an insertion is
    /// at the end (append) or in the middle (requires Replace).
    old_line_count: Option<usize>,

    /// CRDT operations for this file (Trunk → Branch → Leaf model).
    ///
    /// This enables token-level diff and conflict-free merging.
    /// Generated automatically during recording.
    crdt_ops: Option<FileOps>,

    /// CRDT build statistics for this file.
    crdt_stats: Option<CrdtBuildStats>,
}

impl RecordedFile {
    /// Create a new recorded file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path of the file
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordedFile;
    ///
    /// let recorded = RecordedFile::new("test.rs");
    /// assert_eq!(recorded.path(), "test.rs");
    /// ```
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            old_path: None,
            hunks: Vec::new(),
            content: Vec::new(),
            encoding: None,
            kind: None,
            position: None,
            inode: None,
            old_line_count: None,
            crdt_ops: None,
            crdt_stats: None,
        }
    }

    /// Set the old (pristine) line count.
    pub fn set_old_line_count(&mut self, count: usize) {
        self.old_line_count = Some(count);
    }

    /// Get the old (pristine) line count.
    pub fn old_line_count(&self) -> Option<usize> {
        self.old_line_count
    }

    /// Create a new recorded directory (for DirAdd graph_op).
    ///
    /// This is used when explicitly tracking an empty directory.
    /// The graph_op will be a `GraphOp::DirAdd` with no content.
    ///
    /// # Arguments
    ///
    /// * `path` - Path of the directory
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordedFile;
    ///
    /// let recorded = RecordedFile::new_directory("src/empty_module");
    /// assert_eq!(recorded.path(), "src/empty_module");
    /// assert!(recorded.is_directory());
    /// ```
    #[must_use]
    pub fn new_directory(path: impl Into<String>) -> Self {
        let mut recorded = Self::new(path);
        recorded.kind = Some(DetectionKind::Added);
        // Mark as directory by setting encoding to None and content to empty
        // The globalization phase will recognize this and create a DirAdd graph_op
        recorded.encoding = None;
        recorded
    }

    /// Create a recorded file entry for a deleted directory (for DirDel graph_op).
    ///
    /// This is used when a tracked directory has been deleted from disk.
    ///
    /// # Arguments
    ///
    /// * `path` - Path of the deleted directory
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordedFile;
    ///
    /// let recorded = RecordedFile::new_deleted_directory("src/old_module");
    /// assert_eq!(recorded.path(), "src/old_module");
    /// assert!(recorded.is_deleted_directory());
    /// ```
    #[must_use]
    pub fn new_deleted_directory(path: impl Into<String>) -> Self {
        let mut recorded = Self::new(path);
        recorded.kind = Some(DetectionKind::Deleted);
        recorded.encoding = None;
        recorded
    }

    /// Check if this recorded file represents a directory.
    ///
    /// A directory is identified by having `Added` kind with no content.
    #[must_use]
    pub fn is_directory(&self) -> bool {
        matches!(self.kind, Some(DetectionKind::Added))
            && self.content.is_empty()
            && self.hunks.is_empty()
    }

    /// Check if this recorded file represents a deleted directory.
    #[must_use]
    pub fn is_deleted_directory(&self) -> bool {
        matches!(self.kind, Some(DetectionKind::Deleted))
            && self.content.is_empty()
            && self.hunks.is_empty()
    }

    /// Add a graph_op to this file.
    pub fn add_hunk(&mut self, graph_op: BuiltHunk) {
        self.hunks.push(graph_op);
    }

    /// Set the content.
    pub fn set_content(&mut self, content: Vec<u8>) {
        self.content = content;
    }

    /// Set the encoding.
    pub fn set_encoding(&mut self, encoding: Encoding) {
        self.encoding = Some(encoding);
    }

    /// Set the change kind.
    pub fn set_kind(&mut self, kind: DetectionKind) {
        self.kind = Some(kind);
    }

    /// Set the position.
    pub fn set_position(&mut self, position: Position<NodeId>) {
        self.position = Some(position);
    }

    /// Set the inode.
    pub fn set_inode(&mut self, inode: Inode) {
        self.inode = Some(inode);
    }

    /// Set the old path (for moves/renames).
    pub fn set_old_path(&mut self, path: String) {
        self.old_path = Some(path);
    }

    /// Set the CRDT operations for this file.
    pub fn set_crdt_ops(&mut self, ops: FileOps) {
        self.crdt_ops = Some(ops);
    }

    /// Set the CRDT build statistics.
    pub fn set_crdt_stats(&mut self, stats: CrdtBuildStats) {
        self.crdt_stats = Some(stats);
    }

    /// Get the path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get the old path (for moves/renames).
    #[must_use]
    pub fn old_path(&self) -> Option<&str> {
        self.old_path.as_deref()
    }

    /// Get the hunks.
    #[must_use]
    pub fn hunks(&self) -> &[BuiltHunk] {
        &self.hunks
    }

    /// Get the content.
    #[must_use]
    pub fn content(&self) -> &[u8] {
        &self.content
    }

    /// Get the encoding.
    #[must_use]
    pub fn encoding(&self) -> Option<Encoding> {
        self.encoding
    }

    /// Get the change kind.
    #[must_use]
    pub fn kind(&self) -> Option<DetectionKind> {
        self.kind
    }

    /// Get the position.
    #[must_use]
    pub fn position(&self) -> Option<Position<NodeId>> {
        self.position
    }

    /// Get the inode.
    #[must_use]
    pub fn inode(&self) -> Option<Inode> {
        self.inode
    }

    /// Get the CRDT operations for this file.
    #[must_use]
    pub fn crdt_ops(&self) -> Option<&FileOps> {
        self.crdt_ops.as_ref()
    }

    /// Get the CRDT build statistics.
    #[must_use]
    pub fn crdt_stats(&self) -> Option<&CrdtBuildStats> {
        self.crdt_stats.as_ref()
    }

    /// Check if this file has CRDT operations.
    #[must_use]
    pub fn has_crdt_ops(&self) -> bool {
        self.crdt_ops.is_some()
    }

    /// Take ownership of the CRDT operations.
    #[must_use]
    pub fn into_crdt_ops(self) -> Option<FileOps> {
        self.crdt_ops
    }

    /// Check if empty (no hunks).
    ///
    /// Note: Moved files are never considered empty even if they have no hunks,
    /// because the move itself is a meaningful operation that must be recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        if matches!(self.kind, Some(DetectionKind::Moved)) {
            return false;
        }
        self.hunks.is_empty()
    }

    /// Get number of hunks.
    #[must_use]
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Get content length.
    #[must_use]
    pub fn content_len(&self) -> usize {
        self.content.len()
    }

    /// Take ownership of the hunks.
    #[must_use]
    pub fn into_hunks(self) -> Vec<BuiltHunk> {
        self.hunks
    }

    /// Take ownership of the content.
    #[must_use]
    pub fn into_content(self) -> Vec<u8> {
        self.content
    }
}

// ============================================================================
// RECORDING RESULT
// ============================================================================

/// The complete result of recording detected changes.
///
/// Contains all recorded files, accumulated content, and statistics.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::record::RecordingResult;
///
/// let result = RecordingResult::new();
/// assert!(result.is_empty());
/// assert_eq!(result.file_count(), 0);
/// ```
#[derive(Debug, Clone, Default)]
pub struct RecordingResult {
    /// Recorded files.
    files: Vec<RecordedFile>,

    /// Errors encountered during recording.
    errors: Vec<String>,

    /// Recording statistics.
    stats: RecordingStats,
}

impl RecordingResult {
    /// Create a new empty result.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::record::RecordingResult;
    ///
    /// let result = RecordingResult::new();
    /// assert!(result.is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a recorded file.
    pub fn add_file(&mut self, file: RecordedFile) {
        self.stats.files_recorded += 1;
        self.stats.hunks_created += file.hunk_count();
        self.stats.bytes_recorded += file.content_len();
        self.files.push(file);
    }

    /// Add an error.
    pub fn add_error(&mut self, error: impl Into<String>) {
        self.stats.errors += 1;
        self.errors.push(error.into());
    }

    /// Record that a file was skipped.
    pub fn record_skipped(&mut self) {
        self.stats.files_skipped += 1;
    }

    /// Record that a binary file was encountered.
    pub fn record_binary(&mut self) {
        self.stats.binary_files += 1;
    }

    /// Record that a file exceeded size limits.
    pub fn record_oversized(&mut self) {
        self.stats.oversized_files += 1;
    }

    /// Record line changes.
    pub fn record_line_changes(&mut self, added: usize, deleted: usize) {
        self.stats.lines_added += added;
        self.stats.lines_deleted += deleted;
    }

    /// Check if empty (no files recorded).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Check if there were errors.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Get recorded files.
    #[must_use]
    pub fn files(&self) -> &[RecordedFile] {
        &self.files
    }

    /// Get errors.
    #[must_use]
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Get statistics.
    #[must_use]
    pub fn stats(&self) -> &RecordingStats {
        &self.stats
    }

    /// Get number of files.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Get total graph_op count.
    #[must_use]
    pub fn hunk_count(&self) -> usize {
        self.stats.hunks_created
    }

    /// Get total content length.
    #[must_use]
    pub fn content_len(&self) -> usize {
        self.stats.bytes_recorded
    }

    /// Iterate over recorded files.
    pub fn iter(&self) -> impl Iterator<Item = &RecordedFile> {
        self.files.iter()
    }

    /// Take ownership of files.
    #[must_use]
    pub fn into_files(self) -> Vec<RecordedFile> {
        self.files
    }

    /// Merge another result into this one.
    pub fn merge(&mut self, other: RecordingResult) {
        self.files.extend(other.files);
        self.errors.extend(other.errors);
        self.stats.merge(&other.stats);
    }
}

impl IntoIterator for RecordingResult {
    type Item = RecordedFile;
    type IntoIter = std::vec::IntoIter<RecordedFile>;

    fn into_iter(self) -> Self::IntoIter {
        self.files.into_iter()
    }
}

impl<'a> IntoIterator for &'a RecordingResult {
    type Item = &'a RecordedFile;
    type IntoIter = std::slice::Iter<'a, RecordedFile>;

    fn into_iter(self) -> Self::IntoIter {
        self.files.iter()
    }
}

// ============================================================================
// RECORDING FUNCTIONS
// ============================================================================

/// Record a single added file from the working copy.
///
/// Reads the file content and creates appropriate hunks for a new file.
///
/// # Arguments
///
/// * `working_copy` - Working copy interface
/// * `detected` - The detected file
/// * `options` - Recording options
///
/// # Returns
///
/// A `RecordedFile` with the file's hunks and content.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::record::{record_added_file, RecordingOptions};
///
/// let options = RecordingOptions::new();
/// let recorded = record_added_file(&working_copy, &detected_file, &options)?;
/// ```
pub fn record_added_file<W>(
    working_copy: &W,
    detected: &DetectedFile,
    options: &RecordingOptions,
) -> Result<RecordedFile, String>
where
    W: WorkingCopyRead,
{
    let mut recorded = RecordedFile::new(&detected.path);
    recorded.set_kind(DetectionKind::Added);

    // Read content from working copy
    let mut content = Vec::new();
    if let Err(e) = working_copy.read_file(&detected.path, &mut content) {
        return Err(format!("Failed to read file {}: {}", detected.path, e));
    }

    // Check size limits
    if options.exceeds_max_size(content.len()) {
        return Err(format!(
            "File {} exceeds maximum size ({} bytes)",
            detected.path,
            content.len()
        ));
    }

    // Skip empty files if configured
    if content.is_empty() && !options.get_record_empty_files() {
        return Err(format!("Skipping empty file {}", detected.path));
    }

    // Detect encoding
    let encoding = detect_encoding(&content);
    recorded.set_encoding(encoding);

    // Skip binary files if configured
    if encoding == Encoding::Binary && options.get_skip_binary() {
        return Err(format!("Skipping binary file {}", detected.path));
    }

    // Create an edit graph_op for the new content
    let local = Local::new(&detected.path, 1);
    let content_len = content.len() as u64;
    let graph_op = BuiltHunk::new_edit(local, Some(encoding), 0, content_len);
    recorded.add_hunk(graph_op);

    // Generate CRDT operations for token-level tracking
    // Use NodeId(0) as placeholder - will be resolved during change finalization
    let crdt_ops = build_crdt_ops_for_added_file(&detected.path, &content, encoding);
    recorded.set_crdt_ops(crdt_ops.0);
    recorded.set_crdt_stats(crdt_ops.1);

    // Store the content
    recorded.set_content(content);

    Ok(recorded)
}

/// Build CRDT operations for a newly added file.
///
/// This tokenizes the content into lines and tokens, creating the full
/// Trunk → Branch → Leaf hierarchy for conflict-free merging.
fn build_crdt_ops_for_added_file(
    path: &str,
    content: &[u8],
    encoding: Encoding,
) -> (FileOps, CrdtBuildStats) {
    use super::crdt::FileOps as BuilderFileOps;

    // Use placeholder change ID - will be resolved during globalization
    let placeholder_change_id = NodeId::new(0);
    let mut builder = CrdtChangeBuilder::new(placeholder_change_id);

    // Add the file with content - this tokenizes into lines and tokens
    let enc = if encoding == Encoding::Binary {
        None
    } else {
        Some(encoding)
    };
    let _trunk_id = builder.add_file_with_content(path, content, enc);

    // Finish and extract the file ops
    let result = builder.finish();
    let stats = result.stats().clone();

    // Extract the FileOps for this file (should be exactly one)
    let (file_ops, _, _) = result.into_parts();
    let builder_file_op = file_ops.into_iter().next().unwrap_or_else(|| {
        BuilderFileOps::new(
            TrunkId::new(placeholder_change_id, 0),
            path.to_string(),
            None,
        )
    });

    // Convert to the canonical change::FileOps type
    (builder_file_op.into_change_ops(), stats)
}

/// Record a single deleted file.
///
/// Creates a deletion graph_op for a file that no longer exists in the working copy.
///
/// # Arguments
///
/// * `detected` - The detected file
/// * `options` - Recording options
///
/// # Returns
///
/// A `RecordedFile` with deletion hunks.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::record::{record_deleted_file, RecordingOptions};
///
/// let options = RecordingOptions::new();
/// let recorded = record_deleted_file(&detected_file, &options)?;
/// ```
pub fn record_deleted_file(
    detected: &DetectedFile,
    _options: &RecordingOptions,
) -> Result<RecordedFile, String> {
    let mut recorded = RecordedFile::new(&detected.path);
    recorded.set_kind(DetectionKind::Deleted);

    // Copy inode and position if available
    if let Some(inode) = detected.inode {
        recorded.set_inode(inode);
    }
    if let Some(position) = detected.position {
        recorded.set_position(position);
    }

    // Create a deletion graph_op for the file.
    // The actual line information would come from the pristine graph traversal
    // when integrated with the full repository context. The deletion graph_op
    // marks the file's content edges as deleted in the graph.
    let local = Local::new(&detected.path, 1);
    let encoding = detected.encoding;
    let graph_op = BuiltHunk::new_delete(local, encoding, vec![0], 0);
    recorded.add_hunk(graph_op);

    // Generate CRDT operations for the deletion
    let crdt_ops = build_crdt_ops_for_deleted_file(&detected.path);
    recorded.set_crdt_ops(crdt_ops.0);
    recorded.set_crdt_stats(crdt_ops.1);

    Ok(recorded)
}

/// Build CRDT operations for a deleted file.
///
/// Creates a TrunkOp::Delete to mark the file as deleted in the CRDT graph.
fn build_crdt_ops_for_deleted_file(path: &str) -> (FileOps, CrdtBuildStats) {
    use super::crdt::FileOps as BuilderFileOps;

    // Use placeholder change ID
    let placeholder_change_id = NodeId::new(0);
    let mut builder = CrdtChangeBuilder::new(placeholder_change_id);

    // Create a trunk ID for the deletion
    // Note: In a full implementation, we'd look up the existing trunk ID
    let trunk_id = TrunkId::new(placeholder_change_id, 0);
    builder.delete_file(trunk_id);

    let result = builder.finish();
    let stats = result.stats().clone();
    let (file_ops, _, _) = result.into_parts();
    let builder_file_op = file_ops
        .into_iter()
        .next()
        .unwrap_or_else(|| BuilderFileOps::delete(trunk_id, path.to_string()));

    // Convert to the canonical change::FileOps type
    (builder_file_op.into_change_ops(), stats)
}

/// Record a single modified file.
///
/// Compares old and new content and creates appropriate edit/replacement hunks.
///
/// # Arguments
///
/// * `working_copy` - Working copy interface
/// * `detected` - The detected file (should have diff_ops populated)
/// * `old_content` - The pristine (old) content
/// * `options` - Recording options
///
/// # Returns
///
/// A `RecordedFile` with modification hunks.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::record::{record_modified_file, RecordingOptions};
///
/// let options = RecordingOptions::new();
/// let recorded = record_modified_file(&working_copy, &detected_file, &old_content, &options)?;
/// ```
#[allow(clippy::type_complexity)]
pub fn record_modified_file<W>(
    working_copy: &W,
    detected: &DetectedFile,
    old_content: &[u8],
    options: &RecordingOptions,
) -> Result<RecordedFile, String>
where
    W: WorkingCopyRead,
{
    let mut recorded = RecordedFile::new(&detected.path);
    recorded.set_kind(DetectionKind::Modified);

    // Count lines in old content for precise insertion point detection
    let old_line_count = old_content.iter().filter(|&&b| b == b'\n').count();
    recorded.set_old_line_count(old_line_count);

    // Copy inode and position if available
    if let Some(inode) = detected.inode {
        recorded.set_inode(inode);
    }
    if let Some(position) = detected.position {
        recorded.set_position(position);
    }

    // Read new content from working copy
    let mut new_content = Vec::new();
    if let Err(e) = working_copy.read_file(&detected.path, &mut new_content) {
        return Err(format!("Failed to read file {}: {}", detected.path, e));
    }

    // Check size limits
    if options.exceeds_max_size(new_content.len()) {
        return Err(format!(
            "File {} exceeds maximum size ({} bytes)",
            detected.path,
            new_content.len()
        ));
    }

    // Detect encoding
    let encoding = detect_encoding(&new_content);
    recorded.set_encoding(encoding);

    // Skip binary files if configured
    if encoding == Encoding::Binary && options.get_skip_binary() {
        return Err(format!("Skipping binary file {}", detected.path));
    }

    // Compare content and build hunks
    let comparison = compare_content(old_content, &new_content, options.get_algorithm());

    // Build hunks from diff ops using HunkBuilder
    let hunk_options = options.to_hunk_options().encoding(encoding);
    let mut builder = HunkBuilder::with_options(&detected.path, hunk_options);

    for op in &comparison.diff_ops {
        builder.process_diff_op(op);
    }

    let hunk_result = builder.finish();

    // Calculate line offsets in the new content for mapping line numbers to byte positions
    let new_line_offsets = calculate_line_offsets(&new_content);

    // Add all built hunks to the recorded file, updating content positions
    // ── Consolidate hunks into a single whole-file Replace ──
    //
    // In the globalize pipeline, every hunk kind that isn't a clean
    // Prepend or Append ends up doing the same thing: delete ALL content
    // vertices for the file, then insert full_content (the entire new
    // file). Specifically:
    //
    //   Replace  → always does delete-all + insert-full_content
    //   Delete   → always does delete-all (every content vertex)
    //   Insert with 0 < old_start < old_line_count
    //            → escalates to NeedsReplace → delete-all + insert-full_content
    //
    // When multiple hunks hit these paths, each one independently
    // inserts a full copy of the file, causing N× content duplication.
    //
    // Fix: detect when ANY combination of hunks would trigger whole-file
    // behavior, and collapse them all into a single Replace that covers
    // the entire file. This guarantees exactly one delete-all +
    // insert-full_content in the globalizer, regardless of diff shape.
    //
    // Insert hunks that are clean Prepend (old_start == 0) or Append
    // (old_start >= old_line_count) are safe — they only insert their
    // own content slice, not full_content. Those are kept as-is.
    //
    // The semantic layer (CRDT line_ops) is unaffected — it's built
    // separately from the raw diff and retains per-line granularity.
    let mut hunks: Vec<BuiltHunk> = hunk_result.into_hunks();

    // Count hunks that will trigger whole-file operations in globalize.
    //
    // The globalize layer has NO concept of partial deletion — both
    // `globalize_delete` and `globalize_replace` call `delete_all_content`
    // which marks EVERY content vertex as deleted.  So:
    //
    //   Replace  → delete-all + insert-full_content  (correct on its own)
    //   Delete   → delete-all, insert nothing         (DESTROYS the file)
    //   Insert with 0 < old_start < old_line_count
    //            → escalates to delete-all + insert-full_content
    //
    // A Delete hunk in a *modified* file means "some lines were removed"
    // — the file still has content.  But globalize_delete would nuke the
    // whole file.  Since `record_modified_file` is never called for truly
    // deleted files (those go through `record_deleted_file`), every
    // Delete hunk here MUST be promoted to a Replace so the surviving
    // content is re-inserted.
    //
    // We also consolidate when multiple nuclear hunks coexist, or when a
    // nuclear hunk coexists with other hunks, to prevent N× duplication.
    let has_nuclear_hunk = hunks.iter().any(|h| match h.kind {
        // Replace already does delete-all + insert-full; correct on its own
        // but must be consolidated if it coexists with other hunks.
        BuiltHunkKind::Replace => true,
        // Delete does delete-all with NO re-insert — would nuke the whole
        // file.  Since record_modified_file is never called for truly
        // deleted files, every Delete here is a partial line removal that
        // must become a Replace so the surviving content is re-inserted.
        BuiltHunkKind::Delete => true,
        // Middle inserts (0 < old_start < old_line_count) cannot be
        // handled by the globalizer — it only supports Prepend and Append.
        // These must be consolidated into a Replace.
        BuiltHunkKind::Insert => h.old_start != 0 && h.old_start < old_line_count,
    });

    if has_nuclear_hunk {
        // Multiple hunks where at least one is nuclear, OR a single nuclear
        // hunk coexisting with other hunks. Collapse everything into one
        // Replace to prevent duplication.
        //
        // Collect deleted lines from all hunks that delete old content.
        let mut all_deleted: Vec<usize> = Vec::new();
        for h in &hunks {
            all_deleted.extend_from_slice(&h.deleted_lines);
        }
        all_deleted.sort_unstable();
        all_deleted.dedup();

        let new_line_count = new_content.split(|&b| b == b'\n').count();
        let merged_replace = BuiltHunk::new_replace_with_lines(
            Local::new(&detected.path, 1),
            Some(encoding),
            all_deleted,
            0,              // old_start: beginning of old content
            0,              // new_start: beginning of new content
            new_line_count, // new_len: all lines in the new file
        );

        // Replace all hunks with the single merged Replace
        hunks.clear();
        hunks.push(merged_replace);
    }

    for mut graph_op in hunks {
        // For Insert and Replace hunks, calculate actual content byte positions
        // based on which lines of new content they represent
        if graph_op.kind == BuiltHunkKind::Insert || graph_op.kind == BuiltHunkKind::Replace {
            // Use the graph_op's new_start and new_len to find the byte range in new_content.
            // new_start is 0-indexed line number, new_len is number of lines.
            if graph_op.new_len > 0 {
                // Find the start byte position (beginning of new_start line)
                let start_byte = if graph_op.new_start < new_line_offsets.len() {
                    new_line_offsets[graph_op.new_start]
                } else {
                    new_content.len()
                };

                // Find the end byte position (end of last line in range)
                let end_line = graph_op.new_start + graph_op.new_len;
                let end_byte = if end_line < new_line_offsets.len() {
                    new_line_offsets[end_line]
                } else {
                    new_content.len()
                };

                graph_op.content_start = Some(start_byte as u64);
                graph_op.content_end = Some(end_byte as u64);
            } else {
                // No new content (shouldn't happen for Insert/Replace, but handle gracefully)
                graph_op.content_start = Some(0);
                graph_op.content_end = Some(0);
            }
        }
        recorded.add_hunk(graph_op);
    }

    // Generate CRDT operations for token-level diff tracking
    let crdt_ops = build_crdt_ops_for_modified_file(
        &detected.path,
        old_content,
        &new_content,
        encoding,
        options.get_algorithm(),
    );
    recorded.set_crdt_ops(crdt_ops.0);
    recorded.set_crdt_stats(crdt_ops.1);

    // Store the new content
    recorded.set_content(new_content);

    Ok(recorded)
}

/// Build CRDT operations for a modified file.
///
/// This performs token-level diff analysis to generate fine-grained
/// Branch and Leaf operations for conflict-free merging.
fn build_crdt_ops_for_modified_file(
    path: &str,
    old_content: &[u8],
    new_content: &[u8],
    _encoding: Encoding,
    algorithm: Algorithm,
) -> (FileOps, CrdtBuildStats) {
    use super::crdt::FileOps as BuilderFileOps;
    use super::crdt::LineOps as BuilderLineOps;
    use crate::crdt::LeafOp;

    // Use placeholder change ID
    let placeholder_change_id = NodeId::new(0);

    // Create file ops container (no TrunkOp for modification - file already exists)
    let trunk_id = TrunkId::new(placeholder_change_id, 0);
    let mut file_ops = BuilderFileOps::new(trunk_id, path.to_string(), None);

    let mut stats = CrdtBuildStats::new();
    let mut next_branch_idx: u32 = 0;
    let mut next_leaf_idx: u32 = 0;

    // Helper to allocate branch IDs
    let mut alloc_branch = || {
        let id = BranchId::new(placeholder_change_id, next_branch_idx);
        next_branch_idx += 1;
        id
    };

    // Helper to allocate leaf IDs
    let mut alloc_leaf = || {
        let id = crate::crdt::LeafId::new(placeholder_change_id, next_leaf_idx);
        next_leaf_idx += 1;
        id
    };

    // Tokenize old and new content into lines
    let old_tokenizer = ContentTokenizer::new(old_content);
    let new_tokenizer = ContentTokenizer::new(new_content);

    let old_lines: Vec<_> = old_tokenizer.lines().collect();
    let new_lines: Vec<_> = new_tokenizer.lines().collect();

    // Perform line-level diff
    let line_diff = compare_content(old_content, new_content, algorithm);

    let mut collected_line_ops: Vec<BuilderLineOps> = Vec::new();

    // Track which old lines have been processed
    let mut _old_line_idx = 0;
    let mut _new_line_idx = 0;
    let mut prev_branch: Option<BranchId> = None;

    for op in &line_diff.diff_ops {
        match op {
            crate::diff::DiffOp::Equal {
                old_pos,
                new_pos,
                len,
            } => {
                // Equal lines - no CRDT operations needed, but track position
                _old_line_idx = old_pos + len;
                _new_line_idx = new_pos + len;
                // Update prev_branch to reference the last equal line
                // (In a full implementation, we'd look up the existing branch ID)
            }
            crate::diff::DiffOp::Delete {
                old_pos,
                new_pos: _,
                len,
            } => {
                // Deleted lines - create BranchOp::Delete for each with original content
                for i in 0..*len {
                    let line_idx = old_pos + i;
                    let branch_id = alloc_branch();

                    // Capture the original line content for diff display
                    let content = if line_idx < old_lines.len() {
                        let line = &old_lines[line_idx];
                        let mut leaf_ops = Vec::new();
                        for token in line.tokens() {
                            leaf_ops.push(LeafOp::Insert {
                                after: None,
                                kind: token.kind(),
                                content: token.content().to_vec(),
                            });
                        }
                        leaf_ops
                    } else {
                        Vec::new()
                    };

                    let line_op =
                        BuilderLineOps::delete(branch_id, content).with_old_line_num(line_idx + 1);
                    collected_line_ops.push(line_op);
                    stats.lines_deleted += 1;
                }
                _old_line_idx = old_pos + len;
            }
            crate::diff::DiffOp::Insert {
                old_pos: _,
                new_pos,
                len,
            } => {
                // Inserted lines - create BranchOp::Insert with token-level LeafOps
                for i in 0..*len {
                    let line_idx = new_pos + i;
                    if line_idx < new_lines.len() {
                        let line = &new_lines[line_idx];
                        let branch_id = alloc_branch();

                        // Generate LeafOps for tokens in this line
                        let mut leaf_ops = Vec::new();
                        let mut prev_leaf: Option<crate::crdt::LeafId> = None;

                        for token in line.tokens() {
                            let leaf_id = alloc_leaf();
                            leaf_ops.push(LeafOp::Insert {
                                after: prev_leaf,
                                kind: token.kind(),
                                content: token.content().to_vec(),
                            });
                            stats.tokens_added += 1;
                            prev_leaf = Some(leaf_id);
                        }

                        let line_op = BuilderLineOps::insert(branch_id, prev_branch, leaf_ops)
                            .with_new_line_num(line_idx + 1);
                        collected_line_ops.push(line_op);
                        stats.lines_added += 1;
                        prev_branch = Some(branch_id);
                    }
                }
                _new_line_idx = new_pos + len;
            }
            crate::diff::DiffOp::Replace {
                old_pos,
                old_len,
                new_pos,
                new_len,
            } => {
                // ══════════════════════════════════════════════════════════
                // Replace → BranchOp::Modify (equal count) or Delete+Insert
                // ══════════════════════════════════════════════════════════
                //
                // When old_len == new_len (1:1 replacement), the diff algorithm
                // anchored each old line to exactly one new line.  We emit
                // BranchOp::Modify so the display layer can show adjacent -/+
                // pairs with word-level highlighting.
                //
                // When counts differ (pure insertions or deletions within the
                // block), we emit all old lines as Delete then all new lines as
                // Insert — matching git's unified diff format exactly.
                //
                // NOTE: For git-imported changes, the CRDT ops are overridden
                // in write_commit() with build_crdt_ops_from_git_diff(), so
                // what we emit here only matters for atomic record (non-import).

                if *old_len == *new_len {
                    // Equal counts: positional 1:1 Modify — always correct
                    // because the diff algorithm anchored these lines together.
                    let build_old_leaf_ops = |line_idx: usize| -> Vec<LeafOp> {
                        if line_idx < old_lines.len() {
                            old_lines[line_idx]
                                .tokens()
                                .iter()
                                .map(|t| LeafOp::Insert {
                                    after: None,
                                    kind: t.kind(),
                                    content: t.content().to_vec(),
                                })
                                .collect()
                        } else {
                            Vec::new()
                        }
                    };

                    for i in 0..*old_len {
                        let old_line_idx = old_pos + i;
                        let new_line_idx = new_pos + i;
                        let branch_id = alloc_branch();
                        let old_leaf_ops = build_old_leaf_ops(old_line_idx);

                        let mut prev_leaf: Option<crate::crdt::LeafId> = None;
                        let new_leaf_ops: Vec<LeafOp> = if new_line_idx < new_lines.len() {
                            new_lines[new_line_idx]
                                .tokens()
                                .iter()
                                .map(|t| {
                                    let leaf_id = alloc_leaf();
                                    let op = LeafOp::Insert {
                                        after: prev_leaf,
                                        kind: t.kind(),
                                        content: t.content().to_vec(),
                                    };
                                    stats.tokens_added += 1;
                                    prev_leaf = Some(leaf_id);
                                    op
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };

                        let line_op = BuilderLineOps::modify(branch_id, old_leaf_ops, new_leaf_ops)
                            .with_old_line_num(old_line_idx + 1)
                            .with_new_line_num(new_line_idx + 1);
                        collected_line_ops.push(line_op);
                        stats.lines_modified += 1;
                        prev_branch = Some(branch_id);
                    }
                } else {
                    // Unequal counts: all old lines as Delete, all new as Insert.

                    // Emit all old lines as Delete
                    for oi in 0..*old_len {
                        let old_line_idx = old_pos + oi;
                        let branch_id = alloc_branch();
                        let content = if old_line_idx < old_lines.len() {
                            old_lines[old_line_idx]
                                .tokens()
                                .iter()
                                .map(|t| LeafOp::Insert {
                                    after: None,
                                    kind: t.kind(),
                                    content: t.content().to_vec(),
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let line_op = BuilderLineOps::delete(branch_id, content)
                            .with_old_line_num(old_line_idx + 1);
                        collected_line_ops.push(line_op);
                        stats.lines_deleted += 1;
                    }

                    // Emit all new lines as Insert
                    for ni in 0..*new_len {
                        let new_line_idx = new_pos + ni;
                        if new_line_idx < new_lines.len() {
                            let branch_id = alloc_branch();
                            let mut prev_leaf: Option<crate::crdt::LeafId> = None;
                            let leaf_ops: Vec<LeafOp> = new_lines[new_line_idx]
                                .tokens()
                                .iter()
                                .map(|t| {
                                    let leaf_id = alloc_leaf();
                                    let op = LeafOp::Insert {
                                        after: prev_leaf,
                                        kind: t.kind(),
                                        content: t.content().to_vec(),
                                    };
                                    stats.tokens_added += 1;
                                    prev_leaf = Some(leaf_id);
                                    op
                                })
                                .collect();
                            let line_op = BuilderLineOps::insert(branch_id, prev_branch, leaf_ops)
                                .with_new_line_num(new_line_idx + 1);
                            collected_line_ops.push(line_op);
                            stats.lines_added += 1;
                            prev_branch = Some(branch_id);
                        }
                    }

                    // Unequal counts: use bigram similarity to pair as many
                    // old↔new lines as possible as BranchOp::Modify, then
                    // emit remaining old lines as Delete and new lines as Insert.
                    let bigrams = |s: &str| -> std::collections::HashSet<(u8, u8)> {
                        let bytes = s.trim().as_bytes();
                        let mut set = std::collections::HashSet::new();
                        if bytes.len() >= 2 {
                            for w in bytes.windows(2) {
                                set.insert((w[0], w[1]));
                            }
                        }
                        set
                    };

                    let old_texts: Vec<String> = (0..*old_len)
                        .map(|oi| {
                            let idx = old_pos + oi;
                            if idx < old_lines.len() {
                                String::from_utf8_lossy(old_lines[idx].content()).into_owned()
                            } else {
                                String::new()
                            }
                        })
                        .collect();
                    let new_texts: Vec<String> = (0..*new_len)
                        .map(|ni| {
                            let idx = new_pos + ni;
                            if idx < new_lines.len() {
                                String::from_utf8_lossy(new_lines[idx].content()).into_owned()
                            } else {
                                String::new()
                            }
                        })
                        .collect();

                    let mut scores: Vec<(usize, usize, f64)> = Vec::new();
                    for oi in 0..*old_len {
                        let old_bg = bigrams(old_texts[oi].trim());
                        if old_bg.is_empty() {
                            continue;
                        }
                        for ni in 0..*new_len {
                            let new_bg = bigrams(new_texts[ni].trim());
                            if new_bg.is_empty() {
                                continue;
                            }
                            let inter = old_bg.intersection(&new_bg).count();
                            let union = old_bg.union(&new_bg).count();
                            if union > 0 {
                                let score = inter as f64 / union as f64;
                                if score >= 0.3 {
                                    scores.push((oi, ni, score));
                                }
                            }
                        }
                    }
                    scores
                        .sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

                    let mut paired_old: Vec<Option<usize>> = vec![None; *old_len];
                    let mut paired_new: Vec<bool> = vec![false; *new_len];
                    for (oi, ni, _) in &scores {
                        if paired_old[*oi].is_none() && !paired_new[*ni] {
                            paired_old[*oi] = Some(*ni);
                            paired_new[*ni] = true;
                        }
                    }

                    // Unpaired old lines → Delete
                    for (oi, maybe_ni) in paired_old.iter().enumerate() {
                        if maybe_ni.is_some() {
                            continue;
                        }
                        let old_line_idx = old_pos + oi;
                        let branch_id = alloc_branch();
                        let content = if old_line_idx < old_lines.len() {
                            old_lines[old_line_idx]
                                .tokens()
                                .iter()
                                .map(|t| LeafOp::Insert {
                                    after: None,
                                    kind: t.kind(),
                                    content: t.content().to_vec(),
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let line_op = BuilderLineOps::delete(branch_id, content)
                            .with_old_line_num(old_line_idx + 1);
                        collected_line_ops.push(line_op);
                        stats.lines_deleted += 1;
                    }

                    // Walk new lines: paired → Modify, unpaired → Insert
                    for ni in 0..*new_len {
                        let new_line_idx = new_pos + ni;
                        // Find if any old line pairs with this new line
                        let paired_oi = paired_old.iter().position(|m| m == &Some(ni));

                        if let Some(oi) = paired_oi {
                            let old_line_idx = old_pos + oi;
                            let branch_id = alloc_branch();
                            let old_leaf_ops = if old_line_idx < old_lines.len() {
                                old_lines[old_line_idx]
                                    .tokens()
                                    .iter()
                                    .map(|t| LeafOp::Insert {
                                        after: None,
                                        kind: t.kind(),
                                        content: t.content().to_vec(),
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            };
                            let mut prev_leaf: Option<crate::crdt::LeafId> = None;
                            let new_leaf_ops: Vec<LeafOp> = if new_line_idx < new_lines.len() {
                                new_lines[new_line_idx]
                                    .tokens()
                                    .iter()
                                    .map(|t| {
                                        let leaf_id = alloc_leaf();
                                        let op = LeafOp::Insert {
                                            after: prev_leaf,
                                            kind: t.kind(),
                                            content: t.content().to_vec(),
                                        };
                                        stats.tokens_added += 1;
                                        prev_leaf = Some(leaf_id);
                                        op
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            };
                            let line_op =
                                BuilderLineOps::modify(branch_id, old_leaf_ops, new_leaf_ops)
                                    .with_old_line_num(old_line_idx + 1)
                                    .with_new_line_num(new_line_idx + 1);
                            collected_line_ops.push(line_op);
                            stats.lines_modified += 1;
                            prev_branch = Some(branch_id);
                        } else {
                            // Unpaired new line → Insert
                            if new_line_idx < new_lines.len() {
                                let branch_id = alloc_branch();
                                let mut prev_leaf: Option<crate::crdt::LeafId> = None;
                                let leaf_ops: Vec<LeafOp> = new_lines[new_line_idx]
                                    .tokens()
                                    .iter()
                                    .map(|t| {
                                        let leaf_id = alloc_leaf();
                                        let op = LeafOp::Insert {
                                            after: prev_leaf,
                                            kind: t.kind(),
                                            content: t.content().to_vec(),
                                        };
                                        stats.tokens_added += 1;
                                        prev_leaf = Some(leaf_id);
                                        op
                                    })
                                    .collect();
                                let line_op =
                                    BuilderLineOps::insert(branch_id, prev_branch, leaf_ops)
                                        .with_new_line_num(new_line_idx + 1);
                                collected_line_ops.push(line_op);
                                stats.lines_added += 1;
                                prev_branch = Some(branch_id);
                            }
                        }
                    }

                    _old_line_idx = old_pos + old_len;
                    _new_line_idx = new_pos + new_len;
                } // end unequal-count branch
            }
        }
    }

    // ── Cross-block Delete+Insert→Modify consolidation ───────────────────
    //
    // After the Replace blocks have handled within-block pairing, this pass
    // promotes any remaining standalone Delete+Insert pairs (from separate
    // DiffOp::Delete and DiffOp::Insert operations) into BranchOp::Modify
    // when the lines are similar (bigram Jaccard ≥ 0.3).
    //
    // NOTE: For git-imported changes, build_crdt_ops_from_git_diff() overrides
    // the entire CRDT output, so this pairing only affects `atomic record`.
    {
        use crate::crdt::BranchOp;

        let extract_text = |op: &BuilderLineOps| -> String {
            let leaves = match op.operation() {
                BranchOp::Delete { content, .. } | BranchOp::Insert { content, .. } => content,
                BranchOp::Modify { new_content, .. } => new_content,
                _ => return String::new(),
            };
            let mut text = String::new();
            for leaf in leaves.iter() {
                if let LeafOp::Insert { content: bytes, .. } = leaf {
                    if let Ok(s) = std::str::from_utf8(bytes) {
                        text.push_str(s);
                    }
                }
            }
            text
        };

        let bigrams2 = |s: &str| -> std::collections::HashSet<(u8, u8)> {
            let bytes = s.trim().as_bytes();
            let mut set = std::collections::HashSet::new();
            if bytes.len() >= 2 {
                for w in bytes.windows(2) {
                    set.insert((w[0], w[1]));
                }
            }
            set
        };

        let mut del_entries: Vec<(usize, String, std::collections::HashSet<(u8, u8)>)> = Vec::new();
        let mut ins_entries: Vec<(usize, String, std::collections::HashSet<(u8, u8)>)> = Vec::new();

        for (idx, op) in collected_line_ops.iter().enumerate() {
            if op.is_modify() {
                continue;
            }
            let text = extract_text(op);
            let trimmed = text.trim().to_string();
            if trimmed.len() < 2 {
                continue;
            }
            let bg = bigrams2(&trimmed);
            if bg.is_empty() {
                continue;
            }
            if op.is_delete() {
                del_entries.push((idx, trimmed, bg));
            } else if op.is_insert() {
                ins_entries.push((idx, trimmed, bg));
            }
        }

        let mut candidates: Vec<(usize, usize, f64)> = Vec::new();
        for (di, (_, _, del_bg)) in del_entries.iter().enumerate() {
            for (ii, (_, _, ins_bg)) in ins_entries.iter().enumerate() {
                let inter = del_bg.intersection(ins_bg).count();
                let union = del_bg.union(ins_bg).count();
                if union > 0 {
                    let score = inter as f64 / union as f64;
                    if score >= 0.3 {
                        candidates.push((di, ii, score));
                    }
                }
            }
        }
        candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        let mut matched_del: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut matched_ins: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut promote: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();

        for (di, ii, _) in &candidates {
            if matched_del.contains(di) || matched_ins.contains(ii) {
                continue;
            }
            matched_del.insert(*di);
            matched_ins.insert(*ii);
            let del_idx = del_entries[*di].0;
            let ins_idx = ins_entries[*ii].0;
            promote.insert(ins_idx, del_idx);
        }

        let del_indices_to_skip: std::collections::HashSet<usize> =
            promote.values().copied().collect();

        let placeholder_id = BranchId::new(crate::types::NodeId::new(0), 0);
        let make_placeholder = || {
            BuilderLineOps::new(
                placeholder_id,
                BranchOp::Restore {
                    branch: placeholder_id,
                },
            )
        };

        let mut slots: Vec<BuilderLineOps> = collected_line_ops
            .iter_mut()
            .map(|op| std::mem::replace(op, make_placeholder()))
            .collect();

        let mut consolidated: Vec<BuilderLineOps> = Vec::with_capacity(slots.len());

        for idx in 0..slots.len() {
            if del_indices_to_skip.contains(&idx) {
                continue;
            }
            if let Some(&del_idx) = promote.get(&idx) {
                let del_op = &slots[del_idx];
                let ins_op = &slots[idx];
                let old_line_num = del_op.old_line_num();
                let new_line_num = ins_op.new_line_num();
                let old_content = match del_op.operation() {
                    BranchOp::Delete { content, .. } => content.clone(),
                    _ => Vec::new(),
                };
                let new_content = match ins_op.operation() {
                    BranchOp::Insert { content, .. } => content.clone(),
                    _ => Vec::new(),
                };
                let branch_id = del_op.branch_id();
                let mut modify = BuilderLineOps::modify(branch_id, old_content, new_content);
                if let Some(v) = old_line_num {
                    modify = modify.with_old_line_num(v);
                }
                if let Some(v) = new_line_num {
                    modify = modify.with_new_line_num(v);
                }
                consolidated.push(modify);
            } else {
                let op = std::mem::replace(&mut slots[idx], make_placeholder());
                consolidated.push(op);
            }
        }

        collected_line_ops = consolidated;
    }

    // Add final line_ops to file_ops.
    for line_op in collected_line_ops {
        file_ops.add_line_op(line_op);
    }

    // Convert to the canonical change::FileOps type
    (file_ops.into_change_ops(), stats)
}

/// Calculate byte offsets for each line in content.
///
/// Returns a vector where index i contains the byte offset where line i starts.
/// Line 0 starts at offset 0.
/// Build CRDT FileOps directly from git diff lines.
///
/// This is the authoritative path for git import: instead of re-running
/// our Myers diff algorithm (which may produce different edit operations
/// than git), we translate git's own `+`/`-`/` ` line classifications
/// directly into BranchOps.
///
/// The result has exactly the same Insert/Delete operations that
/// `git diff` would show — so `atomic diff -c` output matches `git diff`
/// line-for-line.
///
/// # Arguments
///
/// * `path`       — file path (for the FileOps container)
/// * `diff_lines` — ordered slice of GitDiffLine from git2::Diff::foreach
///
/// # Returns
///
/// `(FileOps, CrdtBuildStats)` ready to be stored in a `RecordedFile`.
pub fn build_crdt_ops_from_git_diff(
    path: &str,
    diff_lines: &[crate::record::workflow::GitDiffLine],
) -> (FileOps, CrdtBuildStats) {
    use super::crdt::FileOps as BuilderFileOps;
    use super::crdt::LineOps as BuilderLineOps;
    use crate::crdt::LeafOp;

    let placeholder_change_id = NodeId::new(0);
    let trunk_id = TrunkId::new(placeholder_change_id, 0);
    let mut file_ops = BuilderFileOps::new(trunk_id, path.to_string(), None);

    let mut stats = CrdtBuildStats::new();
    let mut next_branch_idx: u32 = 0;
    let mut next_leaf_idx: u32 = 0;

    let mut alloc_branch = || {
        let id = BranchId::new(placeholder_change_id, next_branch_idx);
        next_branch_idx += 1;
        id
    };

    let mut alloc_leaf = || {
        let id = crate::crdt::LeafId::new(placeholder_change_id, next_leaf_idx);
        next_leaf_idx += 1;
        id
    };

    let mut prev_branch: Option<BranchId> = None;

    for diff_line in diff_lines {
        match diff_line.origin {
            // ── Deleted line ─────────────────────────────────────────────
            '-' => {
                let branch_id = alloc_branch();

                // Store the deleted line content as leaf ops so
                // `atomic diff -c` can reconstruct the old line text.
                let content_bytes = &diff_line.content;
                // Strip trailing newline for storage (consistent with inserts).
                let trimmed = if content_bytes.ends_with(b"\n") {
                    &content_bytes[..content_bytes.len() - 1]
                } else {
                    content_bytes
                };

                let leaf_ops = vec![LeafOp::Insert {
                    after: None,
                    kind: crate::diff::TokenKind::Word,
                    content: trimmed.to_vec(),
                }];

                let mut line_op = BuilderLineOps::delete(branch_id, leaf_ops);
                if let Some(n) = diff_line.old_lineno {
                    line_op = line_op.with_old_line_num(n as usize);
                }
                file_ops.add_line_op(line_op);
                stats.lines_deleted += 1;
            }

            // ── Added line ───────────────────────────────────────────────
            '+' => {
                let branch_id = alloc_branch();

                let content_bytes = &diff_line.content;
                let trimmed = if content_bytes.ends_with(b"\n") {
                    &content_bytes[..content_bytes.len() - 1]
                } else {
                    content_bytes
                };

                let leaf_id = alloc_leaf();
                let leaf_ops = vec![LeafOp::Insert {
                    after: None,
                    kind: crate::diff::TokenKind::Word,
                    content: trimmed.to_vec(),
                }];
                let _ = leaf_id; // leaf_id allocated for ordering; content is in leaf_ops

                let mut line_op = BuilderLineOps::insert(branch_id, prev_branch, leaf_ops);
                if let Some(n) = diff_line.new_lineno {
                    line_op = line_op.with_new_line_num(n as usize);
                }
                file_ops.add_line_op(line_op);
                stats.lines_added += 1;
                prev_branch = Some(branch_id);
            }

            // ── Context line (unchanged) — update prev_branch ────────────
            ' ' => {
                // Context lines are not recorded as BranchOps; they only
                // advance the `prev_branch` cursor so subsequent inserts
                // are anchored after the right line.
                //
                // We allocate a phantom branch ID to maintain the ordering
                // chain.  It is never stored anywhere.
                let phantom_id = alloc_branch();
                prev_branch = Some(phantom_id);
            }

            _ => {} // Hunk headers, no-newline markers, etc. — skip.
        }
    }

    (file_ops.into_change_ops(), stats)
}

fn calculate_line_offsets(content: &[u8]) -> Vec<usize> {
    let mut offsets = vec![0];
    for (i, &byte) in content.iter().enumerate() {
        if byte == b'\n' && i + 1 < content.len() {
            offsets.push(i + 1);
        }
    }
    offsets
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Memory;

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

        let result = record_modified_file(&wc, &detected, old_content, &options);

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

        let result = record_modified_file(&wc, &detected, old_content, &options);

        assert!(result.is_err());
    }

    #[test]
    fn test_record_modified_file_with_diff() {
        let wc = Memory::new();
        wc.add_file("test.rs", b"line1\nline2\nline3\n");

        let detected = DetectedFile::modified("test.rs");
        let old_content = b"line1\nold_line\nline3\n";
        let options = RecordingOptions::new();

        let result = record_modified_file(&wc, &detected, old_content, &options);

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

        let result = record_modified_file(&wc, &detected, old_content, &options);
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

        let result = record_modified_file(&wc, &detected, old_content, &options);
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

        let result = record_modified_file(&wc, &detected, old_content, &options);
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

        let result = record_modified_file(&wc, &detected, old_content, &options);
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
}

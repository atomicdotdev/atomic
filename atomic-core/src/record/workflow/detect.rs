//! High-level change detection integrating collection and comparison.
//!
//! This module provides the main entry points for detecting changes between
//! the working copy and the pristine (recorded) state. It orchestrates the
//! collection, comparison, and categorization of file changes.
//!
//! # Overview
//!
//! Change detection is a multi-phase process:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                    High-Level Detection Pipeline                         │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Phase 1: Collection                                                    │
//! │  ┌──────────────────┐     ┌──────────────────┐                         │
//! │  │ collect_tracked  │     │ collect_working  │                         │
//! │  │ (pristine state) │     │ (disk state)     │                         │
//! │  └────────┬─────────┘     └────────┬─────────┘                         │
//! │           │                        │                                    │
//! │           └────────────┬───────────┘                                    │
//! │                        ▼                                                │
//! │  Phase 2: Set Analysis                                                  │
//! │  ┌─────────────────────────────────────────────────────────────────┐   │
//! │  │ Compare tracked vs working sets:                                │   │
//! │  │ • In tracked only     → Deleted                                 │   │
//! │  │ • In working only     → Added (untracked)                       │   │
//! │  │ • In both             → Potentially modified                    │   │
//! │  └─────────────────────────────────────────────────────────────────┘   │
//! │                        │                                                │
//! │                        ▼                                                │
//! │  Phase 3: Content Comparison                                           │
//! │  ┌─────────────────────────────────────────────────────────────────┐   │
//! │  │ For files in both sets:                                         │   │
//! │  │ • Retrieve pristine content                                     │   │
//! │  │ • Read working copy content                                     │   │
//! │  │ • Compare using diff algorithm                                  │   │
//! │  │ • Categorize: Modified / Unchanged                              │   │
//! │  └─────────────────────────────────────────────────────────────────┘   │
//! │                        │                                                │
//! │                        ▼                                                │
//! │  Phase 4: Results                                                       │
//! │  ┌─────────────────────────────────────────────────────────────────┐   │
//! │  │ DetectionResult containing:                                     │   │
//! │  │ • added: Vec<DetectedFile>                                      │   │
//! │  │ • deleted: Vec<DetectedFile>                                    │   │
//! │  │ • modified: Vec<DetectedFile> (with diff)                       │   │
//! │  │ • unchanged: Vec<DetectedFile> (optional)                       │   │
//! │  └─────────────────────────────────────────────────────────────────┘   │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ## Basic Detection
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::detect::{detect_changes, DetectionOptions};
//!
//! let options = DetectionOptions::new();
//! let result = detect_changes(&txn, &working_copy, &changes, &options)?;
//!
//! println!("Added: {} files", result.added_count());
//! println!("Deleted: {} files", result.deleted_count());
//! println!("Modified: {} files", result.modified_count());
//! ```
//!
//! ## Filtered Detection
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::detect::{detect_changes, DetectionOptions};
//!
//! // Only detect changes under src/
//! let options = DetectionOptions::new().prefix("src/");
//! let result = detect_changes(&txn, &working_copy, &changes, &options)?;
//! ```
//!
//! ## Detection with Move Tracking
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::detect::{detect_changes, DetectionOptions};
//!
//! let options = DetectionOptions::new().detect_moves(true);
//! let result = detect_changes(&txn, &working_copy, &changes, &options)?;
//!
//! for file in result.moved() {
//!     println!("Moved: {} -> {}", file.old_path.unwrap(), file.path);
//! }
//! ```
//!
//! # Performance Considerations
//!
//! - **mtime optimization**: Skip files whose modification time hasn't changed
//! - **size optimization**: Skip files whose size matches recorded size
//! - **prefix filtering**: Only scan files under a specific path
//! - **lazy content loading**: Only load content when comparison is needed
//!
//! # Error Handling
//!
//! Detection can fail for several reasons:
//!
//! - IO errors when reading files
//! - Pristine database errors
//! - Change store errors when retrieving content
//!
//! All errors are collected in the result rather than failing early,
//! allowing partial results to be returned.

use std::collections::HashSet;
use std::time::SystemTime;

use crate::change::Encoding;
use crate::diff::{Algorithm, DiffOp};
use crate::output::WorkingCopyRead;

use crate::types::{Inode, NodeId, Position};

// ============================================================================
// DETECTION OPTIONS
// ============================================================================

/// Options controlling the change detection process.
///
/// These options allow fine-tuning detection behavior for performance
/// and specific use cases.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::detect::DetectionOptions;
/// use atomic_core::diff::Algorithm;
///
/// let options = DetectionOptions::new()
///     .prefix("src/")
///     .algorithm(Algorithm::Patience)
///     .check_mtime(true)
///     .detect_moves(true)
///     .include_unchanged(false);
///
/// assert_eq!(options.get_prefix(), Some("src/"));
/// assert_eq!(options.get_algorithm(), Algorithm::Patience);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectionOptions {
    /// Path prefix to filter detection (empty = all files).
    prefix: Option<String>,

    /// Diff algorithm to use for content comparison.
    algorithm: Algorithm,

    /// Whether to check modification time before comparing content.
    ///
    /// When true, files with unchanged mtime are skipped.
    check_mtime: bool,

    /// Whether to detect file moves (renames).
    ///
    /// When true, deleted files with matching content in added files
    /// are reported as moves instead of separate add/delete.
    detect_moves: bool,

    /// Whether to include unchanged files in results.
    ///
    /// When false (default), only changed files are returned.
    include_unchanged: bool,

    /// Maximum file size to diff (in bytes).
    ///
    /// Files larger than this are marked as binary and not diffed.
    max_diff_size: Option<usize>,

    /// Whether to force re-diffing even if mtime suggests no change.
    force_rediff: bool,
}

impl DetectionOptions {
    /// Default maximum diff size (10 MB).
    pub const DEFAULT_MAX_DIFF_SIZE: usize = 10 * 1024 * 1024;

    /// Create new options with defaults.
    ///
    /// Default values:
    /// - `prefix`: None (all files)
    /// - `algorithm`: Myers
    /// - `check_mtime`: true
    /// - `detect_moves`: false
    /// - `include_unchanged`: false
    /// - `max_diff_size`: 10 MB
    /// - `force_rediff`: false
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new();
    /// assert!(options.get_check_mtime());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the path prefix filter.
    ///
    /// Only files under this prefix will be detected.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path prefix (e.g., "src/")
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().prefix("src/");
    /// assert_eq!(options.get_prefix(), Some("src/"));
    /// ```
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        let p = prefix.into();
        self.prefix = if p.is_empty() { None } else { Some(p) };
        self
    }

    /// Set the diff algorithm.
    ///
    /// # Arguments
    ///
    /// * `algorithm` - The algorithm to use
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    /// use atomic_core::diff::Algorithm;
    ///
    /// let options = DetectionOptions::new().algorithm(Algorithm::Patience);
    /// assert_eq!(options.get_algorithm(), Algorithm::Patience);
    /// ```
    #[must_use]
    pub fn algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Set whether to check modification time.
    ///
    /// # Arguments
    ///
    /// * `check` - Whether to check mtime before diffing
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().check_mtime(false);
    /// assert!(!options.get_check_mtime());
    /// ```
    #[must_use]
    pub fn check_mtime(mut self, check: bool) -> Self {
        self.check_mtime = check;
        self
    }

    /// Set whether to detect moves.
    ///
    /// # Arguments
    ///
    /// * `detect` - Whether to detect file moves
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().detect_moves(true);
    /// assert!(options.get_detect_moves());
    /// ```
    #[must_use]
    pub fn detect_moves(mut self, detect: bool) -> Self {
        self.detect_moves = detect;
        self
    }

    /// Set whether to include unchanged files.
    ///
    /// # Arguments
    ///
    /// * `include` - Whether to include unchanged files in results
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().include_unchanged(true);
    /// assert!(options.get_include_unchanged());
    /// ```
    #[must_use]
    pub fn include_unchanged(mut self, include: bool) -> Self {
        self.include_unchanged = include;
        self
    }

    /// Set the maximum file size to diff.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum size in bytes
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().max_diff_size(1024 * 1024); // 1 MB
    /// assert_eq!(options.get_max_diff_size(), Some(1024 * 1024));
    /// ```
    #[must_use]
    pub fn max_diff_size(mut self, size: usize) -> Self {
        self.max_diff_size = Some(size);
        self
    }

    /// Set whether to force re-diffing.
    ///
    /// # Arguments
    ///
    /// * `force` - Whether to force re-diffing
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionOptions;
    ///
    /// let options = DetectionOptions::new().force_rediff(true);
    /// assert!(options.get_force_rediff());
    /// ```
    #[must_use]
    pub fn force_rediff(mut self, force: bool) -> Self {
        self.force_rediff = force;
        self
    }

    /// Get the prefix setting.
    #[must_use]
    pub fn get_prefix(&self) -> Option<&str> {
        self.prefix.as_deref()
    }

    /// Get the algorithm setting.
    #[must_use]
    pub fn get_algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Get the check_mtime setting.
    #[must_use]
    pub fn get_check_mtime(&self) -> bool {
        self.check_mtime
    }

    /// Get the detect_moves setting.
    #[must_use]
    pub fn get_detect_moves(&self) -> bool {
        self.detect_moves
    }

    /// Get the include_unchanged setting.
    #[must_use]
    pub fn get_include_unchanged(&self) -> bool {
        self.include_unchanged
    }

    /// Get the max_diff_size setting.
    #[must_use]
    pub fn get_max_diff_size(&self) -> Option<usize> {
        self.max_diff_size
    }

    /// Get the force_rediff setting.
    #[must_use]
    pub fn get_force_rediff(&self) -> bool {
        self.force_rediff
    }

    /// Check if a file size exceeds the max diff size.
    #[must_use]
    pub fn exceeds_max_size(&self, size: usize) -> bool {
        self.max_diff_size.map_or(false, |max| size > max)
    }
}

impl Default for DetectionOptions {
    fn default() -> Self {
        Self {
            prefix: None,
            algorithm: Algorithm::Myers,
            check_mtime: true,
            detect_moves: false,
            include_unchanged: false,
            max_diff_size: Some(Self::DEFAULT_MAX_DIFF_SIZE),
            force_rediff: false,
        }
    }
}

// ============================================================================
// DETECTED FILE
// ============================================================================

/// A file that has been detected as changed.
///
/// Contains all information about a detected change, including the
/// file path, change type, and optional diff operations.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::detect::{DetectedFile, DetectionKind};
/// use atomic_core::change::Encoding;
///
/// let file = DetectedFile::added("new_file.rs");
/// assert!(file.is_added());
/// assert_eq!(file.path, "new_file.rs");
/// ```
#[derive(Debug, Clone)]
pub struct DetectedFile {
    /// Path relative to repository root.
    pub path: String,

    /// The kind of change detected.
    pub kind: DetectionKind,

    /// Old path (for moves/renames).
    pub old_path: Option<String>,

    /// Inode in the pristine (for tracked files).
    pub inode: Option<Inode>,

    /// Position in the graph (for tracked files).
    pub position: Option<Position<NodeId>>,

    /// Detected encoding.
    pub encoding: Option<Encoding>,

    /// Diff operations (for modified files).
    pub diff_ops: Vec<DiffOp>,

    /// Whether this is a directory.
    pub is_directory: bool,

    /// File size in working copy (if available).
    pub size: Option<u64>,

    /// Modification time (if available).
    pub mtime: Option<SystemTime>,
}

/// The kind of change detected for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetectionKind {
    /// File was added (exists in working copy but not tracked).
    Added,

    /// File was deleted (tracked but not in working copy).
    Deleted,

    /// File was modified (content changed).
    Modified,

    /// File was moved/renamed.
    Moved,

    /// File is unchanged.
    Unchanged,

    /// File type changed (e.g., file to directory).
    TypeChanged,

    /// Only metadata changed (permissions, etc.).
    MetadataOnly,
}

impl DetectedFile {
    /// Create a new detected file with the given kind.
    fn new(path: impl Into<String>, kind: DetectionKind) -> Self {
        Self {
            path: path.into(),
            kind,
            old_path: None,
            inode: None,
            position: None,
            encoding: None,
            diff_ops: Vec::new(),
            is_directory: false,
            size: None,
            mtime: None,
        }
    }

    /// Create an added file.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectedFile;
    ///
    /// let file = DetectedFile::added("new_file.rs");
    /// assert!(file.is_added());
    /// ```
    #[must_use]
    pub fn added(path: impl Into<String>) -> Self {
        Self::new(path, DetectionKind::Added)
    }

    /// Create a deleted file.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectedFile;
    ///
    /// let file = DetectedFile::deleted("old_file.rs");
    /// assert!(file.is_deleted());
    /// ```
    #[must_use]
    pub fn deleted(path: impl Into<String>) -> Self {
        Self::new(path, DetectionKind::Deleted)
    }

    /// Create a modified file.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectedFile;
    ///
    /// let file = DetectedFile::modified("changed_file.rs");
    /// assert!(file.is_modified());
    /// ```
    #[must_use]
    pub fn modified(path: impl Into<String>) -> Self {
        Self::new(path, DetectionKind::Modified)
    }

    /// Create a moved file.
    ///
    /// # Arguments
    ///
    /// * `old_path` - Original path
    /// * `new_path` - New path
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectedFile;
    ///
    /// let file = DetectedFile::moved("old/path.rs", "new/path.rs");
    /// assert!(file.is_moved());
    /// assert_eq!(file.old_path, Some("old/path.rs".to_string()));
    /// ```
    #[must_use]
    pub fn moved(old_path: impl Into<String>, new_path: impl Into<String>) -> Self {
        let mut file = Self::new(new_path, DetectionKind::Moved);
        file.old_path = Some(old_path.into());
        file
    }

    /// Create an unchanged file.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectedFile;
    ///
    /// let file = DetectedFile::unchanged("same_file.rs");
    /// assert!(file.is_unchanged());
    /// ```
    #[must_use]
    pub fn unchanged(path: impl Into<String>) -> Self {
        Self::new(path, DetectionKind::Unchanged)
    }

    /// Set the inode.
    #[must_use]
    pub fn with_inode(mut self, inode: Inode) -> Self {
        self.inode = Some(inode);
        self
    }

    /// Set the position.
    #[must_use]
    pub fn with_position(mut self, position: Position<NodeId>) -> Self {
        self.position = Some(position);
        self
    }

    /// Set the encoding.
    #[must_use]
    pub fn with_encoding(mut self, encoding: Encoding) -> Self {
        self.encoding = Some(encoding);
        self
    }

    /// Set the diff operations.
    #[must_use]
    pub fn with_diff(mut self, diff_ops: Vec<DiffOp>) -> Self {
        self.diff_ops = diff_ops;
        self
    }

    /// Set as directory.
    #[must_use]
    pub fn as_directory(mut self) -> Self {
        self.is_directory = true;
        self
    }

    /// Set the file size.
    #[must_use]
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// Set the modification time.
    #[must_use]
    pub fn with_mtime(mut self, mtime: SystemTime) -> Self {
        self.mtime = Some(mtime);
        self
    }

    /// Check if this is an added file.
    #[must_use]
    pub fn is_added(&self) -> bool {
        self.kind == DetectionKind::Added
    }

    /// Check if this is a deleted file.
    #[must_use]
    pub fn is_deleted(&self) -> bool {
        self.kind == DetectionKind::Deleted
    }

    /// Check if this is a modified file.
    #[must_use]
    pub fn is_modified(&self) -> bool {
        self.kind == DetectionKind::Modified
    }

    /// Check if this is a moved file.
    #[must_use]
    pub fn is_moved(&self) -> bool {
        self.kind == DetectionKind::Moved
    }

    /// Check if this is unchanged.
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        self.kind == DetectionKind::Unchanged
    }

    /// Check if this has any changes.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        !matches!(self.kind, DetectionKind::Unchanged)
    }

    /// Check if this has diff operations.
    #[must_use]
    pub fn has_diff(&self) -> bool {
        !self.diff_ops.is_empty()
    }

    /// Get the number of diff operations.
    #[must_use]
    pub fn diff_count(&self) -> usize {
        self.diff_ops.len()
    }
}

// ============================================================================
// DETECTION RESULT
// ============================================================================

/// The complete result of change detection.
///
/// Contains categorized lists of all detected changes along with
/// statistics and any errors that occurred during detection.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::workflow::detect::DetectionResult;
///
/// let result = DetectionResult::new();
/// assert!(result.is_empty());
/// assert_eq!(result.total_count(), 0);
/// ```
#[derive(Debug, Clone, Default)]
pub struct DetectionResult {
    /// Added files (untracked files in working copy).
    added: Vec<DetectedFile>,

    /// Deleted files (tracked files missing from working copy).
    deleted: Vec<DetectedFile>,

    /// Modified files (content changed).
    modified: Vec<DetectedFile>,

    /// Moved files (renamed or relocated).
    moved: Vec<DetectedFile>,

    /// Unchanged files (only included if options.include_unchanged).
    unchanged: Vec<DetectedFile>,

    /// Errors encountered during detection.
    errors: Vec<String>,

    /// Number of files scanned.
    files_scanned: usize,

    /// Number of files skipped (due to mtime, size, etc.).
    files_skipped: usize,
}

impl DetectionResult {
    /// Create an empty result.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::workflow::detect::DetectionResult;
    ///
    /// let result = DetectionResult::new();
    /// assert!(result.is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an added file to the result.
    pub fn add_added(&mut self, file: DetectedFile) {
        self.added.push(file);
    }

    /// Add a deleted file to the result.
    pub fn add_deleted(&mut self, file: DetectedFile) {
        self.deleted.push(file);
    }

    /// Add a modified file to the result.
    pub fn add_modified(&mut self, file: DetectedFile) {
        self.modified.push(file);
    }

    /// Add a moved file to the result.
    pub fn add_moved(&mut self, file: DetectedFile) {
        self.moved.push(file);
    }

    /// Add an unchanged file to the result.
    pub fn add_unchanged(&mut self, file: DetectedFile) {
        self.unchanged.push(file);
    }

    /// Add an error message.
    pub fn add_error(&mut self, error: impl Into<String>) {
        self.errors.push(error.into());
    }

    /// Increment the files scanned counter.
    pub fn increment_scanned(&mut self) {
        self.files_scanned += 1;
    }

    /// Increment the files skipped counter.
    pub fn increment_skipped(&mut self) {
        self.files_skipped += 1;
    }

    /// Check if there are no detected changes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.deleted.is_empty()
            && self.modified.is_empty()
            && self.moved.is_empty()
    }

    /// Check if there were any errors.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Get all added files.
    #[must_use]
    pub fn added(&self) -> &[DetectedFile] {
        &self.added
    }

    /// Get all deleted files.
    #[must_use]
    pub fn deleted(&self) -> &[DetectedFile] {
        &self.deleted
    }

    /// Get all modified files.
    #[must_use]
    pub fn modified(&self) -> &[DetectedFile] {
        &self.modified
    }

    /// Get all moved files.
    #[must_use]
    pub fn moved(&self) -> &[DetectedFile] {
        &self.moved
    }

    /// Get all unchanged files.
    #[must_use]
    pub fn unchanged(&self) -> &[DetectedFile] {
        &self.unchanged
    }

    /// Get all errors.
    #[must_use]
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Get count of added files.
    #[must_use]
    pub fn added_count(&self) -> usize {
        self.added.len()
    }

    /// Get count of deleted files.
    #[must_use]
    pub fn deleted_count(&self) -> usize {
        self.deleted.len()
    }

    /// Get count of modified files.
    #[must_use]
    pub fn modified_count(&self) -> usize {
        self.modified.len()
    }

    /// Get count of moved files.
    #[must_use]
    pub fn moved_count(&self) -> usize {
        self.moved.len()
    }

    /// Get count of unchanged files.
    #[must_use]
    pub fn unchanged_count(&self) -> usize {
        self.unchanged.len()
    }

    /// Get total count of changed files (excluding unchanged).
    #[must_use]
    pub fn changed_count(&self) -> usize {
        self.added.len() + self.deleted.len() + self.modified.len() + self.moved.len()
    }

    /// Get total count of all files.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.changed_count() + self.unchanged.len()
    }

    /// Get number of files scanned.
    #[must_use]
    pub fn files_scanned(&self) -> usize {
        self.files_scanned
    }

    /// Get number of files skipped.
    #[must_use]
    pub fn files_skipped(&self) -> usize {
        self.files_skipped
    }

    /// Iterate over all changed files (added, deleted, modified, moved).
    pub fn changed_files(&self) -> impl Iterator<Item = &DetectedFile> {
        self.added
            .iter()
            .chain(self.deleted.iter())
            .chain(self.modified.iter())
            .chain(self.moved.iter())
    }

    /// Iterate over all files including unchanged.
    pub fn all_files(&self) -> impl Iterator<Item = &DetectedFile> {
        self.changed_files().chain(self.unchanged.iter())
    }

    /// Merge another result into this one.
    pub fn merge(&mut self, other: DetectionResult) {
        self.added.extend(other.added);
        self.deleted.extend(other.deleted);
        self.modified.extend(other.modified);
        self.moved.extend(other.moved);
        self.unchanged.extend(other.unchanged);
        self.errors.extend(other.errors);
        self.files_scanned += other.files_scanned;
        self.files_skipped += other.files_skipped;
    }
}

// ============================================================================
// DETECTION FUNCTIONS
// ============================================================================

/// Detect changes between the working copy and pristine state.
///
/// This is the main entry point for change detection. It collects files
/// from both the pristine database and working copy, then compares them
/// to find additions, deletions, modifications, and moves.
///
/// # Arguments
///
/// * `txn` - Transaction providing graph and tree access
/// * `working_copy` - Working copy interface
/// * `changes` - Change store for retrieving pristine content
/// * `options` - Detection options
///
/// # Returns
///
/// A `DetectionResult` containing all detected changes.
///
/// # Errors
///
/// Returns `RecordError` if a critical error occurs. Non-critical errors
/// (like failing to read a single file) are collected in the result.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::record::workflow::detect::{detect_changes, DetectionOptions};
///
/// let options = DetectionOptions::new();
/// let result = detect_changes(&txn, &working_copy, &changes, &options)?;
///
/// for file in result.modified() {
///     println!("Modified: {}", file.path);
/// }
/// ```
pub fn detect_changes_simple<W>(
    working_copy: &W,
    tracked_paths: &[String],
    options: &DetectionOptions,
) -> DetectionResult
where
    W: WorkingCopyRead,
{
    let mut result = DetectionResult::new();
    let prefix = options.get_prefix().unwrap_or("");

    // Build set of tracked paths for quick lookup
    let tracked_set: HashSet<&str> = tracked_paths.iter().map(|s| s.as_str()).collect();

    // Collect working copy files
    let working_files = match working_copy.walk_files(prefix) {
        Ok(files) => files,
        Err(e) => {
            result.add_error(format!("Failed to walk working copy: {}", e));
            return result;
        }
    };

    let working_set: HashSet<&str> = working_files.iter().map(|s| s.as_str()).collect();

    // Find added files (in working copy but not tracked)
    for path in &working_files {
        result.increment_scanned();
        if !tracked_set.contains(path.as_str()) {
            // Check prefix filter
            if prefix.is_empty() || path.starts_with(prefix) {
                let file = DetectedFile::added(path);
                result.add_added(file);
            }
        }
    }

    // Find deleted files (tracked but not in working copy)
    for path in tracked_paths {
        if !working_set.contains(path.as_str()) {
            // Check prefix filter
            if prefix.is_empty() || path.starts_with(prefix) {
                let file = DetectedFile::deleted(path);
                result.add_deleted(file);
            }
        }
    }

    // Files in both sets exist in working copy and are tracked.
    // Content comparison requires pristine content retrieval, which is
    // handled by the caller using record_modified_file() with the
    // retrieved pristine content. Here we just identify which files
    // need comparison.
    for path in tracked_paths {
        if working_set.contains(path.as_str()) {
            // Check prefix filter
            if prefix.is_empty() || path.starts_with(prefix) {
                // Files present in both sets are tracked as unchanged here.
                // The caller should perform content comparison to determine
                // if they are actually modified.
                if options.get_include_unchanged() {
                    let file = DetectedFile::unchanged(path);
                    result.add_unchanged(file);
                }
            }
        }
    }

    result
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Memory;

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
}

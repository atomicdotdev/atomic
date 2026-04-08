//! Core types for the change detection module.
//!
//! This module contains [`DetectedFile`], [`DetectionKind`], and
//! [`DetectionResult`] — the primary data structures produced by
//! the detection pipeline.

use std::time::SystemTime;

use crate::change::Encoding;
use crate::diff::DiffOp;
use crate::types::{Inode, NodeId, Position};

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
    #[must_use]
    pub fn added(path: impl Into<String>) -> Self {
        Self::new(path, DetectionKind::Added)
    }

    /// Create a deleted file.
    #[must_use]
    pub fn deleted(path: impl Into<String>) -> Self {
        Self::new(path, DetectionKind::Deleted)
    }

    /// Create a modified file.
    #[must_use]
    pub fn modified(path: impl Into<String>) -> Self {
        Self::new(path, DetectionKind::Modified)
    }

    /// Create a moved file.
    #[must_use]
    pub fn moved(old_path: impl Into<String>, new_path: impl Into<String>) -> Self {
        let mut file = Self::new(new_path, DetectionKind::Moved);
        file.old_path = Some(old_path.into());
        file
    }

    /// Create an unchanged file.
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

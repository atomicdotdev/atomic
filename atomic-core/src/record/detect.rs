//! Change detection between working copy and pristine state.
//!
//! This module provides functionality for detecting modifications in the
//! working copy compared to the pristine (recorded) state. It is the core
//! engine that powers the `record` command.
//!
//! # Overview
//!
//! Change detection involves comparing each tracked file's current state
//! (in the working copy) with its recorded state (in the pristine graph).
//! The result is a list of detected changes that can be converted into
//! hunks for a new change.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                       Change Detection Pipeline                          │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  Working Copy          Detection              Detected Changes          │
//! │  ┌──────────────┐     ┌─────────────────┐    ┌─────────────────────┐   │
//! │  │ File Content │ ──► │ Compare with    │ ─► │ FileChange::Added   │   │
//! │  │ File Status  │     │ Pristine State  │    │ FileChange::Modified│   │
//! │  │ Metadata     │     │ using Diff Algo │    │ FileChange::Deleted │   │
//! │  └──────────────┘     └─────────────────┘    │ FileChange::Moved   │   │
//! │                                              └─────────────────────┘   │
//! │                                                                         │
//! │  Detection Types:                                                       │
//! │  ┌─────────────────────────────────────────────────────────────────┐   │
//! │  │ Added    - New file in working copy, not in pristine           │   │
//! │  │ Deleted  - File in pristine, missing from working copy         │   │
//! │  │ Modified - Content changed between pristine and working copy   │   │
//! │  │ Moved    - File renamed or relocated                           │   │
//! │  │ MetaOnly - Only metadata changed (permissions, etc.)           │   │
//! │  └─────────────────────────────────────────────────────────────────┘   │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::record::detect::{detect_changes, DetectOptions};
//! use atomic_core::diff::Algorithm;
//!
//! // Detect all changes in the working copy
//! let result = detect_changes(
//!     &txn,
//!     &working_copy,
//!     &changes,
//!     "",  // empty prefix = entire repo
//!     DetectOptions::new(),
//! )?;
//!
//! for change in &result.changes {
//!     match &change.kind {
//!         FileChangeKind::Added => println!("Added: {}", change.path),
//!         FileChangeKind::Modified { diff_ops } => {
//!             println!("Modified: {} ({} operations)", change.path, diff_ops.len());
//!         }
//!         FileChangeKind::Deleted => println!("Deleted: {}", change.path),
//!         _ => {}
//!     }
//! }
//! ```
//!
//! # Performance
//!
//! Detection can be expensive for large repositories. Optimizations:
//!
//! - **mtime checking**: Skip files whose modification time hasn't changed
//! - **size checking**: Skip files whose size matches the recorded size
//! - **prefix filtering**: Only check files under a specific path
//! - **parallel detection**: Process files in parallel (optional)

use std::time::SystemTime;

use crate::change::Encoding;
use crate::diff::{diff, Algorithm, DiffOp, Line};
use crate::output::FileMetadata;
use crate::types::{Inode, NodeId, Position};

// ============================================================================
// DETECT OPTIONS
// ============================================================================

/// Options for change detection.
///
/// Controls how changes are detected between the working copy and pristine.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::DetectOptions;
/// use atomic_core::diff::Algorithm;
///
/// // Default options
/// let opts = DetectOptions::new();
///
/// // With custom diff algorithm
/// let opts = DetectOptions::new()
///     .algorithm(Algorithm::Patience);
///
/// // Skip mtime optimization (always compare content)
/// let opts = DetectOptions::new()
///     .check_mtime(false);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectOptions {
    /// Diff algorithm to use for content comparison.
    pub algorithm: Algorithm,

    /// Whether to use mtime to skip unchanged files.
    ///
    /// When true, files whose mtime matches the recorded time are skipped.
    pub check_mtime: bool,

    /// Whether to detect moved files.
    ///
    /// Move detection requires comparing content hashes, which can be slow.
    pub detect_moves: bool,

    /// Whether to detect encoding changes.
    ///
    /// Check if file encoding has changed (e.g., UTF-8 to binary).
    pub detect_encoding_changes: bool,

    /// Whether to detect permission changes.
    pub detect_permission_changes: bool,

    /// Prefix to filter detection.
    ///
    /// Only files under this prefix will be checked.
    pub prefix: String,

    /// Maximum file size to diff (in bytes).
    ///
    /// Files larger than this are treated as binary.
    pub max_diff_size: Option<u64>,

    /// Force re-diff even if file appears unchanged.
    pub force_rediff: bool,
}

impl DetectOptions {
    /// Create new options with defaults.
    ///
    /// Default configuration:
    /// - Algorithm: Myers
    /// - check_mtime: true
    /// - detect_moves: true
    /// - detect_encoding_changes: true
    /// - detect_permission_changes: true
    /// - prefix: "" (all files)
    /// - max_diff_size: Some(10MB)
    /// - force_rediff: false
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::DetectOptions;
    ///
    /// let opts = DetectOptions::new();
    /// assert!(opts.check_mtime);
    /// assert!(opts.detect_moves);
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the diff algorithm.
    ///
    /// # Arguments
    ///
    /// * `algorithm` - Algorithm to use for content comparison
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::DetectOptions;
    /// use atomic_core::diff::Algorithm;
    ///
    /// let opts = DetectOptions::new().algorithm(Algorithm::Patience);
    /// assert_eq!(opts.algorithm, Algorithm::Patience);
    /// ```
    pub fn algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Set whether to use mtime optimization.
    ///
    /// # Arguments
    ///
    /// * `check` - Whether to check mtime before comparing content
    pub fn check_mtime(mut self, check: bool) -> Self {
        self.check_mtime = check;
        self
    }

    /// Set whether to detect moved files.
    ///
    /// # Arguments
    ///
    /// * `detect` - Whether to detect file moves
    pub fn detect_moves(mut self, detect: bool) -> Self {
        self.detect_moves = detect;
        self
    }

    /// Set whether to detect encoding changes.
    ///
    /// # Arguments
    ///
    /// * `detect` - Whether to detect encoding changes
    pub fn detect_encoding_changes(mut self, detect: bool) -> Self {
        self.detect_encoding_changes = detect;
        self
    }

    /// Set whether to detect permission changes.
    ///
    /// # Arguments
    ///
    /// * `detect` - Whether to detect permission changes
    pub fn detect_permission_changes(mut self, detect: bool) -> Self {
        self.detect_permission_changes = detect;
        self
    }

    /// Set the prefix filter.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path prefix to filter files
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Set the maximum file size to diff.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum size in bytes
    pub fn max_diff_size(mut self, size: u64) -> Self {
        self.max_diff_size = Some(size);
        self
    }

    /// Set whether to force re-diff.
    ///
    /// # Arguments
    ///
    /// * `force` - Whether to force re-diffing
    pub fn force_rediff(mut self, force: bool) -> Self {
        self.force_rediff = force;
        self
    }

    /// Check if a path matches the prefix filter.
    pub fn matches_prefix(&self, path: &str) -> bool {
        if self.prefix.is_empty() {
            true
        } else {
            path.starts_with(&self.prefix)
        }
    }
}

impl Default for DetectOptions {
    fn default() -> Self {
        Self {
            algorithm: Algorithm::Myers,
            check_mtime: true,
            detect_moves: true,
            detect_encoding_changes: true,
            detect_permission_changes: true,
            prefix: String::new(),
            max_diff_size: Some(10 * 1024 * 1024), // 10 MB
            force_rediff: false,
        }
    }
}

// ============================================================================
// FILE CHANGE KIND
// ============================================================================

/// The kind of change detected for a file.
///
/// Each variant represents a different type of modification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChangeKind {
    /// File was added to the working copy.
    ///
    /// This means the file exists in the working copy but has no
    /// corresponding entry in the pristine graph.
    Added {
        /// Detected encoding of the new file.
        encoding: Encoding,
    },

    /// File content was modified.
    ///
    /// The file exists in both working copy and pristine, but the
    /// content has changed.
    Modified {
        /// Diff operations describing the changes.
        diff_ops: Vec<DiffOp>,
        /// Old encoding (from pristine).
        old_encoding: Encoding,
        /// New encoding (from working copy).
        new_encoding: Encoding,
    },

    /// File was deleted from the working copy.
    ///
    /// The file exists in pristine but not in the working copy.
    Deleted,

    /// File was moved/renamed.
    ///
    /// The file exists at a different path than in pristine.
    Moved {
        /// The original path (from pristine).
        from_path: String,
        /// Whether the content also changed.
        content_changed: bool,
        /// Diff operations if content changed.
        diff_ops: Option<Vec<DiffOp>>,
    },

    /// Only metadata changed (permissions, etc.).
    ///
    /// The content is unchanged but file metadata differs.
    MetadataOnly {
        /// Old metadata.
        old_metadata: FileMetadata,
        /// New metadata.
        new_metadata: FileMetadata,
    },
}

impl FileChangeKind {
    /// Check if this is an addition.
    pub fn is_added(&self) -> bool {
        matches!(self, Self::Added { .. })
    }

    /// Check if this is a modification.
    pub fn is_modified(&self) -> bool {
        matches!(self, Self::Modified { .. })
    }

    /// Check if this is a deletion.
    pub fn is_deleted(&self) -> bool {
        matches!(self, Self::Deleted)
    }

    /// Check if this is a move.
    pub fn is_moved(&self) -> bool {
        matches!(self, Self::Moved { .. })
    }

    /// Check if this is a metadata-only change.
    pub fn is_metadata_only(&self) -> bool {
        matches!(self, Self::MetadataOnly { .. })
    }

    /// Get the diff operations if available.
    pub fn diff_ops(&self) -> Option<&[DiffOp]> {
        match self {
            Self::Modified { diff_ops, .. } => Some(diff_ops),
            Self::Moved {
                diff_ops: Some(ops),
                ..
            } => Some(ops),
            _ => None,
        }
    }

    /// Check if this change involves content modification.
    pub fn has_content_change(&self) -> bool {
        match self {
            Self::Added { .. } => true,
            Self::Modified { .. } => true,
            Self::Deleted => true,
            Self::Moved { content_changed, .. } => *content_changed,
            Self::MetadataOnly { .. } => false,
        }
    }
}

// ============================================================================
// FILE CHANGE
// ============================================================================

/// A detected change to a file.
///
/// Contains all information about a single file's changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Path to the file (relative to repository root).
    pub path: String,

    /// Inode of the file (if tracked).
    pub inode: Option<Inode>,

    /// Position in the graph (if exists in pristine).
    pub position: Option<Position<NodeId>>,

    /// The kind of change detected.
    pub kind: FileChangeKind,

    /// File metadata from working copy.
    pub metadata: Option<FileMetadata>,

    /// Size of the file in bytes (from working copy).
    pub size: Option<u64>,

    /// Modification time of the file.
    pub mtime: Option<SystemTime>,
}

impl FileChange {
    /// Create a new file change for an added file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file
    /// * `encoding` - Detected encoding
    pub fn added(path: impl Into<String>, encoding: Encoding) -> Self {
        Self {
            path: path.into(),
            inode: None,
            position: None,
            kind: FileChangeKind::Added { encoding },
            metadata: None,
            size: None,
            mtime: None,
        }
    }

    /// Create a new file change for a modified file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file
    /// * `inode` - File's inode
    /// * `position` - Position in the graph
    /// * `diff_ops` - Diff operations describing changes
    /// * `old_encoding` - Encoding in pristine
    /// * `new_encoding` - Encoding in working copy
    pub fn modified(
        path: impl Into<String>,
        inode: Inode,
        position: Position<NodeId>,
        diff_ops: Vec<DiffOp>,
        old_encoding: Encoding,
        new_encoding: Encoding,
    ) -> Self {
        Self {
            path: path.into(),
            inode: Some(inode),
            position: Some(position),
            kind: FileChangeKind::Modified {
                diff_ops,
                old_encoding,
                new_encoding,
            },
            metadata: None,
            size: None,
            mtime: None,
        }
    }

    /// Create a new file change for a deleted file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file
    /// * `inode` - File's inode
    /// * `position` - Position in the graph
    pub fn deleted(path: impl Into<String>, inode: Inode, position: Position<NodeId>) -> Self {
        Self {
            path: path.into(),
            inode: Some(inode),
            position: Some(position),
            kind: FileChangeKind::Deleted,
            metadata: None,
            size: None,
            mtime: None,
        }
    }

    /// Create a new file change for a moved file.
    ///
    /// # Arguments
    ///
    /// * `new_path` - New path to the file
    /// * `from_path` - Original path
    /// * `inode` - File's inode
    /// * `position` - Position in the graph
    /// * `content_changed` - Whether content also changed
    /// * `diff_ops` - Diff operations if content changed
    pub fn moved(
        new_path: impl Into<String>,
        from_path: impl Into<String>,
        inode: Inode,
        position: Position<NodeId>,
        content_changed: bool,
        diff_ops: Option<Vec<DiffOp>>,
    ) -> Self {
        Self {
            path: new_path.into(),
            inode: Some(inode),
            position: Some(position),
            kind: FileChangeKind::Moved {
                from_path: from_path.into(),
                content_changed,
                diff_ops,
            },
            metadata: None,
            size: None,
            mtime: None,
        }
    }

    /// Create a new file change for metadata-only changes.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file
    /// * `inode` - File's inode
    /// * `position` - Position in the graph
    /// * `old_metadata` - Old metadata
    /// * `new_metadata` - New metadata
    pub fn metadata_only(
        path: impl Into<String>,
        inode: Inode,
        position: Position<NodeId>,
        old_metadata: FileMetadata,
        new_metadata: FileMetadata,
    ) -> Self {
        Self {
            path: path.into(),
            inode: Some(inode),
            position: Some(position),
            kind: FileChangeKind::MetadataOnly {
                old_metadata,
                new_metadata,
            },
            metadata: Some(new_metadata),
            size: None,
            mtime: None,
        }
    }

    /// Set the metadata for this change.
    pub fn with_metadata(mut self, metadata: FileMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Set the size for this change.
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// Set the mtime for this change.
    pub fn with_mtime(mut self, mtime: SystemTime) -> Self {
        self.mtime = Some(mtime);
        self
    }
}

// ============================================================================
// DETECT RESULT
// ============================================================================

/// Result of change detection.
///
/// Contains all detected changes and statistics about the detection process.
#[derive(Debug, Clone, Default)]
pub struct DetectResult {
    /// All detected file changes.
    pub changes: Vec<FileChange>,

    /// Number of files checked.
    pub files_checked: usize,

    /// Number of files skipped (e.g., due to mtime).
    pub files_skipped: usize,

    /// Number of added files.
    pub added_count: usize,

    /// Number of modified files.
    pub modified_count: usize,

    /// Number of deleted files.
    pub deleted_count: usize,

    /// Number of moved files.
    pub moved_count: usize,

    /// Number of metadata-only changes.
    pub metadata_only_count: usize,

    /// Errors encountered during detection.
    pub errors: Vec<(String, String)>,
}

impl DetectResult {
    /// Create a new empty result.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a detected change.
    pub fn add_change(&mut self, change: FileChange) {
        match &change.kind {
            FileChangeKind::Added { .. } => self.added_count += 1,
            FileChangeKind::Modified { .. } => self.modified_count += 1,
            FileChangeKind::Deleted => self.deleted_count += 1,
            FileChangeKind::Moved { .. } => self.moved_count += 1,
            FileChangeKind::MetadataOnly { .. } => self.metadata_only_count += 1,
        }
        self.changes.push(change);
    }

    /// Record that a file was checked.
    pub fn record_checked(&mut self) {
        self.files_checked += 1;
    }

    /// Record that a file was skipped.
    pub fn record_skipped(&mut self) {
        self.files_skipped += 1;
    }

    /// Record an error.
    pub fn record_error(&mut self, path: String, error: String) {
        self.errors.push((path, error));
    }

    /// Check if any changes were detected.
    pub fn has_changes(&self) -> bool {
        !self.changes.is_empty()
    }

    /// Get the total number of changes.
    pub fn change_count(&self) -> usize {
        self.changes.len()
    }

    /// Check if there were errors.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Get only added files.
    pub fn added(&self) -> impl Iterator<Item = &FileChange> {
        self.changes.iter().filter(|c| c.kind.is_added())
    }

    /// Get only modified files.
    pub fn modified(&self) -> impl Iterator<Item = &FileChange> {
        self.changes.iter().filter(|c| c.kind.is_modified())
    }

    /// Get only deleted files.
    pub fn deleted(&self) -> impl Iterator<Item = &FileChange> {
        self.changes.iter().filter(|c| c.kind.is_deleted())
    }

    /// Get only moved files.
    pub fn moved(&self) -> impl Iterator<Item = &FileChange> {
        self.changes.iter().filter(|c| c.kind.is_moved())
    }

    /// Merge another result into this one.
    pub fn merge(&mut self, other: DetectResult) {
        self.files_checked += other.files_checked;
        self.files_skipped += other.files_skipped;
        self.added_count += other.added_count;
        self.modified_count += other.modified_count;
        self.deleted_count += other.deleted_count;
        self.moved_count += other.moved_count;
        self.metadata_only_count += other.metadata_only_count;
        self.changes.extend(other.changes);
        self.errors.extend(other.errors);
    }
}

// ============================================================================
// CONTENT COMPARISON
// ============================================================================

/// Compare two byte slices and produce diff operations.
///
/// This function handles the actual diffing of file content.
///
/// # Arguments
///
/// * `old_content` - Content from pristine
/// * `new_content` - Content from working copy
/// * `algorithm` - Diff algorithm to use
///
/// # Returns
///
/// A vector of diff operations describing the changes.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::compare_content;
/// use atomic_core::diff::Algorithm;
///
/// let old = b"hello\nworld\n";
/// let new = b"hello\nbeautiful\nworld\n";
///
/// let ops = compare_content(old, new, Algorithm::Myers);
/// assert!(!ops.is_empty());
/// ```
pub fn compare_content(old_content: &[u8], new_content: &[u8], algorithm: Algorithm) -> Vec<DiffOp> {
    // Convert to lines
    let old_lines: Vec<Line> = Line::from_bytes(old_content);
    let new_lines: Vec<Line> = Line::from_bytes(new_content);

    // Perform diff and extract operations
    let result = diff(&old_lines, &new_lines, algorithm);
    result.ops().to_vec()
}

/// Detect the encoding of content.
///
/// # Arguments
///
/// * `content` - The content to analyze
///
/// # Returns
///
/// The detected encoding.
pub fn detect_encoding(content: &[u8]) -> Encoding {
    // Check for UTF-8 validity
    if std::str::from_utf8(content).is_ok() {
        // Check for null bytes (binary indicator even in valid UTF-8)
        if content.contains(&0) {
            Encoding::Binary
        } else {
            Encoding::Utf8
        }
    } else {
        Encoding::Binary
    }
}

/// Check if content is binary.
///
/// # Arguments
///
/// * `content` - The content to check
///
/// # Returns
///
/// `true` if the content appears to be binary.
pub fn is_binary_content(content: &[u8]) -> bool {
    // Quick check: null bytes indicate binary
    if content.contains(&0) {
        return true;
    }

    // Check for high ratio of non-printable characters
    let non_printable = content
        .iter()
        .filter(|&&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
        .count();

    // If more than 30% non-printable, consider binary
    if content.len() > 0 && non_printable * 100 / content.len() > 30 {
        return true;
    }

    false
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // DetectOptions Tests
    // ========================================================================

    #[test]
    fn test_options_new() {
        let opts = DetectOptions::new();

        assert_eq!(opts.algorithm, Algorithm::Myers);
        assert!(opts.check_mtime);
        assert!(opts.detect_moves);
        assert!(opts.detect_encoding_changes);
        assert!(opts.detect_permission_changes);
        assert!(opts.prefix.is_empty());
        assert_eq!(opts.max_diff_size, Some(10 * 1024 * 1024));
        assert!(!opts.force_rediff);
    }

    #[test]
    fn test_options_default() {
        let opts = DetectOptions::default();

        assert_eq!(opts.algorithm, Algorithm::Myers);
        assert!(opts.check_mtime);
    }

    #[test]
    fn test_options_algorithm() {
        let opts = DetectOptions::new().algorithm(Algorithm::Patience);

        assert_eq!(opts.algorithm, Algorithm::Patience);
    }

    #[test]
    fn test_options_check_mtime() {
        let opts = DetectOptions::new().check_mtime(false);

        assert!(!opts.check_mtime);
    }

    #[test]
    fn test_options_detect_moves() {
        let opts = DetectOptions::new().detect_moves(false);

        assert!(!opts.detect_moves);
    }

    #[test]
    fn test_options_detect_encoding_changes() {
        let opts = DetectOptions::new().detect_encoding_changes(false);

        assert!(!opts.detect_encoding_changes);
    }

    #[test]
    fn test_options_detect_permission_changes() {
        let opts = DetectOptions::new().detect_permission_changes(false);

        assert!(!opts.detect_permission_changes);
    }

    #[test]
    fn test_options_prefix() {
        let opts = DetectOptions::new().prefix("src/");

        assert_eq!(opts.prefix, "src/");
    }

    #[test]
    fn test_options_max_diff_size() {
        let opts = DetectOptions::new().max_diff_size(1024);

        assert_eq!(opts.max_diff_size, Some(1024));
    }

    #[test]
    fn test_options_force_rediff() {
        let opts = DetectOptions::new().force_rediff(true);

        assert!(opts.force_rediff);
    }

    #[test]
    fn test_options_chaining() {
        let opts = DetectOptions::new()
            .algorithm(Algorithm::Patience)
            .check_mtime(false)
            .detect_moves(false)
            .prefix("test/")
            .force_rediff(true);

        assert_eq!(opts.algorithm, Algorithm::Patience);
        assert!(!opts.check_mtime);
        assert!(!opts.detect_moves);
        assert_eq!(opts.prefix, "test/");
        assert!(opts.force_rediff);
    }

    #[test]
    fn test_options_matches_prefix_empty() {
        let opts = DetectOptions::new();

        assert!(opts.matches_prefix("anything"));
        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix(""));
    }

    #[test]
    fn test_options_matches_prefix_with_prefix() {
        let opts = DetectOptions::new().prefix("src/");

        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix("src/lib/mod.rs"));
        assert!(!opts.matches_prefix("tests/test.rs"));
    }

    #[test]
    fn test_options_clone() {
        let opts = DetectOptions::new().prefix("test/");
        let cloned = opts.clone();

        assert_eq!(opts, cloned);
    }

    #[test]
    fn test_options_debug() {
        let opts = DetectOptions::new();
        let debug = format!("{:?}", opts);

        assert!(debug.contains("DetectOptions"));
    }

    // ========================================================================
    // FileChangeKind Tests
    // ========================================================================

    #[test]
    fn test_change_kind_added() {
        let kind = FileChangeKind::Added {
            encoding: Encoding::Utf8,
        };

        assert!(kind.is_added());
        assert!(!kind.is_modified());
        assert!(!kind.is_deleted());
        assert!(!kind.is_moved());
        assert!(!kind.is_metadata_only());
        assert!(kind.has_content_change());
        assert!(kind.diff_ops().is_none());
    }

    #[test]
    fn test_change_kind_modified() {
        let kind = FileChangeKind::Modified {
            diff_ops: vec![DiffOp::equal(0, 0, 1)],
            old_encoding: Encoding::Utf8,
            new_encoding: Encoding::Utf8,
        };

        assert!(!kind.is_added());
        assert!(kind.is_modified());
        assert!(!kind.is_deleted());
        assert!(kind.has_content_change());
        assert!(kind.diff_ops().is_some());
        assert_eq!(kind.diff_ops().unwrap().len(), 1);
    }

    #[test]
    fn test_change_kind_deleted() {
        let kind = FileChangeKind::Deleted;

        assert!(kind.is_deleted());
        assert!(!kind.is_added());
        assert!(kind.has_content_change());
        assert!(kind.diff_ops().is_none());
    }

    #[test]
    fn test_change_kind_moved_without_content_change() {
        let kind = FileChangeKind::Moved {
            from_path: "old/path.rs".to_string(),
            content_changed: false,
            diff_ops: None,
        };

        assert!(kind.is_moved());
        assert!(!kind.is_modified());
        assert!(!kind.has_content_change());
        assert!(kind.diff_ops().is_none());
    }

    #[test]
    fn test_change_kind_moved_with_content_change() {
        let kind = FileChangeKind::Moved {
            from_path: "old/path.rs".to_string(),
            content_changed: true,
            diff_ops: Some(vec![DiffOp::insert(0, 0, 1)]),
        };

        assert!(kind.is_moved());
        assert!(kind.has_content_change());
        assert!(kind.diff_ops().is_some());
    }

    #[test]
    fn test_change_kind_metadata_only() {
        let kind = FileChangeKind::MetadataOnly {
            old_metadata: FileMetadata::file(),
            new_metadata: FileMetadata::executable(),
        };

        assert!(kind.is_metadata_only());
        assert!(!kind.is_modified());
        assert!(!kind.has_content_change());
        assert!(kind.diff_ops().is_none());
    }

    // ========================================================================
    // FileChange Tests
    // ========================================================================

    #[test]
    fn test_file_change_added() {
        let change = FileChange::added("test.rs", Encoding::Utf8);

        assert_eq!(change.path, "test.rs");
        assert!(change.inode.is_none());
        assert!(change.position.is_none());
        assert!(change.kind.is_added());
    }

    #[test]
    fn test_file_change_modified() {
        let change = FileChange::modified(
            "test.rs",
            Inode::ROOT,
            Position::ROOT,
            vec![DiffOp::equal(0, 0, 1)],
            Encoding::Utf8,
            Encoding::Utf8,
        );

        assert_eq!(change.path, "test.rs");
        assert!(change.inode.is_some());
        assert!(change.position.is_some());
        assert!(change.kind.is_modified());
    }

    #[test]
    fn test_file_change_deleted() {
        let change = FileChange::deleted("test.rs", Inode::ROOT, Position::ROOT);

        assert_eq!(change.path, "test.rs");
        assert!(change.kind.is_deleted());
    }

    #[test]
    fn test_file_change_moved() {
        let change = FileChange::moved(
            "new/path.rs",
            "old/path.rs",
            Inode::ROOT,
            Position::ROOT,
            false,
            None,
        );

        assert_eq!(change.path, "new/path.rs");
        assert!(change.kind.is_moved());
    }

    #[test]
    fn test_file_change_metadata_only() {
        let change = FileChange::metadata_only(
            "test.rs",
            Inode::ROOT,
            Position::ROOT,
            FileMetadata::file(),
            FileMetadata::executable(),
        );

        assert!(change.kind.is_metadata_only());
        assert!(change.metadata.is_some());
    }

    #[test]
    fn test_file_change_with_metadata() {
        let change = FileChange::added("test.rs", Encoding::Utf8)
            .with_metadata(FileMetadata::executable());

        assert!(change.metadata.is_some());
        assert!(change.metadata.unwrap().is_executable());
    }

    #[test]
    fn test_file_change_with_size() {
        let change = FileChange::added("test.rs", Encoding::Utf8).with_size(1024);

        assert_eq!(change.size, Some(1024));
    }

    #[test]
    fn test_file_change_with_mtime() {
        let now = SystemTime::now();
        let change = FileChange::added("test.rs", Encoding::Utf8).with_mtime(now);

        assert!(change.mtime.is_some());
    }

    #[test]
    fn test_file_change_clone() {
        let change = FileChange::added("test.rs", Encoding::Utf8);
        let cloned = change.clone();

        assert_eq!(change, cloned);
    }

    #[test]
    fn test_file_change_debug() {
        let change = FileChange::added("test.rs", Encoding::Utf8);
        let debug = format!("{:?}", change);

        assert!(debug.contains("FileChange"));
        assert!(debug.contains("test.rs"));
    }

    // ========================================================================
    // DetectResult Tests
    // ========================================================================

    #[test]
    fn test_result_new() {
        let result = DetectResult::new();

        assert!(result.changes.is_empty());
        assert_eq!(result.files_checked, 0);
        assert_eq!(result.files_skipped, 0);
        assert_eq!(result.added_count, 0);
        assert_eq!(result.modified_count, 0);
        assert_eq!(result.deleted_count, 0);
        assert!(!result.has_changes());
        assert!(!result.has_errors());
    }

    #[test]
    fn test_result_add_change_added() {
        let mut result = DetectResult::new();
        result.add_change(FileChange::added("test.rs", Encoding::Utf8));

        assert_eq!(result.added_count, 1);
        assert_eq!(result.change_count(), 1);
        assert!(result.has_changes());
    }

    #[test]
    fn test_result_add_change_modified() {
        let mut result = DetectResult::new();
        result.add_change(FileChange::modified(
            "test.rs",
            Inode::ROOT,
            Position::ROOT,
            vec![],
            Encoding::Utf8,
            Encoding::Utf8,
        ));

        assert_eq!(result.modified_count, 1);
    }

    #[test]
    fn test_result_add_change_deleted() {
        let mut result = DetectResult::new();
        result.add_change(FileChange::deleted("test.rs", Inode::ROOT, Position::ROOT));

        assert_eq!(result.deleted_count, 1);
    }

    #[test]
    fn test_result_add_change_moved() {
        let mut result = DetectResult::new();
        result.add_change(FileChange::moved(
            "new.rs",
            "old.rs",
            Inode::ROOT,
            Position::ROOT,
            false,
            None,
        ));

        assert_eq!(result.moved_count, 1);
    }

    #[test]
    fn test_result_add_change_metadata_only() {
        let mut result = DetectResult::new();
        result.add_change(FileChange::metadata_only(
            "test.rs",
            Inode::ROOT,
            Position::ROOT,
            FileMetadata::file(),
            FileMetadata::executable(),
        ));

        assert_eq!(result.metadata_only_count, 1);
    }

    #[test]
    fn test_result_record_checked() {
        let mut result = DetectResult::new();
        result.record_checked();
        result.record_checked();

        assert_eq!(result.files_checked, 2);
    }

    #[test]
    fn test_result_record_skipped() {
        let mut result = DetectResult::new();
        result.record_skipped();

        assert_eq!(result.files_skipped, 1);
    }

    #[test]
    fn test_result_record_error() {
        let mut result = DetectResult::new();
        result.record_error("test.rs".to_string(), "error".to_string());

        assert!(result.has_errors());
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_result_iterators() {
        let mut result = DetectResult::new();
        result.add_change(FileChange::added("a.rs", Encoding::Utf8));
        result.add_change(FileChange::modified(
            "b.rs",
            Inode::ROOT,
            Position::ROOT,
            vec![],
            Encoding::Utf8,
            Encoding::Utf8,
        ));
        result.add_change(FileChange::deleted("c.rs", Inode::ROOT, Position::ROOT));

        assert_eq!(result.added().count(), 1);
        assert_eq!(result.modified().count(), 1);
        assert_eq!(result.deleted().count(), 1);
        assert_eq!(result.moved().count(), 0);
    }

    #[test]
    fn test_result_merge() {
        let mut result1 = DetectResult::new();
        result1.add_change(FileChange::added("a.rs", Encoding::Utf8));
        result1.files_checked = 5;

        let mut result2 = DetectResult::new();
        result2.add_change(FileChange::deleted("b.rs", Inode::ROOT, Position::ROOT));
        result2.files_checked = 3;

        result1.merge(result2);

        assert_eq!(result1.change_count(), 2);
        assert_eq!(result1.files_checked, 8);
        assert_eq!(result1.added_count, 1);
        assert_eq!(result1.deleted_count, 1);
    }

    #[test]
    fn test_result_clone() {
        let mut result = DetectResult::new();
        result.add_change(FileChange::added("test.rs", Encoding::Utf8));

        let cloned = result.clone();

        assert_eq!(result.change_count(), cloned.change_count());
    }

    #[test]
    fn test_result_debug() {
        let result = DetectResult::new();
        let debug = format!("{:?}", result);

        assert!(debug.contains("DetectResult"));
    }

    // ========================================================================
    // Content Comparison Tests
    // ========================================================================

    #[test]
    fn test_compare_content_identical() {
        let content = b"hello\nworld\n";
        let ops = compare_content(content, content, Algorithm::Myers);

        // All equal operations
        assert!(ops.iter().all(|op| op.is_equal()));
    }

    #[test]
    fn test_compare_content_insertion() {
        let old = b"hello\nworld\n";
        let new = b"hello\nbeautiful\nworld\n";

        let ops = compare_content(old, new, Algorithm::Myers);

        // Should have at least one insert
        assert!(ops.iter().any(|op| op.is_insert()));
    }

    #[test]
    fn test_compare_content_deletion() {
        let old = b"hello\nbeautiful\nworld\n";
        let new = b"hello\nworld\n";

        let ops = compare_content(old, new, Algorithm::Myers);

        // Should have at least one delete
        assert!(ops.iter().any(|op| op.is_delete()));
    }

    #[test]
    fn test_compare_content_empty_old() {
        let old = b"";
        let new = b"hello\n";

        let ops = compare_content(old, new, Algorithm::Myers);

        // Should be all inserts
        assert!(ops.iter().all(|op| op.is_insert() || op.is_equal()));
    }

    #[test]
    fn test_compare_content_empty_new() {
        let old = b"hello\n";
        let new = b"";

        let ops = compare_content(old, new, Algorithm::Myers);

        // Should be all deletes
        assert!(ops.iter().all(|op| op.is_delete() || op.is_equal()));
    }

    #[test]
    fn test_compare_content_patience() {
        let old = b"hello\nworld\n";
        let new = b"hello\nbeautiful\nworld\n";

        let ops = compare_content(old, new, Algorithm::Patience);

        // Should produce valid diff
        assert!(!ops.is_empty());
    }

    // ========================================================================
    // Encoding Detection Tests
    // ========================================================================

    #[test]
    fn test_detect_encoding_utf8() {
        let content = b"hello world";
        let encoding = detect_encoding(content);

        assert_eq!(encoding, Encoding::Utf8);
    }

    #[test]
    fn test_detect_encoding_binary_null() {
        let content = b"hello\x00world";
        let encoding = detect_encoding(content);

        assert_eq!(encoding, Encoding::Binary);
    }

    #[test]
    fn test_detect_encoding_binary_invalid_utf8() {
        let content = &[0xff, 0xfe, 0x00, 0x01];
        let encoding = detect_encoding(content);

        assert_eq!(encoding, Encoding::Binary);
    }

    #[test]
    fn test_detect_encoding_empty() {
        let content = b"";
        let encoding = detect_encoding(content);

        assert_eq!(encoding, Encoding::Utf8);
    }

    // ========================================================================
    // Binary Detection Tests
    // ========================================================================

    #[test]
    fn test_is_binary_null_bytes() {
        let content = b"hello\x00world";
        assert!(is_binary_content(content));
    }

    #[test]
    fn test_is_binary_text() {
        let content = b"hello world\n";
        assert!(!is_binary_content(content));
    }

    #[test]
    fn test_is_binary_high_non_printable() {
        // More than 30% non-printable
        let content = &[0x01, 0x02, 0x03, 0x04, b'a', b'b'];
        assert!(is_binary_content(content));
    }

    #[test]
    fn test_is_binary_empty() {
        let content = b"";
        assert!(!is_binary_content(content));
    }

    #[test]
    fn test_is_binary_with_tabs_and_newlines() {
        let content = b"hello\tworld\nfoo\rbar";
        assert!(!is_binary_content(content));
    }
}

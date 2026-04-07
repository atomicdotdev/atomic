//! Recording result types.
//!
//! Contains the data structures produced by the recording pipeline:
//! - [`RecordingStats`]: Metrics about the recording process
//! - [`RecordedFile`]: Information about a single recorded file
//! - [`RecordingResult`]: The complete result of recording detected changes

use crate::change::{Encoding, FileOps};
use crate::record::workflow::crdt::CrdtBuildStats;
use crate::record::workflow::detect::DetectionKind;
use crate::record::workflow::graph_op::BuiltHunk;
use crate::types::{Inode, NodeId, Position};

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
    #[must_use]
    pub fn is_empty(&self) -> bool {
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

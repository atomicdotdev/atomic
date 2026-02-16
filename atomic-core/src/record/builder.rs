//! Record builder for accumulating changes from the working copy
//!
//! The [`RecordBuilder`] is the main entry point for recording changes. It
//! accumulates detected modifications (file additions, deletions, edits) and
//! converts them into [`GraphOp`] operations that can be serialized into a
//! [`Change`].
//!
//! # Overview
//!
//! Recording is a multi-step process:
//!
//! 1. Create a `RecordBuilder`
//! 2. Add recorded actions (hunks) for each modified file
//! 3. Track inode updates for database maintenance
//! 4. Finalize into a `Recorded` result
//! 5. Convert to a `Change` with a header
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                         RecordBuilder                                   │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  ┌─────────────┐    ┌─────────────┐    ┌─────────────────────────────┐ │
//! │  │   Hunks     │    │  Contents   │    │      Inode Updates          │ │
//! │  │  (actions)  │    │  (bytes)    │    │  (Add/Delete tracking)      │ │
//! │  └─────────────┘    └─────────────┘    └─────────────────────────────┘ │
//! │         │                  │                        │                   │
//! │         └──────────────────┴────────────────────────┘                   │
//! │                            │                                            │
//! │                            ▼                                            │
//! │                     ┌─────────────┐                                     │
//! │                     │  Recorded   │                                     │
//! │                     │  (result)   │                                     │
//! │                     └─────────────┘                                     │
//! │                            │                                            │
//! │                            ▼                                            │
//! │                     ┌─────────────┐                                     │
//! │                     │   Change    │                                     │
//! │                     │  (final)    │                                     │
//! │                     └─────────────┘                                     │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust
//! use atomic_core::record::RecordBuilder;
//!
//! // Create a builder
//! let mut builder = RecordBuilder::new();
//!
//! // Initially empty (no hunks)
//! assert!(builder.is_empty());
//!
//! // Add content for a new file
//! let content = b"Hello, World!\n";
//! let content_start = builder.append_contents(content);
//! let content_end = content_start + content.len() as u64;
//!
//! // The builder tracks the content
//! assert_eq!(builder.contents_len(), content.len());
//!
//! // Still "empty" until hunks are added (is_empty checks hunks, not content)
//! assert!(builder.is_empty());
//! assert_eq!(builder.hunk_count(), 0);
//! ```
//!
//! # Thread Safety
//!
//! The `RecordBuilder` is designed for single-threaded use. For parallel
//! recording, create multiple builders and merge results.
//!
//! [`GraphOp`]: crate::change::GraphOp
//! [`Change`]: crate::change::Change

use std::collections::HashMap;
use std::time::SystemTime;

#[allow(unused_imports)]
use crate::change::{Encoding, GraphOp, Insertion};
use crate::types::{Hash, Inode, NodeId, Position};

use super::item::InodeUpdate;

/// Builder for accumulating recorded changes.
///
/// This is the main interface for recording changes from the working copy.
/// It accumulates hunks (file operations) and their content, then produces
/// a [`Recorded`] result that can be converted into a [`Change`].
///
/// # Usage Pattern
///
/// ```rust
/// use atomic_core::record::RecordBuilder;
///
/// let mut builder = RecordBuilder::new();
///
/// // Record changes...
/// // builder.record_file_add(...);
/// // builder.record_edit(...);
///
/// // Finalize
/// let recorded = builder.finish();
/// ```
///
/// # Configuration
///
/// The builder supports several configuration options:
///
/// - `force_rediff`: Force re-diffing even if file appears unchanged
/// - `ignore_missing`: Don't error on missing files
///
/// [`Recorded`]: crate::record::Recorded
/// [`Change`]: crate::change::Change
#[derive(Debug)]
pub struct RecordBuilder {
    /// Accumulated hunks (file operations).
    ///
    /// Uses `Option<Hash>` because the change being recorded doesn't
    /// have a hash yet - it will be computed when serialized.
    actions: Vec<GraphOp<Option<Hash>>>,

    /// Raw byte contents referenced by hunks.
    contents: Vec<u8>,

    /// Inode updates for database maintenance.
    ///
    /// Maps graph_op index to the inode update that should be applied
    /// when the change is applied locally.
    updatables: HashMap<usize, InodeUpdate>,

    /// Inodes that have been recorded (to avoid duplicates).
    recorded_inodes: HashMap<Inode, Position<Option<NodeId>>>,

    /// The largest file size encountered during recording.
    largest_file: u64,

    /// Whether any binary files were recorded.
    has_binary_files: bool,

    /// The oldest modification time of recorded files.
    oldest_change: SystemTime,

    /// Force re-diffing even if file appears unchanged.
    pub force_rediff: bool,

    /// Don't error on missing files.
    pub ignore_missing: bool,
}

impl RecordBuilder {
    /// Create a new empty record builder.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::RecordBuilder;
    ///
    /// let builder = RecordBuilder::new();
    /// assert!(builder.is_empty());
    /// ```
    pub fn new() -> Self {
        Self {
            actions: Vec::new(),
            contents: Vec::new(),
            updatables: HashMap::new(),
            recorded_inodes: HashMap::new(),
            largest_file: 0,
            has_binary_files: false,
            oldest_change: SystemTime::UNIX_EPOCH,
            force_rediff: false,
            ignore_missing: false,
        }
    }

    /// Create a builder with pre-allocated capacity.
    ///
    /// Use this when you know approximately how many actions and content
    /// bytes will be recorded.
    ///
    /// # Arguments
    ///
    /// * `action_capacity` - Expected number of hunks
    /// * `content_capacity` - Expected total content bytes
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::RecordBuilder;
    ///
    /// // Pre-allocate for a large recording session
    /// let builder = RecordBuilder::with_capacity(100, 1024 * 1024);
    /// ```
    pub fn with_capacity(action_capacity: usize, content_capacity: usize) -> Self {
        Self {
            actions: Vec::with_capacity(action_capacity),
            contents: Vec::with_capacity(content_capacity),
            updatables: HashMap::with_capacity(action_capacity / 4),
            recorded_inodes: HashMap::new(),
            largest_file: 0,
            has_binary_files: false,
            oldest_change: SystemTime::UNIX_EPOCH,
            force_rediff: false,
            ignore_missing: false,
        }
    }

    /// Check if no changes have been recorded.
    ///
    /// # Returns
    ///
    /// `true` if no hunks have been added.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::RecordBuilder;
    ///
    /// let builder = RecordBuilder::new();
    /// assert!(builder.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Get the number of recorded hunks.
    ///
    /// # Returns
    ///
    /// The count of hunks (actions) that have been recorded.
    pub fn hunk_count(&self) -> usize {
        self.actions.len()
    }

    /// Get the total size of recorded contents.
    ///
    /// # Returns
    ///
    /// The number of bytes in the contents buffer.
    pub fn contents_len(&self) -> usize {
        self.contents.len()
    }

    /// Append raw bytes to the contents buffer.
    ///
    /// Returns the starting position of the appended content, which can be
    /// used to reference this content in hunks.
    ///
    /// # Arguments
    ///
    /// * `data` - The bytes to append
    ///
    /// # Returns
    ///
    /// The byte offset where the content was appended.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::RecordBuilder;
    ///
    /// let mut builder = RecordBuilder::new();
    ///
    /// let start = builder.append_contents(b"Hello");
    /// assert_eq!(start, 0);
    ///
    /// let start2 = builder.append_contents(b" World");
    /// assert_eq!(start2, 5);
    /// ```
    pub fn append_contents(&mut self, data: &[u8]) -> u64 {
        let start = self.contents.len() as u64;
        self.contents.extend_from_slice(data);
        start
    }

    /// Get a reference to the contents buffer.
    ///
    /// # Returns
    ///
    /// A slice of all recorded content bytes.
    pub fn contents(&self) -> &[u8] {
        &self.contents
    }

    /// Add a graph_op to the recorded actions.
    ///
    /// # Arguments
    ///
    /// * `graph_op` - The graph_op to add
    ///
    /// # Returns
    ///
    /// The index of the added graph_op (for associating inode updates).
    pub fn add_hunk(&mut self, graph_op: GraphOp<Option<Hash>>) -> usize {
        let index = self.actions.len();
        self.actions.push(graph_op);
        index
    }

    /// Associate an inode update with a graph_op.
    ///
    /// When the change is applied locally, this update will be used to
    /// maintain the tree/inode database tables.
    ///
    /// # Arguments
    ///
    /// * `hunk_index` - The index of the graph_op (from `add_hunk`)
    /// * `update` - The inode update to associate
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::{RecordBuilder, InodeUpdate};
    /// use atomic_core::types::{ChangePosition, Inode};
    ///
    /// let mut builder = RecordBuilder::new();
    ///
    /// // After adding a graph_op for a new file...
    /// // let hunk_index = builder.add_hunk(file_add_hunk);
    ///
    /// // Associate the inode update
    /// // builder.add_inode_update(hunk_index, InodeUpdate::add(
    /// //     ChangePosition::new(0),
    /// //     Inode::new(42),
    /// // ));
    /// ```
    pub fn add_inode_update(&mut self, hunk_index: usize, update: InodeUpdate) {
        self.updatables.insert(hunk_index, update);
    }

    /// Mark an inode as recorded at a specific position.
    ///
    /// This prevents duplicate recording of the same inode.
    ///
    /// # Arguments
    ///
    /// * `inode` - The inode being recorded
    /// * `position` - The graph position of the inode
    pub fn mark_inode_recorded(&mut self, inode: Inode, position: Position<Option<NodeId>>) {
        self.recorded_inodes.insert(inode, position);
    }

    /// Check if an inode has already been recorded.
    ///
    /// # Arguments
    ///
    /// * `inode` - The inode to check
    ///
    /// # Returns
    ///
    /// `Some(position)` if already recorded, `None` otherwise.
    pub fn get_recorded_inode(&self, inode: &Inode) -> Option<Position<Option<NodeId>>> {
        self.recorded_inodes.get(inode).copied()
    }

    /// Check if an inode has been recorded.
    pub fn is_inode_recorded(&self, inode: &Inode) -> bool {
        self.recorded_inodes.contains_key(inode)
    }

    /// Update the largest file size if this file is larger.
    ///
    /// # Arguments
    ///
    /// * `size` - The file size in bytes
    pub fn update_largest_file(&mut self, size: u64) {
        self.largest_file = self.largest_file.max(size);
    }

    /// Get the largest file size encountered.
    pub fn largest_file(&self) -> u64 {
        self.largest_file
    }

    /// Mark that a binary file was recorded.
    pub fn mark_binary_file(&mut self) {
        self.has_binary_files = true;
    }

    /// Check if any binary files were recorded.
    pub fn has_binary_files(&self) -> bool {
        self.has_binary_files
    }

    /// Update the oldest modification time if this is older.
    ///
    /// # Arguments
    ///
    /// * `time` - The file modification time
    pub fn update_oldest_change(&mut self, time: SystemTime) {
        if self.oldest_change == SystemTime::UNIX_EPOCH
            || (time > SystemTime::UNIX_EPOCH && time < self.oldest_change)
        {
            self.oldest_change = time;
        }
    }

    /// Get the oldest modification time.
    pub fn oldest_change(&self) -> SystemTime {
        self.oldest_change
    }

    /// Finish recording and produce a `Recorded` result.
    ///
    /// This consumes the builder and returns the accumulated state.
    ///
    /// # Returns
    ///
    /// A `Recorded` containing all hunks, contents, and metadata.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::record::RecordBuilder;
    ///
    /// let mut builder = RecordBuilder::new();
    /// builder.append_contents(b"test content");
    ///
    /// let recorded = builder.finish();
    /// assert_eq!(recorded.contents().len(), 12);
    /// ```
    pub fn finish(self) -> Recorded {
        Recorded {
            actions: self.actions,
            contents: self.contents,
            updatables: self.updatables,
            largest_file: self.largest_file,
            has_binary_files: self.has_binary_files,
            oldest_change: self.oldest_change,
        }
    }

    /// Clear all recorded state for reuse.
    ///
    /// This retains allocated capacity for efficiency.
    pub fn clear(&mut self) {
        self.actions.clear();
        self.contents.clear();
        self.updatables.clear();
        self.recorded_inodes.clear();
        self.largest_file = 0;
        self.has_binary_files = false;
        self.oldest_change = SystemTime::UNIX_EPOCH;
    }

    /// Get statistics about the recording.
    pub fn stats(&self) -> RecordStats {
        RecordStats {
            hunk_count: self.actions.len(),
            content_bytes: self.contents.len(),
            inode_update_count: self.updatables.len(),
            recorded_inode_count: self.recorded_inodes.len(),
            largest_file: self.largest_file,
            has_binary_files: self.has_binary_files,
        }
    }
}

impl Default for RecordBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of a recording session.
///
/// This contains all the accumulated hunks, content, and metadata from
/// recording. It can be converted into a [`Change`] by providing a header.
///
/// # Example
///
/// ```rust
/// use atomic_core::record::RecordBuilder;
///
/// let mut builder = RecordBuilder::new();
/// builder.append_contents(b"file content");
///
/// let recorded = builder.finish();
///
/// // Access the results
/// assert!(!recorded.is_empty_contents());
/// ```
///
/// [`Change`]: crate::change::Change
#[derive(Debug)]
pub struct Recorded {
    /// The recorded hunks (file operations).
    actions: Vec<GraphOp<Option<Hash>>>,

    /// Raw byte contents referenced by hunks.
    contents: Vec<u8>,

    /// Inode updates for database maintenance.
    updatables: HashMap<usize, InodeUpdate>,

    /// The largest file size encountered.
    largest_file: u64,

    /// Whether any binary files were recorded.
    has_binary_files: bool,

    /// The oldest modification time.
    oldest_change: SystemTime,
}

impl Recorded {
    /// Check if no hunks were recorded.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Get the number of recorded hunks.
    pub fn hunk_count(&self) -> usize {
        self.actions.len()
    }

    /// Get a reference to the recorded hunks.
    pub fn actions(&self) -> &[GraphOp<Option<Hash>>] {
        &self.actions
    }

    /// Take ownership of the recorded hunks.
    pub fn take_actions(self) -> Vec<GraphOp<Option<Hash>>> {
        self.actions
    }

    /// Check if the contents buffer is empty.
    pub fn is_empty_contents(&self) -> bool {
        self.contents.is_empty()
    }

    /// Get the size of the contents buffer.
    pub fn contents_len(&self) -> usize {
        self.contents.len()
    }

    /// Get a reference to the contents buffer.
    pub fn contents(&self) -> &[u8] {
        &self.contents
    }

    /// Take ownership of the contents buffer.
    pub fn take_contents(self) -> Vec<u8> {
        self.contents
    }

    /// Get the inode updates.
    pub fn updatables(&self) -> &HashMap<usize, InodeUpdate> {
        &self.updatables
    }

    /// Take ownership of the inode updates.
    pub fn take_updatables(self) -> HashMap<usize, InodeUpdate> {
        self.updatables
    }

    /// Get the largest file size encountered.
    pub fn largest_file(&self) -> u64 {
        self.largest_file
    }

    /// Check if any binary files were recorded.
    pub fn has_binary_files(&self) -> bool {
        self.has_binary_files
    }

    /// Get the oldest modification time.
    pub fn oldest_change(&self) -> SystemTime {
        self.oldest_change
    }

    /// Decompose into all parts.
    ///
    /// # Returns
    ///
    /// A tuple of (actions, contents, updatables).
    pub fn into_parts(
        self,
    ) -> (
        Vec<GraphOp<Option<Hash>>>,
        Vec<u8>,
        HashMap<usize, InodeUpdate>,
    ) {
        (self.actions, self.contents, self.updatables)
    }
}

/// Statistics about a recording session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordStats {
    /// Number of hunks recorded.
    pub hunk_count: usize,
    /// Total content bytes.
    pub content_bytes: usize,
    /// Number of inode updates.
    pub inode_update_count: usize,
    /// Number of inodes recorded.
    pub recorded_inode_count: usize,
    /// Largest file size encountered.
    pub largest_file: u64,
    /// Whether binary files were recorded.
    pub has_binary_files: bool,
}

impl RecordStats {
    /// Check if nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.hunk_count == 0 && self.content_bytes == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChangePosition, EdgeFlags};

    // RecordBuilder Basic Tests

    #[test]
    fn test_record_builder_new() {
        let builder = RecordBuilder::new();
        assert!(builder.is_empty());
        assert_eq!(builder.hunk_count(), 0);
        assert_eq!(builder.contents_len(), 0);
    }

    #[test]
    fn test_record_builder_with_capacity() {
        let builder = RecordBuilder::with_capacity(100, 10000);
        assert!(builder.is_empty());
    }

    #[test]
    fn test_record_builder_default() {
        let builder = RecordBuilder::default();
        assert!(builder.is_empty());
    }

    // Contents Tests

    #[test]
    fn test_append_contents() {
        let mut builder = RecordBuilder::new();

        let start1 = builder.append_contents(b"Hello");
        assert_eq!(start1, 0);
        assert_eq!(builder.contents_len(), 5);

        let start2 = builder.append_contents(b" World");
        assert_eq!(start2, 5);
        assert_eq!(builder.contents_len(), 11);

        assert_eq!(builder.contents(), b"Hello World");
    }

    #[test]
    fn test_append_empty_contents() {
        let mut builder = RecordBuilder::new();

        let start = builder.append_contents(b"");
        assert_eq!(start, 0);
        assert_eq!(builder.contents_len(), 0);
    }

    #[test]
    fn test_append_large_contents() {
        let mut builder = RecordBuilder::new();

        let large_data = vec![0u8; 1024 * 1024]; // 1MB
        let start = builder.append_contents(&large_data);

        assert_eq!(start, 0);
        assert_eq!(builder.contents_len(), 1024 * 1024);
    }

    // GraphOp Tests

    #[test]
    fn test_add_hunk() {
        let mut builder = RecordBuilder::new();

        // Create a simple file add graph_op
        let graph_op = create_test_file_add_hunk();
        let index = builder.add_hunk(graph_op);

        assert_eq!(index, 0);
        assert_eq!(builder.hunk_count(), 1);
        assert!(!builder.is_empty());
    }

    #[test]
    fn test_add_multiple_hunks() {
        let mut builder = RecordBuilder::new();

        let idx1 = builder.add_hunk(create_test_file_add_hunk());
        let idx2 = builder.add_hunk(create_test_file_add_hunk());
        let idx3 = builder.add_hunk(create_test_file_add_hunk());

        assert_eq!(idx1, 0);
        assert_eq!(idx2, 1);
        assert_eq!(idx3, 2);
        assert_eq!(builder.hunk_count(), 3);
    }

    // Inode Update Tests

    #[test]
    fn test_add_inode_update() {
        let mut builder = RecordBuilder::new();

        let hunk_index = builder.add_hunk(create_test_file_add_hunk());
        let update = InodeUpdate::add(ChangePosition::new(0), Inode::new(42));

        builder.add_inode_update(hunk_index, update);

        let recorded = builder.finish();
        assert_eq!(recorded.updatables().len(), 1);
        assert!(recorded.updatables().contains_key(&hunk_index));
    }

    #[test]
    fn test_mark_inode_recorded() {
        let mut builder = RecordBuilder::new();
        let inode = Inode::new(42);
        let position = Position::ROOT.to_option();

        assert!(!builder.is_inode_recorded(&inode));

        builder.mark_inode_recorded(inode, position);

        assert!(builder.is_inode_recorded(&inode));
        assert_eq!(builder.get_recorded_inode(&inode), Some(position));
    }

    #[test]
    fn test_get_recorded_inode_not_found() {
        let builder = RecordBuilder::new();
        let inode = Inode::new(999);

        assert!(builder.get_recorded_inode(&inode).is_none());
    }

    // File Metadata Tests

    #[test]
    fn test_update_largest_file() {
        let mut builder = RecordBuilder::new();

        assert_eq!(builder.largest_file(), 0);

        builder.update_largest_file(1000);
        assert_eq!(builder.largest_file(), 1000);

        builder.update_largest_file(500); // smaller, should not update
        assert_eq!(builder.largest_file(), 1000);

        builder.update_largest_file(2000); // larger
        assert_eq!(builder.largest_file(), 2000);
    }

    #[test]
    fn test_mark_binary_file() {
        let mut builder = RecordBuilder::new();

        assert!(!builder.has_binary_files());

        builder.mark_binary_file();

        assert!(builder.has_binary_files());
    }

    #[test]
    fn test_update_oldest_change() {
        let mut builder = RecordBuilder::new();

        assert_eq!(builder.oldest_change(), SystemTime::UNIX_EPOCH);

        let time1 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);
        builder.update_oldest_change(time1);
        assert_eq!(builder.oldest_change(), time1);

        // Older time should update
        let time2 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(500);
        builder.update_oldest_change(time2);
        assert_eq!(builder.oldest_change(), time2);

        // Newer time should not update
        let time3 = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2000);
        builder.update_oldest_change(time3);
        assert_eq!(builder.oldest_change(), time2);
    }

    // Finish Tests

    #[test]
    fn test_finish_empty() {
        let builder = RecordBuilder::new();
        let recorded = builder.finish();

        assert!(recorded.is_empty());
        assert!(recorded.is_empty_contents());
        assert_eq!(recorded.hunk_count(), 0);
    }

    #[test]
    fn test_finish_with_data() {
        let mut builder = RecordBuilder::new();

        builder.append_contents(b"test content");
        builder.add_hunk(create_test_file_add_hunk());
        builder.update_largest_file(100);
        builder.mark_binary_file();

        let recorded = builder.finish();

        assert!(!recorded.is_empty());
        assert_eq!(recorded.hunk_count(), 1);
        assert_eq!(recorded.contents(), b"test content");
        assert_eq!(recorded.largest_file(), 100);
        assert!(recorded.has_binary_files());
    }

    // Clear Tests

    #[test]
    fn test_clear() {
        let mut builder = RecordBuilder::new();

        builder.append_contents(b"test");
        builder.add_hunk(create_test_file_add_hunk());
        builder.mark_inode_recorded(Inode::new(1), Position::ROOT.to_option());
        builder.mark_binary_file();
        builder.update_largest_file(1000);

        assert!(!builder.is_empty());

        builder.clear();

        assert!(builder.is_empty());
        assert_eq!(builder.contents_len(), 0);
        assert_eq!(builder.largest_file(), 0);
        assert!(!builder.has_binary_files());
        assert!(!builder.is_inode_recorded(&Inode::new(1)));
    }

    // Stats Tests

    #[test]
    fn test_stats_empty() {
        let builder = RecordBuilder::new();
        let stats = builder.stats();

        assert!(stats.is_empty());
        assert_eq!(stats.hunk_count, 0);
        assert_eq!(stats.content_bytes, 0);
    }

    #[test]
    fn test_stats_populated() {
        let mut builder = RecordBuilder::new();

        builder.append_contents(b"test content here");
        builder.add_hunk(create_test_file_add_hunk());
        builder.add_hunk(create_test_file_add_hunk());
        builder.add_inode_update(0, InodeUpdate::add(ChangePosition::new(0), Inode::new(1)));
        builder.mark_inode_recorded(Inode::new(1), Position::ROOT.to_option());
        builder.mark_binary_file();
        builder.update_largest_file(500);

        let stats = builder.stats();

        assert!(!stats.is_empty());
        assert_eq!(stats.hunk_count, 2);
        assert_eq!(stats.content_bytes, 17);
        assert_eq!(stats.inode_update_count, 1);
        assert_eq!(stats.recorded_inode_count, 1);
        assert_eq!(stats.largest_file, 500);
        assert!(stats.has_binary_files);
    }

    // Recorded Tests

    #[test]
    fn test_recorded_is_empty() {
        let builder = RecordBuilder::new();
        let recorded = builder.finish();

        assert!(recorded.is_empty());
        assert!(recorded.is_empty_contents());
    }

    #[test]
    fn test_recorded_take_actions() {
        let mut builder = RecordBuilder::new();
        builder.add_hunk(create_test_file_add_hunk());

        let recorded = builder.finish();
        let actions = recorded.take_actions();

        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn test_recorded_take_contents() {
        let mut builder = RecordBuilder::new();
        builder.append_contents(b"hello world");

        let recorded = builder.finish();
        let contents = recorded.take_contents();

        assert_eq!(contents, b"hello world");
    }

    #[test]
    fn test_recorded_take_updatables() {
        let mut builder = RecordBuilder::new();
        builder.add_hunk(create_test_file_add_hunk());
        builder.add_inode_update(0, InodeUpdate::add(ChangePosition::new(0), Inode::new(42)));

        let recorded = builder.finish();
        let updatables = recorded.take_updatables();

        assert_eq!(updatables.len(), 1);
        assert!(updatables.contains_key(&0));
    }

    #[test]
    fn test_recorded_into_parts() {
        let mut builder = RecordBuilder::new();
        builder.append_contents(b"content");
        builder.add_hunk(create_test_file_add_hunk());
        builder.add_inode_update(0, InodeUpdate::add(ChangePosition::new(0), Inode::new(1)));

        let recorded = builder.finish();
        let (actions, contents, updatables) = recorded.into_parts();

        assert_eq!(actions.len(), 1);
        assert_eq!(contents, b"content");
        assert_eq!(updatables.len(), 1);
    }

    #[test]
    fn test_recorded_actions() {
        let mut builder = RecordBuilder::new();
        builder.add_hunk(create_test_file_add_hunk());
        builder.add_hunk(create_test_file_add_hunk());

        let recorded = builder.finish();

        assert_eq!(recorded.actions().len(), 2);
    }

    // Configuration Tests

    #[test]
    fn test_force_rediff() {
        let mut builder = RecordBuilder::new();
        assert!(!builder.force_rediff);

        builder.force_rediff = true;
        assert!(builder.force_rediff);
    }

    #[test]
    fn test_ignore_missing() {
        let mut builder = RecordBuilder::new();
        assert!(!builder.ignore_missing);

        builder.ignore_missing = true;
        assert!(builder.ignore_missing);
    }

    // Debug Format Tests

    #[test]
    fn test_record_builder_debug() {
        let builder = RecordBuilder::new();
        let debug = format!("{:?}", builder);
        assert!(debug.contains("RecordBuilder"));
    }

    #[test]
    fn test_recorded_debug() {
        let builder = RecordBuilder::new();
        let recorded = builder.finish();
        let debug = format!("{:?}", recorded);
        assert!(debug.contains("Recorded"));
    }

    #[test]
    fn test_record_stats_debug() {
        let stats = RecordStats {
            hunk_count: 1,
            content_bytes: 100,
            inode_update_count: 2,
            recorded_inode_count: 3,
            largest_file: 500,
            has_binary_files: true,
        };
        let debug = format!("{:?}", stats);
        assert!(debug.contains("RecordStats"));
    }

    // Helper Functions

    /// Create a simple test FileAdd graph_op
    fn create_test_file_add_hunk() -> GraphOp<Option<Hash>> {
        // For GraphOp<Option<Hash>>, positions use Option<Hash> as their change identifier
        // None means "this change" (the one being created)
        let inode_pos: Position<Option<Hash>> = Position::new(None, ChangePosition::new(0));
        let root_pos: Position<Option<Hash>> = Position::new(None, ChangePosition::new(0));

        GraphOp::FileAdd {
            add_name: Insertion {
                predecessors: vec![root_pos],
                successors: vec![],
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(10),
                inode: inode_pos,
            },
            add_inode: Insertion {
                predecessors: vec![],
                successors: vec![],
                flag: EdgeFlags::FOLDER | EdgeFlags::BLOCK,
                start: ChangePosition::new(10),
                end: ChangePosition::new(20),
                inode: inode_pos,
            },
            contents: None,
            path: "test.txt".to_string(),
            encoding: Some(Encoding::Utf8),
        }
    }
}

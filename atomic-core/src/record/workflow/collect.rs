//! File collection from pristine and working copy.
//!
//! This module provides functions for collecting file information from both
//! the pristine database (tracked files) and the working copy (files on disk).
//! These collections are the foundation for change detection.
//!
//! # Overview
//!
//! Change detection requires comparing two sets of files:
//!
//! 1. **Tracked files**: Files recorded in the pristine database
//! 2. **Working files**: Files present in the working copy
//!
//! By comparing these sets, we can categorize files as:
//! - **Added**: In working copy but not tracked
//! - **Deleted**: Tracked but not in working copy
//! - **Potentially Modified**: In both (requires content comparison)
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                        File Collection Flow                              │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  ┌─────────────────────┐          ┌─────────────────────┐              │
//! │  │     Pristine        │          │   Working Copy      │              │
//! │  │   (TREE table)      │          │   (filesystem)      │              │
//! │  └──────────┬──────────┘          └──────────┬──────────┘              │
//! │             │                                │                          │
//! │             ▼                                ▼                          │
//! │  ┌─────────────────────┐          ┌─────────────────────┐              │
//! │  │ collect_tracked()   │          │ collect_working()   │              │
//! │  │ → TrackedFile[]     │          │ → WorkingFile[]     │              │
//! │  └──────────┬──────────┘          └──────────┬──────────┘              │
//! │             │                                │                          │
//! │             └────────────────┬───────────────┘                          │
//! │                              │                                          │
//! │                              ▼                                          │
//! │                    ┌─────────────────────┐                              │
//! │                    │   Set Comparison    │                              │
//! │                    │   (detect_changes)  │                              │
//! │                    └─────────────────────┘                              │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::record::workflow::collect::{
//!     collect_tracked_files, collect_working_files, TrackedFile,
//! };
//!
//! // Collect tracked files from pristine
//! let tracked = collect_tracked_files(&txn, "")?;
//! println!("Found {} tracked files", tracked.len());
//!
//! // Collect files from working copy
//! let working = collect_working_files(&working_copy, "")?;
//! println!("Found {} working copy files", working.len());
//! ```

use std::collections::HashSet;
use std::time::SystemTime;

use crate::output::WorkingCopyRead;
use crate::pristine::{GraphTxnT, StackTxnT, TreeTxnT};
use crate::types::{Inode, NodeId, Position};

use super::super::error::{RecordError, RecordResult};

// TRACKED FILE

/// Information about a tracked file from the pristine database.
///
/// This structure captures what we know about a file from the TREE table
/// and associated pristine state. It's used as the "source of truth" for
/// what the repository thinks a file looks like.
///
/// # Fields
///
/// - `path`: The file's path relative to repository root
/// - `inode`: Stable identifier that survives renames
/// - `position`: Location in the graph (identifies content)
/// - `is_directory`: Whether this is a directory entry
///
/// # Example
///
/// ```rust,ignore
/// let tracked = TrackedFile::new(
///     "src/main.rs",
///     Inode::new(42),
///     Position::new(change_id, offset),
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedFile {
    /// Path relative to repository root.
    pub path: String,

    /// Stable file identifier.
    pub inode: Inode,

    /// Position in the graph where file content lives.
    pub position: Position<NodeId>,

    /// Whether this is a directory.
    pub is_directory: bool,
}

impl TrackedFile {
    /// Create a new tracked file entry.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to repository root
    /// * `inode` - Stable file identifier
    /// * `position` - Graph position
    ///
    /// # Returns
    ///
    /// A new `TrackedFile` (defaults to not a directory).
    pub fn new(path: impl Into<String>, inode: Inode, position: Position<NodeId>) -> Self {
        Self {
            path: path.into(),
            inode,
            position,
            is_directory: false,
        }
    }

    /// Mark this entry as a directory.
    pub fn as_directory(mut self) -> Self {
        self.is_directory = true;
        self
    }
}

// WORKING FILE

/// Information about a file in the working copy.
///
/// This structure captures what we observe about a file on disk.
/// It's used for comparison against tracked files.
///
/// # Fields
///
/// - `path`: The file's path relative to working copy root
/// - `is_directory`: Whether this is a directory
/// - `size`: File size in bytes (for quick change detection)
/// - `mtime`: Last modification time (for mtime optimization)
///
/// # Example
///
/// ```rust,ignore
/// let working = WorkingFile::new("src/main.rs")
///     .with_size(1024)
///     .with_mtime(SystemTime::now());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingFile {
    /// Path relative to working copy root.
    pub path: String,

    /// Whether this is a directory.
    pub is_directory: bool,

    /// File size in bytes.
    pub size: Option<u64>,

    /// Last modification time.
    pub mtime: Option<SystemTime>,
}

impl WorkingFile {
    /// Create a new working file entry.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to working copy root
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            is_directory: false,
            size: None,
            mtime: None,
        }
    }

    /// Mark this entry as a directory.
    pub fn as_directory(mut self) -> Self {
        self.is_directory = true;
        self
    }

    /// Set the file size.
    pub fn with_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// Set the modification time.
    pub fn with_mtime(mut self, mtime: SystemTime) -> Self {
        self.mtime = Some(mtime);
        self
    }
}

// COLLECTION RESULT

/// Result of collecting files from a source.
///
/// This structure holds both the collected files and any errors
/// encountered during collection (allowing partial results).
#[derive(Debug, Clone)]
pub struct CollectionResult<T> {
    /// Successfully collected files.
    pub files: Vec<T>,

    /// Paths that couldn't be collected (with error messages).
    pub errors: Vec<(String, String)>,

    /// Number of files skipped (e.g., ignored patterns).
    pub skipped: usize,
}

impl<T> CollectionResult<T> {
    /// Create a new empty collection result.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            errors: Vec::new(),
            skipped: 0,
        }
    }

    /// Create a result with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            files: Vec::with_capacity(capacity),
            errors: Vec::new(),
            skipped: 0,
        }
    }

    /// Add a successfully collected file.
    pub fn add(&mut self, file: T) {
        self.files.push(file);
    }

    /// Record an error for a path.
    pub fn add_error(&mut self, path: impl Into<String>, error: impl Into<String>) {
        self.errors.push((path.into(), error.into()));
    }

    /// Record a skipped file.
    pub fn add_skipped(&mut self) {
        self.skipped += 1;
    }

    /// Check if collection was successful (no errors).
    pub fn is_success(&self) -> bool {
        self.errors.is_empty()
    }

    /// Get the number of collected files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Check if no files were collected.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Get paths of all collected files.
    pub fn paths(&self) -> impl Iterator<Item = &str>
    where
        T: AsRef<str>,
    {
        self.files.iter().map(|f| f.as_ref())
    }

    /// Convert to a set of paths for efficient lookup.
    pub fn path_set(&self) -> HashSet<&str>
    where
        T: HasPath,
    {
        self.files.iter().map(|f| f.path()).collect()
    }
}

impl<T> Default for CollectionResult<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait for types that have a path.
pub trait HasPath {
    /// Get the path of this item.
    fn path(&self) -> &str;
}

impl HasPath for TrackedFile {
    fn path(&self) -> &str {
        &self.path
    }
}

impl HasPath for WorkingFile {
    fn path(&self) -> &str {
        &self.path
    }
}

impl HasPath for String {
    fn path(&self) -> &str {
        self
    }
}

impl HasPath for &str {
    fn path(&self) -> &str {
        self
    }
}

// COLLECTION FUNCTIONS

/// Collect tracked files from the pristine database.
///
/// Iterates over the TREE table to find all files tracked by the repository.
/// Results can be filtered by path prefix.
///
/// # Arguments
///
/// * `txn` - Read transaction for pristine database
/// * `prefix` - Path prefix filter (empty = all files)
///
/// # Returns
///
/// A `CollectionResult<TrackedFile>` containing all tracked files.
///
/// # Errors
///
/// Returns `RecordError::Pristine` if database access fails.
///
/// # Example
///
/// ```rust,ignore
/// // Collect all tracked files
/// let result = collect_tracked_files(&txn, "")?;
///
/// // Collect only src/ files
/// let result = collect_tracked_files(&txn, "src/")?;
/// ```
pub fn collect_tracked_files<T>(
    txn: &T,
    prefix: &str,
) -> RecordResult<CollectionResult<TrackedFile>>
where
    T: GraphTxnT + TreeTxnT + StackTxnT,
{
    let mut result = CollectionResult::new();

    // Iterate over all tree entries (trait doesn't support prefix filtering)
    let iter = txn
        .iter_tree()
        .map_err(|e| RecordError::Pristine(Box::new(e)))?;

    for item in iter {
        match item {
            Ok((path, inode)) => {
                // Filter by prefix if specified
                if !prefix.is_empty() && !path.starts_with(prefix) {
                    result.add_skipped();
                    continue;
                }

                // Get the position for this inode
                match txn.inode_position(inode) {
                    Ok(Some(position)) => {
                        let file = TrackedFile::new(&path, inode, position);
                        result.add(file);
                    }
                    Ok(None) => {
                        // Inode exists but has no position - might be a bug or deleted
                        result.add_error(&path, "Inode has no graph position");
                    }
                    Err(e) => {
                        result.add_error(&path, format!("Failed to get position: {}", e));
                    }
                }
            }
            Err(e) => {
                result.add_error("<iteration error>", format!("Tree iteration failed: {}", e));
            }
        }
    }

    Ok(result)
}

/// Collect files from the working copy.
///
/// Scans the working copy to find all files present on disk.
/// This is the complement to `collect_tracked_files` for detecting
/// added/deleted files.
///
/// # Arguments
///
/// * `working_copy` - Working copy interface
/// * `prefix` - Path prefix filter (empty = all files)
///
/// # Returns
///
/// A `CollectionResult<WorkingFile>` containing all working copy files.
///
/// # Behavior
///
/// - Walks the working copy directory tree using the `walk_files` trait method
/// - Filters by prefix if provided
/// - Excludes the `.atomic` directory automatically
/// - Collects metadata (size, mtime) for each file when available
///
/// # Example
///
/// ```rust,ignore
/// let result = collect_working_files(&working_copy, "")?;
/// for file in &result.files {
///     println!("Found: {} (size: {:?})", file.path, file.size);
/// }
/// ```
pub fn collect_working_files<W>(
    working_copy: &W,
    prefix: &str,
) -> RecordResult<CollectionResult<WorkingFile>>
where
    W: WorkingCopyRead,
{
    let mut result = CollectionResult::new();

    // Walk the working copy directory tree
    let paths = working_copy
        .walk_files(prefix)
        .map_err(|e| RecordError::Io(std::io::Error::other(e.to_string())))?;

    for path in paths {
        let mut file = WorkingFile::new(&path);

        // Check if it's a directory
        if working_copy.is_directory(&path) {
            file = file.as_directory();
        }

        // Try to get mtime
        if let Ok(mtime) = working_copy.modified_time(&path) {
            file = file.with_mtime(mtime);
        }

        result.add(file);
    }

    Ok(result)
}

/// Collect working copy state for a specific set of paths.
///
/// This is more efficient than full collection when you already know
/// which paths to check (e.g., from a file watcher).
///
/// # Arguments
///
/// * `working_copy` - Working copy interface
/// * `paths` - Specific paths to check
///
/// # Returns
///
/// A `CollectionResult<WorkingFile>` for the specified paths.
///
/// # Example
///
/// ```rust,ignore
/// let paths = vec!["src/main.rs", "src/lib.rs"];
/// let result = collect_working_paths(&working_copy, &paths)?;
/// ```
pub fn collect_working_paths<'a, W, I>(
    working_copy: &W,
    paths: I,
) -> RecordResult<CollectionResult<WorkingFile>>
where
    W: WorkingCopyRead,
    I: IntoIterator<Item = &'a str>,
{
    let mut result = CollectionResult::new();

    for path in paths {
        // Check if file exists
        if !working_copy.exists(path) {
            continue;
        }

        let mut file = WorkingFile::new(path);

        // Check if it's a directory
        if working_copy.is_directory(path) {
            file = file.as_directory();
        }

        // Try to get mtime
        if let Ok(mtime) = working_copy.modified_time(path) {
            file = file.with_mtime(mtime);
        }

        result.add(file);
    }

    Ok(result)
}

/// Get the pristine state for a single file.
///
/// Looks up a file's tracked state in the pristine database.
///
/// # Arguments
///
/// * `txn` - Read transaction for pristine database
/// * `path` - Path to look up
///
/// # Returns
///
/// `Some(TrackedFile)` if the file is tracked, `None` otherwise.
///
/// # Example
///
/// ```rust,ignore
/// if let Some(tracked) = get_tracked_file(&txn, "src/main.rs")? {
///     println!("File {} has inode {:?}", tracked.path, tracked.inode);
/// }
/// ```
pub fn get_tracked_file<T>(txn: &T, path: &str) -> RecordResult<Option<TrackedFile>>
where
    T: GraphTxnT + TreeTxnT + StackTxnT,
{
    // Look up the inode for this path
    let inode = match txn
        .get_inode(path)
        .map_err(|e| RecordError::Pristine(Box::new(e)))?
    {
        Some(inode) => inode,
        None => return Ok(None),
    };

    // Get the position for this inode
    let position = match txn
        .inode_position(inode)
        .map_err(|e| RecordError::Pristine(Box::new(e)))?
    {
        Some(pos) => pos,
        None => return Ok(None),
    };

    Ok(Some(TrackedFile::new(path, inode, position)))
}

/// Get the working copy state for a single file.
///
/// Checks if a file exists in the working copy and collects its metadata.
///
/// # Arguments
///
/// * `working_copy` - Working copy interface
/// * `path` - Path to check
///
/// # Returns
///
/// `Some(WorkingFile)` if the file exists, `None` otherwise.
///
/// # Example
///
/// ```rust,ignore
/// if let Some(working) = get_working_file(&working_copy, "src/main.rs")? {
///     println!("File exists with size {:?}", working.size);
/// }
/// ```
pub fn get_working_file<W>(working_copy: &W, path: &str) -> RecordResult<Option<WorkingFile>>
where
    W: WorkingCopyRead,
{
    if !working_copy.exists(path) {
        return Ok(None);
    }

    let mut file = WorkingFile::new(path);

    if working_copy.is_directory(path) {
        file = file.as_directory();
    }

    if let Ok(mtime) = working_copy.modified_time(path) {
        file = file.with_mtime(mtime);
    }

    Ok(Some(file))
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Memory;
    use crate::types::ChangePosition;

    // TrackedFile Tests

    #[test]
    fn test_tracked_file_new() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
        let file = TrackedFile::new("src/main.rs", Inode::new(42), pos);

        assert_eq!(file.path, "src/main.rs");
        assert_eq!(file.inode, Inode::new(42));
        assert_eq!(file.position, pos);
        assert!(!file.is_directory);
    }

    #[test]
    fn test_tracked_file_as_directory() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
        let file = TrackedFile::new("src/", Inode::new(1), pos).as_directory();

        assert!(file.is_directory);
    }

    #[test]
    fn test_tracked_file_clone() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
        let file = TrackedFile::new("test.txt", Inode::new(10), pos);
        let cloned = file.clone();

        assert_eq!(file, cloned);
    }

    #[test]
    fn test_tracked_file_debug() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
        let file = TrackedFile::new("test.txt", Inode::new(1), pos);
        let debug = format!("{:?}", file);

        assert!(debug.contains("TrackedFile"));
        assert!(debug.contains("test.txt"));
    }

    // WorkingFile Tests

    #[test]
    fn test_working_file_new() {
        let file = WorkingFile::new("src/main.rs");

        assert_eq!(file.path, "src/main.rs");
        assert!(!file.is_directory);
        assert!(file.size.is_none());
        assert!(file.mtime.is_none());
    }

    #[test]
    fn test_working_file_as_directory() {
        let file = WorkingFile::new("src/").as_directory();

        assert!(file.is_directory);
    }

    #[test]
    fn test_working_file_with_size() {
        let file = WorkingFile::new("test.txt").with_size(1024);

        assert_eq!(file.size, Some(1024));
    }

    #[test]
    fn test_working_file_with_mtime() {
        let now = SystemTime::now();
        let file = WorkingFile::new("test.txt").with_mtime(now);

        assert_eq!(file.mtime, Some(now));
    }

    #[test]
    fn test_working_file_builder_chain() {
        let now = SystemTime::now();
        let file = WorkingFile::new("large.bin")
            .with_size(1024 * 1024)
            .with_mtime(now)
            .as_directory();

        assert_eq!(file.path, "large.bin");
        assert_eq!(file.size, Some(1024 * 1024));
        assert_eq!(file.mtime, Some(now));
        assert!(file.is_directory);
    }

    // CollectionResult Tests

    #[test]
    fn test_collection_result_new() {
        let result: CollectionResult<TrackedFile> = CollectionResult::new();

        assert!(result.is_empty());
        assert_eq!(result.len(), 0);
        assert!(result.is_success());
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn test_collection_result_add() {
        let mut result = CollectionResult::new();
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
        let file = TrackedFile::new("test.txt", Inode::new(1), pos);

        result.add(file);

        assert_eq!(result.len(), 1);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_collection_result_add_error() {
        let mut result: CollectionResult<TrackedFile> = CollectionResult::new();

        result.add_error("bad/path.txt", "Something went wrong");

        assert!(!result.is_success());
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].0, "bad/path.txt");
    }

    #[test]
    fn test_collection_result_add_skipped() {
        let mut result: CollectionResult<TrackedFile> = CollectionResult::new();

        result.add_skipped();
        result.add_skipped();

        assert_eq!(result.skipped, 2);
    }

    #[test]
    fn test_collection_result_path_set() {
        let mut result = CollectionResult::new();
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));

        result.add(TrackedFile::new("a.txt", Inode::new(1), pos));
        result.add(TrackedFile::new("b.txt", Inode::new(2), pos));

        let paths = result.path_set();

        assert!(paths.contains("a.txt"));
        assert!(paths.contains("b.txt"));
        assert!(!paths.contains("c.txt"));
    }

    #[test]
    fn test_collection_result_default() {
        let result: CollectionResult<String> = Default::default();

        assert!(result.is_empty());
    }

    // HasPath Trait Tests

    #[test]
    fn test_has_path_tracked_file() {
        let pos = Position::new(NodeId::new(1), ChangePosition::new(0));
        let file = TrackedFile::new("my/path.txt", Inode::new(1), pos);

        assert_eq!(file.path(), "my/path.txt");
    }

    #[test]
    fn test_has_path_working_file() {
        let file = WorkingFile::new("another/path.rs");

        assert_eq!(file.path(), "another/path.rs");
    }

    #[test]
    fn test_has_path_string() {
        let s = String::from("string/path");
        assert_eq!(s.path(), "string/path");
    }

    #[test]
    fn test_has_path_str() {
        let s = "str/path";
        assert_eq!(s.path(), "str/path");
    }

    // collect_working_paths Tests

    #[test]
    fn test_collect_working_paths_empty() {
        let working_copy = Memory::new();
        let paths: Vec<&str> = vec![];

        let result = collect_working_paths(&working_copy, paths).unwrap();

        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_working_paths_existing() {
        let mut working_copy = Memory::new();
        working_copy.add_file("exists.txt", b"content");

        let paths = vec!["exists.txt", "missing.txt"];
        let result = collect_working_paths(&working_copy, paths).unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result.files[0].path, "exists.txt");
    }

    #[test]
    fn test_collect_working_paths_multiple() {
        let mut working_copy = Memory::new();
        working_copy.add_file("a.txt", b"a");
        working_copy.add_file("b.txt", b"b");
        working_copy.add_file("c.txt", b"c");

        let paths = vec!["a.txt", "b.txt", "c.txt"];
        let result = collect_working_paths(&working_copy, paths).unwrap();

        assert_eq!(result.len(), 3);
    }

    // get_working_file Tests

    #[test]
    fn test_get_working_file_exists() {
        let mut working_copy = Memory::new();
        working_copy.add_file("found.txt", b"hello");

        let result = get_working_file(&working_copy, "found.txt").unwrap();

        assert!(result.is_some());
        let file = result.unwrap();
        assert_eq!(file.path, "found.txt");
        assert!(!file.is_directory);
    }

    #[test]
    fn test_get_working_file_not_found() {
        let working_copy = Memory::new();

        let result = get_working_file(&working_copy, "missing.txt").unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn test_get_working_file_directory() {
        let mut working_copy = Memory::new();
        working_copy.add_directory("mydir");

        let result = get_working_file(&working_copy, "mydir").unwrap();

        assert!(result.is_some());
        let file = result.unwrap();
        assert!(file.is_directory);
    }

    // collect_working_files tests

    #[test]
    fn test_collect_working_files_empty() {
        let working_copy = Memory::new();

        let result = collect_working_files(&working_copy, "").unwrap();

        assert!(result.is_empty());
        assert!(result.is_success());
    }

    #[test]
    fn test_collect_working_files_single_file() {
        let working_copy = Memory::new();
        working_copy.add_file("test.txt", b"hello");

        let result = collect_working_files(&working_copy, "").unwrap();

        assert_eq!(result.len(), 1);
        assert!(result.files.iter().any(|f| f.path == "test.txt"));
    }

    #[test]
    fn test_collect_working_files_multiple_files() {
        let working_copy = Memory::new();
        working_copy.add_file("a.txt", b"a");
        working_copy.add_file("b.txt", b"b");
        working_copy.add_file("c.txt", b"c");

        let result = collect_working_files(&working_copy, "").unwrap();

        assert_eq!(result.len(), 3);
        // Results should be sorted
        assert_eq!(result.files[0].path, "a.txt");
        assert_eq!(result.files[1].path, "b.txt");
        assert_eq!(result.files[2].path, "c.txt");
    }

    #[test]
    fn test_collect_working_files_nested() {
        let working_copy = Memory::new();
        working_copy.add_file("root.txt", b"root");
        working_copy.add_file("src/main.rs", b"main");
        working_copy.add_file("src/lib.rs", b"lib");
        working_copy.add_file("src/utils/helper.rs", b"helper");

        let result = collect_working_files(&working_copy, "").unwrap();

        assert_eq!(result.len(), 4);
        let paths: Vec<&str> = result.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"root.txt"));
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"src/lib.rs"));
        assert!(paths.contains(&"src/utils/helper.rs"));
    }

    #[test]
    fn test_collect_working_files_with_prefix() {
        let working_copy = Memory::new();
        working_copy.add_file("root.txt", b"root");
        working_copy.add_file("src/main.rs", b"main");
        working_copy.add_file("src/lib.rs", b"lib");
        working_copy.add_file("tests/test.rs", b"test");

        let result = collect_working_files(&working_copy, "src").unwrap();

        assert_eq!(result.len(), 2);
        let paths: Vec<&str> = result.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"src/lib.rs"));
        assert!(!paths.contains(&"root.txt"));
        assert!(!paths.contains(&"tests/test.rs"));
    }

    #[test]
    fn test_collect_working_files_excludes_atomic_dir() {
        let working_copy = Memory::new();
        working_copy.add_file("file.txt", b"content");
        working_copy.add_file(".atomic/pristine", b"data");
        working_copy.add_file(".atomic/config.toml", b"config");

        let result = collect_working_files(&working_copy, "").unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result.files[0].path, "file.txt");
    }

    #[test]
    fn test_collect_working_files_excludes_directories() {
        let mut working_copy = Memory::new();
        working_copy.add_file("file.txt", b"content");
        working_copy.add_directory("empty_dir");

        let result = collect_working_files(&working_copy, "").unwrap();

        // Only files should be included, not directories
        assert_eq!(result.len(), 1);
        assert_eq!(result.files[0].path, "file.txt");
        assert!(!result.files[0].is_directory);
    }

    #[test]
    fn test_collect_working_files_collects_mtime() {
        let working_copy = Memory::new();
        working_copy.add_file("file.txt", b"content");

        let result = collect_working_files(&working_copy, "").unwrap();

        assert_eq!(result.len(), 1);
        // Memory working copy provides mtime
        assert!(result.files[0].mtime.is_some());
    }
}

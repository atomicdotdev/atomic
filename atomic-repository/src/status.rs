//! Repository status tracking for Atomic VCS
//!
//! This module provides functionality to determine the status of files in the
//! working copy relative to the repository's recorded state. It's analogous to
//! `git status` but designed for Atomic's graph-based architecture.
//!
//! # Overview
//!
//! The status module compares three sources of truth:
//!
//! 1. **Working Copy**: The actual files on disk
//! 2. **Tree Tables**: The recorded file structure in the pristine database
//! 3. **Graph State**: The content state represented in the repository graph
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                        Status Detection Flow                         │
//! ├─────────────────────────────────────────────────────────────────────┤
//! │                                                                     │
//! │  Working Copy           Tree Tables            Graph State          │
//! │  (Filesystem)           (Pristine DB)          (Content Hash)       │
//! │       │                      │                      │               │
//! │       └──────────┬───────────┴──────────────────────┘               │
//! │                  │                                                  │
//! │                  ▼                                                  │
//! │           ┌──────────────┐                                          │
//! │           │   Compare    │                                          │
//! │           └──────────────┘                                          │
//! │                  │                                                  │
//! │     ┌────────────┼────────────┬─────────────┐                       │
//! │     ▼            ▼            ▼             ▼                       │
//! │  Modified    Untracked    Deleted      Conflicted                   │
//! │                                                                     │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # File Status Categories
//!
//! | Status | On Disk | In Tree | Content Match |
//! |--------|---------|---------|---------------|
//! | Clean | ✓ | ✓ | ✓ |
//! | Modified | ✓ | ✓ | ✗ |
//! | Deleted | ✗ | ✓ | N/A |
//! | Untracked | ✓ | ✗ | N/A |
//! | Added | ✓ | ✓ (new) | N/A |
//! | Conflicted | ✓ | ✓ | Has conflicts |
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_repository::{Repository, RepositoryStatus};
//!
//! let repo = Repository::open(".")?;
//! let status = repo.status()?;
//!
//! println!("On stack: {}", status.stack());
//! println!("Modified files: {}", status.modified_count());
//!
//! for entry in status.modified() {
//!     println!("  M {}", entry.path().display());
//! }
//!
//! for entry in status.untracked() {
//!     println!("  ? {}", entry.path().display());
//! }
//! ```
//!
//! # Performance Considerations
//!
//! Status computation can be expensive for large repositories because it
//! requires:
//!
//! 1. Walking the entire working copy directory tree
//! 2. Reading file metadata and potentially content
//! 3. Querying the tree tables in the database
//!
//! For large repositories, consider:
//! - Using path filters to limit the scope
//! - Caching inode/mtime for unchanged file detection (future optimization)
//! - Running status incrementally on specific directories

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use atomic_core::types::{Hash, Inode, Merkle};
use thiserror::Error;

use crate::ignore::IgnoreRules;

// Constants

/// Patterns that are always ignored (internal directories)
const ALWAYS_IGNORED: &[&str] = &[".atomic", ".git"];

// Error Types

/// Result type for status operations.
pub type StatusResult<T> = Result<T, StatusError>;

/// Errors that can occur during status computation.
#[derive(Debug, Error)]
pub enum StatusError {
    /// An I/O error occurred while reading the working copy.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A database error occurred while querying the tree tables.
    #[error("Database error: {0}")]
    Database(String),

    /// The repository is in an invalid state.
    #[error("Invalid repository state: {0}")]
    InvalidState(String),

    /// A path could not be processed.
    #[error("Path error: {path}")]
    PathError {
        /// The problematic path
        path: String,
        /// Details about the error
        details: String,
    },

    /// Directory traversal error.
    #[error("Directory traversal error: {0}")]
    WalkDir(#[from] walkdir::Error),
}

// FileStatus Enum

/// The status of a file in the repository.
///
/// This enum represents all possible states a file can be in relative to
/// the repository's recorded state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileStatus {
    /// File is tracked and unchanged from the recorded state.
    ///
    /// The file exists on disk, is tracked in the tree tables, and its
    /// content matches the recorded content hash.
    Clean,

    /// File is tracked but has local modifications.
    ///
    /// The file exists on disk and is tracked, but its content differs
    /// from the recorded content hash.
    Modified,

    /// File was tracked but has been deleted from the working copy.
    ///
    /// The file is recorded in the tree tables but no longer exists on disk.
    Deleted,

    /// File exists on disk but is not tracked by the repository.
    ///
    /// This is a new file that hasn't been added to version control yet.
    Untracked,

    /// File has been added to tracking but not yet recorded in a change.
    ///
    /// This represents a file that has been explicitly added but the change
    /// hasn't been committed yet.
    Added,

    /// File has unresolved merge conflicts.
    ///
    /// The file contains conflict markers or is marked as conflicted in
    /// the repository state.
    Conflicted,

    /// File type changed (e.g., file to directory or vice versa).
    ///
    /// This is a special case where the path exists but the type doesn't
    /// match what's recorded.
    TypeChanged,

    /// File permissions changed (on systems that track permissions).
    PermissionsChanged,
}

impl FileStatus {
    /// Check if this status represents a change that needs to be recorded.
    ///
    /// Returns `true` for statuses that indicate uncommitted changes.
    pub fn is_dirty(&self) -> bool {
        matches!(
            self,
            FileStatus::Modified
                | FileStatus::Deleted
                | FileStatus::Added
                | FileStatus::TypeChanged
                | FileStatus::PermissionsChanged
        )
    }

    /// Check if this status represents an untracked file.
    pub fn is_untracked(&self) -> bool {
        matches!(self, FileStatus::Untracked)
    }

    /// Check if this status represents a conflict.
    pub fn is_conflicted(&self) -> bool {
        matches!(self, FileStatus::Conflicted)
    }

    /// Check if this status represents a clean (unchanged) file.
    pub fn is_clean(&self) -> bool {
        matches!(self, FileStatus::Clean)
    }

    /// Get a short code for this status (for display).
    ///
    /// Returns a single character commonly used in VCS status displays:
    /// - ` ` (space) for Clean
    /// - `M` for Modified
    /// - `D` for Deleted
    /// - `?` for Untracked
    /// - `A` for Added
    /// - `C` for Conflicted
    /// - `T` for TypeChanged
    /// - `P` for PermissionsChanged
    pub fn short_code(&self) -> char {
        match self {
            FileStatus::Clean => ' ',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Untracked => '?',
            FileStatus::Added => 'A',
            FileStatus::Conflicted => 'C',
            FileStatus::TypeChanged => 'T',
            FileStatus::PermissionsChanged => 'P',
        }
    }

    /// Get a human-readable description of this status.
    pub fn description(&self) -> &'static str {
        match self {
            FileStatus::Clean => "unchanged",
            FileStatus::Modified => "modified",
            FileStatus::Deleted => "deleted",
            FileStatus::Untracked => "untracked",
            FileStatus::Added => "added",
            FileStatus::Conflicted => "conflicted",
            FileStatus::TypeChanged => "type changed",
            FileStatus::PermissionsChanged => "permissions changed",
        }
    }
}

impl std::fmt::Display for FileStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description())
    }
}

// FileStatusEntry

/// Information about a single file's status.
///
/// This struct contains the path, status, and optional metadata about
/// a file in the repository.
#[derive(Debug, Clone)]
pub struct FileStatusEntry {
    /// Path relative to the repository root.
    path: PathBuf,

    /// Current status of the file.
    status: FileStatus,

    /// Optional inode for tracked files.
    ///
    /// This is `Some` for files that are tracked in the repository,
    /// and `None` for untracked files.
    inode: Option<Inode>,

    /// Optional content hash for the recorded state.
    ///
    /// This is `Some` for tracked files with recorded content,
    /// and `None` for untracked or deleted files.
    recorded_hash: Option<Hash>,

    /// Optional current content hash.
    ///
    /// This is `Some` for files that exist on disk and have been hashed,
    /// and `None` for deleted files or files not yet hashed.
    current_hash: Option<Hash>,

    /// Additional details about the status.
    ///
    /// This may contain information like conflict type, permission details,
    /// or other status-specific information.
    details: Option<String>,
}

impl FileStatusEntry {
    /// Create a new file status entry.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to repository root
    /// * `status` - The file's status
    pub fn new(path: PathBuf, status: FileStatus) -> Self {
        Self {
            path,
            status,
            inode: None,
            recorded_hash: None,
            current_hash: None,
            details: None,
        }
    }

    /// Create a new entry with full details.
    pub fn with_details(
        path: PathBuf,
        status: FileStatus,
        inode: Option<Inode>,
        recorded_hash: Option<Hash>,
        current_hash: Option<Hash>,
        details: Option<String>,
    ) -> Self {
        Self {
            path,
            status,
            inode,
            recorded_hash,
            current_hash,
            details,
        }
    }

    /// Get the file path relative to the repository root.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the file's status.
    pub fn status(&self) -> FileStatus {
        self.status
    }

    /// Get the inode if the file is tracked.
    pub fn inode(&self) -> Option<Inode> {
        self.inode
    }

    /// Get the recorded content hash if available.
    pub fn recorded_hash(&self) -> Option<&Hash> {
        self.recorded_hash.as_ref()
    }

    /// Get the current content hash if available.
    pub fn current_hash(&self) -> Option<&Hash> {
        self.current_hash.as_ref()
    }

    /// Get additional details about the status.
    pub fn details(&self) -> Option<&str> {
        self.details.as_deref()
    }

    /// Set the inode.
    pub fn set_inode(&mut self, inode: Inode) {
        self.inode = Some(inode);
    }

    /// Set the recorded content hash.
    pub fn set_recorded_hash(&mut self, hash: Hash) {
        self.recorded_hash = Some(hash);
    }

    /// Set the current content hash.
    pub fn set_current_hash(&mut self, hash: Hash) {
        self.current_hash = Some(hash);
    }

    /// Set additional details.
    pub fn set_details(&mut self, details: String) {
        self.details = Some(details);
    }
}

// RepositoryStatus

/// Complete status of a repository.
///
/// This struct aggregates the status of all files in the repository,
/// providing convenient access to files by status category.
#[derive(Debug, Clone)]
pub struct RepositoryStatus {
    /// The current stack name.
    stack: String,

    /// The Merkle state of the current stack (if any changes have been applied).
    state: Option<Merkle>,

    /// All file status entries.
    entries: Vec<FileStatusEntry>,

    /// Index of entries by path for quick lookup.
    path_index: HashMap<PathBuf, usize>,
}

impl RepositoryStatus {
    /// Create a new empty repository status.
    ///
    /// # Arguments
    ///
    /// * `stack` - The current stack name
    /// * `state` - The Merkle state of the stack (if any)
    pub fn new(stack: String, state: Option<Merkle>) -> Self {
        Self {
            stack,
            state,
            entries: Vec::new(),
            path_index: HashMap::new(),
        }
    }

    /// Create a repository status with pre-allocated capacity.
    ///
    /// # Arguments
    ///
    /// * `stack` - The current stack name
    /// * `state` - The Merkle state of the stack
    /// * `capacity` - Expected number of entries
    pub fn with_capacity(stack: String, state: Option<Merkle>, capacity: usize) -> Self {
        Self {
            stack,
            state,
            entries: Vec::with_capacity(capacity),
            path_index: HashMap::with_capacity(capacity),
        }
    }

    /// Get the current stack name.
    pub fn stack(&self) -> &str {
        &self.stack
    }

    /// Get the Merkle state of the current stack.
    pub fn state(&self) -> Option<&Merkle> {
        self.state.as_ref()
    }

    /// Add a file status entry.
    pub fn add_entry(&mut self, entry: FileStatusEntry) {
        let index = self.entries.len();
        self.path_index.insert(entry.path.clone(), index);
        self.entries.push(entry);
    }

    /// Get the status entry for a specific path.
    pub fn get(&self, path: &Path) -> Option<&FileStatusEntry> {
        self.path_index.get(path).map(|&i| &self.entries[i])
    }

    /// Get all entries.
    pub fn entries(&self) -> &[FileStatusEntry] {
        &self.entries
    }

    /// Get the total number of files in the status.
    pub fn total_count(&self) -> usize {
        self.entries.len()
    }

    /// Check if the working copy is clean (no uncommitted changes).
    pub fn is_clean(&self) -> bool {
        !self.entries.iter().any(|e| e.status.is_dirty())
    }

    /// Check if there are any conflicts.
    pub fn has_conflicts(&self) -> bool {
        self.entries.iter().any(|e| e.status.is_conflicted())
    }

    /// Check if there are any untracked files.
    pub fn has_untracked(&self) -> bool {
        self.entries.iter().any(|e| e.status.is_untracked())
    }

    // Filtered Iterators

    /// Iterate over modified files.
    pub fn modified(&self) -> impl Iterator<Item = &FileStatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == FileStatus::Modified)
    }

    /// Count of modified files.
    pub fn modified_count(&self) -> usize {
        self.modified().count()
    }

    /// Iterate over deleted files.
    pub fn deleted(&self) -> impl Iterator<Item = &FileStatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == FileStatus::Deleted)
    }

    /// Count of deleted files.
    pub fn deleted_count(&self) -> usize {
        self.deleted().count()
    }

    /// Iterate over untracked files.
    pub fn untracked(&self) -> impl Iterator<Item = &FileStatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == FileStatus::Untracked)
    }

    /// Count of untracked files.
    pub fn untracked_count(&self) -> usize {
        self.untracked().count()
    }

    /// Iterate over added files.
    pub fn added(&self) -> impl Iterator<Item = &FileStatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == FileStatus::Added)
    }

    /// Count of added files.
    pub fn added_count(&self) -> usize {
        self.added().count()
    }

    /// Iterate over conflicted files.
    pub fn conflicted(&self) -> impl Iterator<Item = &FileStatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == FileStatus::Conflicted)
    }

    /// Count of conflicted files.
    pub fn conflicted_count(&self) -> usize {
        self.conflicted().count()
    }

    /// Iterate over clean files.
    pub fn clean(&self) -> impl Iterator<Item = &FileStatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == FileStatus::Clean)
    }

    /// Count of clean files.
    pub fn clean_count(&self) -> usize {
        self.clean().count()
    }

    /// Iterate over all dirty (changed) files.
    pub fn dirty(&self) -> impl Iterator<Item = &FileStatusEntry> {
        self.entries.iter().filter(|e| e.status.is_dirty())
    }

    /// Count of dirty files.
    pub fn dirty_count(&self) -> usize {
        self.dirty().count()
    }

    // Path Collections

    /// Get all paths with a specific status.
    pub fn paths_with_status(&self, status: FileStatus) -> Vec<&Path> {
        self.entries
            .iter()
            .filter(|e| e.status == status)
            .map(|e| e.path())
            .collect()
    }

    /// Get all tracked file paths (not untracked).
    pub fn tracked_paths(&self) -> Vec<&Path> {
        self.entries
            .iter()
            .filter(|e| !e.status.is_untracked())
            .map(|e| e.path())
            .collect()
    }
}

impl Default for RepositoryStatus {
    fn default() -> Self {
        Self::new(String::new(), None)
    }
}

// StatusOptions

/// Options for controlling status computation.
///
/// These options allow customizing which files are included in the status
/// and how the computation is performed.
#[derive(Debug, Clone)]
pub struct StatusOptions {
    /// Include untracked files in the status.
    ///
    /// When `false`, only tracked files are included.
    /// Default: `true`
    pub include_untracked: bool,

    /// Include ignored files in the status (as untracked).
    ///
    /// When `false`, files matching ignore patterns are excluded.
    /// Default: `false`
    pub include_ignored: bool,

    /// Only check paths matching these patterns.
    ///
    /// If empty, all paths are checked.
    pub path_filters: Vec<PathBuf>,

    /// Respect .gitignore and similar ignore files.
    ///
    /// Default: `true`
    pub respect_ignore_files: bool,

    /// Hash file contents to detect modifications.
    ///
    /// When `false`, only uses filesystem metadata (faster but less accurate).
    /// Default: `true`
    pub hash_contents: bool,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self {
            include_untracked: true,
            include_ignored: false,
            path_filters: Vec::new(),
            respect_ignore_files: true,
            hash_contents: true,
        }
    }
}

impl StatusOptions {
    /// Create options that only include tracked files.
    pub fn tracked_only() -> Self {
        Self {
            include_untracked: false,
            ..Default::default()
        }
    }

    /// Create options that include everything.
    pub fn all() -> Self {
        Self {
            include_untracked: true,
            include_ignored: true,
            ..Default::default()
        }
    }

    /// Create options for fast status (metadata only, no content hashing).
    pub fn fast() -> Self {
        Self {
            hash_contents: false,
            ..Default::default()
        }
    }

    /// Add a path filter.
    pub fn filter_path(mut self, path: PathBuf) -> Self {
        self.path_filters.push(path);
        self
    }

    /// Set whether to include untracked files.
    pub fn with_untracked(mut self, include: bool) -> Self {
        self.include_untracked = include;
        self
    }

    /// Set whether to include ignored files.
    pub fn with_ignored(mut self, include: bool) -> Self {
        self.include_ignored = include;
        self
    }
}

// Helper Functions

/// Check if a path should be ignored (always-ignored patterns).
///
/// This checks against built-in patterns like `.atomic/` and `.git/`.
pub fn is_always_ignored(path: &Path) -> bool {
    for component in path.components() {
        if let std::path::Component::Normal(name) = component {
            if let Some(name_str) = name.to_str() {
                if ALWAYS_IGNORED.contains(&name_str) {
                    return true;
                }
            }
        }
    }
    false
}

/// Hash the contents of a file using Blake3.
///
/// This is used to compute content hashes for modification detection.
///
/// # Arguments
///
/// * `path` - Path to the file to hash
///
/// # Returns
///
/// The Blake3 hash of the file contents.
pub fn hash_file_contents(path: &Path) -> StatusResult<Hash> {
    let contents = std::fs::read(path)?;
    Ok(Hash::of(&contents))
}

/// Collect all files in a directory, respecting ignore patterns.
///
/// # Arguments
///
/// * `root` - The root directory to walk
/// * `options` - Status options controlling which files to include
///
/// # Returns
///
/// A set of paths relative to the root directory.
pub fn collect_working_copy_files(
    root: &Path,
    options: &StatusOptions,
) -> StatusResult<HashSet<PathBuf>> {
    collect_working_copy_files_with_rules(root, options, None)
}

/// Collect all files in a directory, respecting ignore patterns and custom rules.
///
/// This is the full version that accepts optional [`IgnoreRules`] for pattern matching.
///
/// # Arguments
///
/// * `root` - The root directory to walk
/// * `options` - Status options controlling which files to include
/// * `rules` - Optional ignore rules from `.atomicignore` files
///
/// # Returns
///
/// A set of paths relative to the root directory.
pub fn collect_working_copy_files_with_rules(
    root: &Path,
    options: &StatusOptions,
    rules: Option<&IgnoreRules>,
) -> StatusResult<HashSet<PathBuf>> {
    let mut files = HashSet::new();

    // Use walkdir for directory traversal
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Always skip the internal directories
            if let Some(name) = e.file_name().to_str() {
                if ALWAYS_IGNORED.contains(&name) {
                    return false;
                }
            }

            // Check ignore rules if provided and not including ignored files
            if !options.include_ignored {
                if let Some(rules) = rules {
                    if let Ok(rel_path) = e.path().strip_prefix(root) {
                        let is_dir = e.file_type().is_dir();
                        if rules.is_ignored(rel_path, is_dir) {
                            return false;
                        }
                    }
                }
            }

            true
        });

    for entry in walker {
        let entry = entry?;

        // Skip directories (we only care about files)
        if entry.file_type().is_dir() {
            continue;
        }

        // Get path relative to root
        if let Ok(rel_path) = entry.path().strip_prefix(root) {
            // Check path filters if specified
            if !options.path_filters.is_empty() {
                let matches_filter = options
                    .path_filters
                    .iter()
                    .any(|filter| rel_path.starts_with(filter) || filter.starts_with(rel_path));
                if !matches_filter {
                    continue;
                }
            }

            files.insert(rel_path.to_path_buf());
        }
    }

    Ok(files)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // FileStatus Tests

    #[test]
    fn test_file_status_is_dirty() {
        assert!(!FileStatus::Clean.is_dirty());
        assert!(FileStatus::Modified.is_dirty());
        assert!(FileStatus::Deleted.is_dirty());
        assert!(!FileStatus::Untracked.is_dirty());
        assert!(FileStatus::Added.is_dirty());
        assert!(!FileStatus::Conflicted.is_dirty());
        assert!(FileStatus::TypeChanged.is_dirty());
        assert!(FileStatus::PermissionsChanged.is_dirty());
    }

    #[test]
    fn test_file_status_is_untracked() {
        assert!(!FileStatus::Clean.is_untracked());
        assert!(!FileStatus::Modified.is_untracked());
        assert!(FileStatus::Untracked.is_untracked());
    }

    #[test]
    fn test_file_status_is_conflicted() {
        assert!(!FileStatus::Clean.is_conflicted());
        assert!(!FileStatus::Modified.is_conflicted());
        assert!(FileStatus::Conflicted.is_conflicted());
    }

    #[test]
    fn test_file_status_is_clean() {
        assert!(FileStatus::Clean.is_clean());
        assert!(!FileStatus::Modified.is_clean());
        assert!(!FileStatus::Untracked.is_clean());
    }

    #[test]
    fn test_file_status_short_code() {
        assert_eq!(FileStatus::Clean.short_code(), ' ');
        assert_eq!(FileStatus::Modified.short_code(), 'M');
        assert_eq!(FileStatus::Deleted.short_code(), 'D');
        assert_eq!(FileStatus::Untracked.short_code(), '?');
        assert_eq!(FileStatus::Added.short_code(), 'A');
        assert_eq!(FileStatus::Conflicted.short_code(), 'C');
        assert_eq!(FileStatus::TypeChanged.short_code(), 'T');
        assert_eq!(FileStatus::PermissionsChanged.short_code(), 'P');
    }

    #[test]
    fn test_file_status_description() {
        assert_eq!(FileStatus::Clean.description(), "unchanged");
        assert_eq!(FileStatus::Modified.description(), "modified");
        assert_eq!(FileStatus::Deleted.description(), "deleted");
        assert_eq!(FileStatus::Untracked.description(), "untracked");
    }

    #[test]
    fn test_file_status_display() {
        assert_eq!(format!("{}", FileStatus::Modified), "modified");
        assert_eq!(format!("{}", FileStatus::Untracked), "untracked");
    }

    // FileStatusEntry Tests

    #[test]
    fn test_file_status_entry_new() {
        let entry = FileStatusEntry::new(PathBuf::from("src/main.rs"), FileStatus::Modified);

        assert_eq!(entry.path(), Path::new("src/main.rs"));
        assert_eq!(entry.status(), FileStatus::Modified);
        assert!(entry.inode().is_none());
        assert!(entry.recorded_hash().is_none());
        assert!(entry.current_hash().is_none());
        assert!(entry.details().is_none());
    }

    #[test]
    fn test_file_status_entry_with_details() {
        let hash1 = Hash::of(b"old content");
        let hash2 = Hash::of(b"new content");
        let inode = Inode::new(42);

        let entry = FileStatusEntry::with_details(
            PathBuf::from("test.txt"),
            FileStatus::Modified,
            Some(inode),
            Some(hash1),
            Some(hash2),
            Some("Content changed".to_string()),
        );

        assert_eq!(entry.path(), Path::new("test.txt"));
        assert_eq!(entry.status(), FileStatus::Modified);
        assert_eq!(entry.inode(), Some(inode));
        assert_eq!(entry.recorded_hash(), Some(&hash1));
        assert_eq!(entry.current_hash(), Some(&hash2));
        assert_eq!(entry.details(), Some("Content changed"));
    }

    #[test]
    fn test_file_status_entry_setters() {
        let mut entry = FileStatusEntry::new(PathBuf::from("test.txt"), FileStatus::Modified);

        let hash = Hash::of(b"content");
        let inode = Inode::new(100);

        entry.set_inode(inode);
        entry.set_recorded_hash(hash);
        entry.set_current_hash(hash);
        entry.set_details("Some details".to_string());

        assert_eq!(entry.inode(), Some(inode));
        assert_eq!(entry.recorded_hash(), Some(&hash));
        assert_eq!(entry.current_hash(), Some(&hash));
        assert_eq!(entry.details(), Some("Some details"));
    }

    // RepositoryStatus Tests

    #[test]
    fn test_repository_status_new() {
        let merkle = Merkle::of(b"test state");
        let status = RepositoryStatus::new("main".to_string(), Some(merkle));

        assert_eq!(status.stack(), "main");
        assert_eq!(status.state(), Some(&merkle));
        assert_eq!(status.total_count(), 0);
        assert!(status.is_clean());
    }

    #[test]
    fn test_repository_status_add_entry() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        status.add_entry(FileStatusEntry::new(
            PathBuf::from("file1.txt"),
            FileStatus::Modified,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("file2.txt"),
            FileStatus::Untracked,
        ));

        assert_eq!(status.total_count(), 2);
        assert!(!status.is_clean());
    }

    #[test]
    fn test_repository_status_get() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        status.add_entry(FileStatusEntry::new(
            PathBuf::from("src/main.rs"),
            FileStatus::Modified,
        ));

        let entry = status.get(Path::new("src/main.rs"));
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().status(), FileStatus::Modified);

        let missing = status.get(Path::new("nonexistent.rs"));
        assert!(missing.is_none());
    }

    #[test]
    fn test_repository_status_filters() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        // Add various status entries
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("clean.txt"),
            FileStatus::Clean,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("modified.txt"),
            FileStatus::Modified,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("deleted.txt"),
            FileStatus::Deleted,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("untracked.txt"),
            FileStatus::Untracked,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("added.txt"),
            FileStatus::Added,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("conflicted.txt"),
            FileStatus::Conflicted,
        ));

        // Test counts
        assert_eq!(status.total_count(), 6);
        assert_eq!(status.modified_count(), 1);
        assert_eq!(status.deleted_count(), 1);
        assert_eq!(status.untracked_count(), 1);
        assert_eq!(status.added_count(), 1);
        assert_eq!(status.conflicted_count(), 1);
        assert_eq!(status.clean_count(), 1);
        assert_eq!(status.dirty_count(), 3); // Modified, Deleted, Added
    }

    #[test]
    fn test_repository_status_is_clean() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        // Empty is clean
        assert!(status.is_clean());

        // Clean files are still clean
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("clean.txt"),
            FileStatus::Clean,
        ));
        assert!(status.is_clean());

        // Untracked doesn't make it dirty
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("untracked.txt"),
            FileStatus::Untracked,
        ));
        assert!(status.is_clean());

        // Modified makes it dirty
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("modified.txt"),
            FileStatus::Modified,
        ));
        assert!(!status.is_clean());
    }

    #[test]
    fn test_repository_status_has_conflicts() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        assert!(!status.has_conflicts());

        status.add_entry(FileStatusEntry::new(
            PathBuf::from("clean.txt"),
            FileStatus::Clean,
        ));
        assert!(!status.has_conflicts());

        status.add_entry(FileStatusEntry::new(
            PathBuf::from("conflict.txt"),
            FileStatus::Conflicted,
        ));
        assert!(status.has_conflicts());
    }

    #[test]
    fn test_repository_status_has_untracked() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        assert!(!status.has_untracked());

        status.add_entry(FileStatusEntry::new(
            PathBuf::from("tracked.txt"),
            FileStatus::Clean,
        ));
        assert!(!status.has_untracked());

        status.add_entry(FileStatusEntry::new(
            PathBuf::from("new.txt"),
            FileStatus::Untracked,
        ));
        assert!(status.has_untracked());
    }

    #[test]
    fn test_repository_status_paths_with_status() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        status.add_entry(FileStatusEntry::new(
            PathBuf::from("a.txt"),
            FileStatus::Modified,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("b.txt"),
            FileStatus::Modified,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("c.txt"),
            FileStatus::Untracked,
        ));

        let modified_paths = status.paths_with_status(FileStatus::Modified);
        assert_eq!(modified_paths.len(), 2);

        let untracked_paths = status.paths_with_status(FileStatus::Untracked);
        assert_eq!(untracked_paths.len(), 1);
    }

    #[test]
    fn test_repository_status_tracked_paths() {
        let mut status = RepositoryStatus::new("dev".to_string(), None);

        status.add_entry(FileStatusEntry::new(
            PathBuf::from("tracked1.txt"),
            FileStatus::Clean,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("tracked2.txt"),
            FileStatus::Modified,
        ));
        status.add_entry(FileStatusEntry::new(
            PathBuf::from("untracked.txt"),
            FileStatus::Untracked,
        ));

        let tracked = status.tracked_paths();
        assert_eq!(tracked.len(), 2);
    }

    // StatusOptions Tests

    #[test]
    fn test_status_options_default() {
        let opts = StatusOptions::default();

        assert!(opts.include_untracked);
        assert!(!opts.include_ignored);
        assert!(opts.path_filters.is_empty());
        assert!(opts.respect_ignore_files);
        assert!(opts.hash_contents);
    }

    #[test]
    fn test_status_options_tracked_only() {
        let opts = StatusOptions::tracked_only();

        assert!(!opts.include_untracked);
    }

    #[test]
    fn test_status_options_all() {
        let opts = StatusOptions::all();

        assert!(opts.include_untracked);
        assert!(opts.include_ignored);
    }

    #[test]
    fn test_status_options_fast() {
        let opts = StatusOptions::fast();

        assert!(!opts.hash_contents);
    }

    #[test]
    fn test_status_options_builder() {
        let opts = StatusOptions::default()
            .with_untracked(false)
            .with_ignored(true)
            .filter_path(PathBuf::from("src"));

        assert!(!opts.include_untracked);
        assert!(opts.include_ignored);
        assert_eq!(opts.path_filters.len(), 1);
    }

    // Helper Function Tests

    #[test]
    fn test_is_always_ignored() {
        assert!(is_always_ignored(Path::new(".atomic")));
        assert!(is_always_ignored(Path::new(".atomic/changes")));
        assert!(is_always_ignored(Path::new("src/.atomic/test")));
        assert!(is_always_ignored(Path::new(".git")));
        assert!(is_always_ignored(Path::new(".git/objects")));

        assert!(!is_always_ignored(Path::new("src")));
        assert!(!is_always_ignored(Path::new("src/main.rs")));
        assert!(!is_always_ignored(Path::new("atomic")));
    }

    #[test]
    fn test_hash_file_contents() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        std::fs::write(&file_path, b"Hello, world!").unwrap();

        let hash = hash_file_contents(&file_path).unwrap();
        let expected = Hash::of(b"Hello, world!");

        assert_eq!(hash, expected);
    }

    #[test]
    fn test_collect_working_copy_files() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create some files
        std::fs::write(root.join("file1.txt"), b"content1").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();

        // Create .atomic directory (should be ignored)
        std::fs::create_dir_all(root.join(".atomic")).unwrap();
        std::fs::write(root.join(".atomic/config.toml"), b"test").unwrap();

        let options = StatusOptions::default();
        let files = collect_working_copy_files(root, &options).unwrap();

        assert!(files.contains(&PathBuf::from("file1.txt")));
        assert!(files.contains(&PathBuf::from("src/main.rs")));
        assert!(!files.contains(&PathBuf::from(".atomic/config.toml")));
    }

    #[test]
    fn test_collect_working_copy_files_with_filter() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create files in different directories
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(root.join("tests/test.rs"), b"#[test]").unwrap();
        std::fs::write(root.join("README.md"), b"# Readme").unwrap();

        // Filter to only src directory
        let options = StatusOptions::default().filter_path(PathBuf::from("src"));
        let files = collect_working_copy_files(root, &options).unwrap();

        assert!(files.contains(&PathBuf::from("src/main.rs")));
        assert!(!files.contains(&PathBuf::from("tests/test.rs")));
        assert!(!files.contains(&PathBuf::from("README.md")));
    }

    // StatusError Tests

    #[test]
    fn test_status_error_display() {
        let err = StatusError::Database("connection failed".to_string());
        assert!(err.to_string().contains("connection failed"));

        let err = StatusError::InvalidState("corrupt tree".to_string());
        assert!(err.to_string().contains("corrupt tree"));

        let err = StatusError::PathError {
            path: "/some/path".to_string(),
            details: "invalid utf8".to_string(),
        };
        assert!(err.to_string().contains("/some/path"));
    }
}

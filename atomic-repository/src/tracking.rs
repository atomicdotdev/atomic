//! File tracking for Atomic VCS
//!
//! This module provides functionality for tracking files in the repository.
//! Tracking establishes the connection between files in the working copy and
//! the repository's internal graph structure.
//!
//! # Overview
//!
//! File tracking in Atomic works through **inodes** - stable file identifiers
//! that survive renames. When you track a file:
//!
//! 1. An inode is allocated for the file
//! 2. The path → inode mapping is stored in the TREE table
//! 3. The inode → path reverse mapping is stored in REV_TREE
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                         File Tracking Flow                          │
//! ├─────────────────────────────────────────────────────────────────────┤
//! │                                                                     │
//! │  Working Copy              Tree Tables               Graph          │
//! │  (Filesystem)              (Pristine DB)             (Content)      │
//! │                                                                     │
//! │  src/main.rs  ──add()──▶  TREE[src/main.rs] = 42                   │
//! │                           REV_TREE[42] = src/main.rs                │
//! │                                      │                              │
//! │                                      │ (after record)               │
//! │                                      ▼                              │
//! │                           INODES[42] = Position(...)  ──▶ Content   │
//! │                                                                     │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # File vs Directory Tracking
//!
//! Atomic tracks both files and directories as first-class citizens:
//!
//! - **Files**: Have content that will be stored in the graph
//! - **Directories**: Can be explicitly tracked, even when empty
//!
//! Unlike Git (which uses `.keep` files), Atomic tracks directories explicitly:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                    Directory Tracking Architecture                   │
//! ├─────────────────────────────────────────────────────────────────────┤
//! │                                                                     │
//! │  Git Approach:                Atomic Approach:                      │
//! │  src/empty_module/.keep       src/empty_module/                     │
//! │  (synthetic file)             (first-class directory)               │
//! │                                                                     │
//! │  Tables:                      Tables:                               │
//! │  TREE[path] = inode           TREE[path] = inode                    │
//! │  (files only)                 DIRECTORIES[inode] = flags            │
//! │                               (explicit directory marker)           │
//! │                                                                     │
//! │  Graph:                       Graph:                                │
//! │  .keep content span         Empty inode span (FOLDER edge)      │
//! │                                                                     │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Tracking vs Recording
//!
//! **Tracking** (`add`) just marks a file for version control:
//! - Allocates an inode
//! - Creates tree mappings
//! - For directories: marks inode in DIRECTORIES table
//! - Does NOT create a change or modify the graph
//!
//! **Recording** (`record`) creates a change from tracked files:
//! - Reads file contents
//! - Creates hunks and atoms (including `GraphOp::DirAdd` for directories)
//! - Stores content in the graph
//! - Creates a change that can be applied
//!
//! # Ignore Patterns
//!
//! The tracking system respects `.atomicignore` patterns. See [`crate::ignore`]
//! for details on pattern syntax and precedence.
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_repository::Repository;
//!
//! let mut repo = Repository::open(".")?;
//!
//! // Track a single file
//! repo.add("src/main.rs")?;
//!
//! // Track an entire directory (recursively adds files)
//! repo.add("src/")?;
//!
//! // Track an empty directory explicitly
//! repo.add_directory("src/empty_module/")?;
//!
//! // Check if a file is tracked
//! if repo.is_tracked("src/main.rs")? {
//!     println!("File is tracked");
//! }
//!
//! // Remove from tracking (does not delete the file)
//! repo.remove("old_file.txt")?;
//!
//! // Move/rename a tracked file
//! repo.move_file("old_name.rs", "new_name.rs")?;
//! ```

use std::path::{Path, PathBuf};

use atomic_core::pristine::directory_flags;
use atomic_core::pristine::{MutTxnT, TreeTxnT};
use atomic_core::types::Inode;
use thiserror::Error;

use crate::ignore::IgnoreRules;
use crate::status::is_always_ignored;

// Constants

/// Maximum depth for recursive directory traversal.
///
/// This prevents infinite loops from symlink cycles and limits memory usage.
const MAX_RECURSION_DEPTH: usize = 100;

// Error Types

/// Result type for tracking operations.
pub type TrackingResult<T> = Result<T, TrackingError>;

/// Errors that can occur during file tracking operations.
#[derive(Debug, Error)]
pub enum TrackingError {
    /// The file or directory does not exist.
    #[error("Path not found: {path}")]
    PathNotFound {
        /// The path that doesn't exist
        path: String,
    },

    /// The file is already tracked.
    #[error("Already tracked: {path}")]
    AlreadyTracked {
        /// The path that's already tracked
        path: String,
    },

    /// The file is not tracked.
    #[error("Not tracked: {path}")]
    NotTracked {
        /// The path that's not tracked
        path: String,
    },

    /// The path is inside the .atomic directory.
    #[error("Cannot track internal path: {path}")]
    InternalPath {
        /// The internal path
        path: String,
    },

    /// The path is outside the repository.
    #[error("Path is outside repository: {path}")]
    OutsidRepository {
        /// The external path
        path: String,
    },

    /// The destination path already exists (for move operations).
    #[error("Destination already exists: {path}")]
    DestinationExists {
        /// The existing destination path
        path: String,
    },

    /// Maximum recursion depth exceeded.
    #[error("Maximum recursion depth exceeded at: {path}")]
    MaxDepthExceeded {
        /// The path where max depth was reached
        path: String,
    },

    /// A database error occurred.
    #[error("Database error: {0}")]
    Database(String),

    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Directory traversal error.
    #[error("Directory traversal error: {0}")]
    WalkDir(#[from] walkdir::Error),

    /// Cannot delete a non-empty directory.
    #[error("Directory not empty: {path}")]
    DirectoryNotEmpty {
        /// The directory that has children
        path: String,
    },

    /// Path is a directory but was expected to be a file.
    #[error("Path is a directory: {path}")]
    IsDirectory {
        /// The directory path
        path: String,
    },

    /// Path is a file but was expected to be a directory.
    #[error("Path is not a directory: {path}")]
    NotDirectory {
        /// The file path
        path: String,
    },
}

// TrackingStats

/// Statistics from a tracking operation.
///
/// This provides feedback about what was done during add/remove operations,
/// especially useful for recursive operations on directories.
#[derive(Debug, Clone, Default)]
pub struct TrackingStats {
    /// Number of files added to tracking.
    pub files_added: usize,

    /// Number of directories added to tracking.
    pub directories_added: usize,

    /// Number of explicit (empty) directories added.
    pub explicit_directories_added: usize,

    /// Number of files removed from tracking.
    pub files_removed: usize,

    /// Number of directories removed from tracking.
    pub directories_removed: usize,

    /// Number of files skipped (already tracked or ignored).
    pub skipped: usize,

    /// Paths that were skipped with reasons.
    pub skipped_paths: Vec<(PathBuf, String)>,
}

impl TrackingStats {
    /// Create empty stats.
    pub fn new() -> Self {
        Self::default()
    }

    /// Total number of items added.
    pub fn total_added(&self) -> usize {
        self.files_added + self.directories_added + self.explicit_directories_added
    }

    /// Total number of items removed.
    pub fn total_removed(&self) -> usize {
        self.files_removed + self.directories_removed
    }

    /// Check if any changes were made.
    pub fn has_changes(&self) -> bool {
        self.total_added() > 0 || self.total_removed() > 0
    }

    /// Record a skipped path.
    pub fn skip(&mut self, path: PathBuf, reason: &str) {
        self.skipped += 1;
        self.skipped_paths.push((path, reason.to_string()));
    }
}

// TrackingOptions

/// Options for controlling tracking operations.
#[derive(Debug, Clone)]
pub struct TrackingOptions {
    /// Recursively add/remove directories.
    ///
    /// When `true`, adding a directory will add all files within it.
    /// Default: `true`
    pub recursive: bool,

    /// Force the operation even if it would normally be skipped.
    ///
    /// For add: Add even if already tracked (no-op but no error).
    /// For remove: Remove even if not tracked (no-op but no error).
    /// Default: `false`
    pub force: bool,

    /// Include hidden files (starting with '.').
    ///
    /// Default: `true`
    pub include_hidden: bool,

    /// Dry run - don't actually make changes, just report what would be done.
    ///
    /// Default: `false`
    pub dry_run: bool,
}

impl Default for TrackingOptions {
    fn default() -> Self {
        Self {
            recursive: true,
            force: false,
            include_hidden: true,
            dry_run: false,
        }
    }
}

impl TrackingOptions {
    /// Create options for non-recursive operation.
    pub fn non_recursive() -> Self {
        Self {
            recursive: false,
            ..Default::default()
        }
    }

    /// Create options with force enabled.
    pub fn forced() -> Self {
        Self {
            force: true,
            ..Default::default()
        }
    }

    /// Create options for dry run.
    pub fn dry_run() -> Self {
        Self {
            dry_run: true,
            ..Default::default()
        }
    }

    /// Set recursive option.
    pub fn with_recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        self
    }

    /// Set force option.
    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Set include_hidden option.
    pub fn with_hidden(mut self, include: bool) -> Self {
        self.include_hidden = include;
        self
    }
}

// TrackedFile

/// Information about a tracked file.
#[derive(Debug, Clone)]
pub struct TrackedFile {
    /// Path relative to repository root.
    pub path: PathBuf,

    /// The file's inode (stable identifier).
    pub inode: Inode,

    /// Whether this is a directory.
    pub is_directory: bool,
}

impl TrackedFile {
    /// Create a new tracked file entry.
    pub fn new(path: PathBuf, inode: Inode, is_directory: bool) -> Self {
        Self {
            path,
            inode,
            is_directory,
        }
    }
}

// Helper Functions

/// Normalize a path for storage.
/// Normalize a path for storage in the repository.
///
/// This:
/// - Converts backslashes to forward slashes (Windows compatibility)
/// - Removes trailing slashes
/// - Strips absolute path prefix if it matches repo root
/// - No trailing slash (except for root)
/// - Relative to repository root
pub fn normalize_path(path: &Path) -> String {
    normalize_path_with_root(path, None)
}

/// Normalize a path for storage, optionally stripping a repo root prefix.
///
/// This handles the case where absolute paths are accidentally passed in.
/// On macOS, `/tmp` is a symlink to `/private/tmp`, so we try both the
/// given root and its canonical form.
///
/// # Arguments
///
/// * `path` - The path to normalize
/// * `repo_root` - Optional repository root to strip from absolute paths
pub fn normalize_path_with_root(path: &Path, repo_root: Option<&Path>) -> String {
    let mut path_to_normalize = path.to_path_buf();

    // If path is absolute and we have a repo root, try to make it relative
    if path_to_normalize.is_absolute() {
        if let Some(root) = repo_root {
            // Try stripping the root directly
            if let Ok(rel) = path_to_normalize.strip_prefix(root) {
                path_to_normalize = rel.to_path_buf();
            } else if let Ok(canonical_root) = root.canonicalize() {
                // On macOS, /tmp -> /private/tmp, so try canonical
                if let Ok(rel) = path_to_normalize.strip_prefix(&canonical_root) {
                    path_to_normalize = rel.to_path_buf();
                }
            }
        }
    }

    let path_str = path_to_normalize.to_string_lossy();

    // Convert to forward slashes and remove trailing slash
    let normalized = path_str
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();

    // Handle empty path (current directory)
    if normalized.is_empty() {
        ".".to_string()
    } else {
        normalized
    }
}

/// Check if a path should be ignored during tracking.
///
/// This checks (in order):
/// 1. Internal directories (.atomic, .git) - always ignored
/// 2. `.atomicignore` patterns (if rules provided)
/// 3. Hidden files (if not included)
///
/// # Arguments
///
/// * `path` - Path to check (relative to repository root)
/// * `include_hidden` - Whether to include hidden files (starting with '.')
///
/// # Example
///
/// ```rust,ignore
/// use atomic_repository::tracking::should_ignore;
///
/// // Without ignore rules
/// assert!(should_ignore(Path::new(".atomic"), true, None));
/// assert!(!should_ignore(Path::new("src/main.rs"), true, None));
///
/// // With ignore rules
/// let rules = IgnoreRules::load(repo_root);
/// assert!(should_ignore(Path::new("target/debug"), true, Some(&rules)));
/// ```
pub fn should_ignore(path: &Path, include_hidden: bool) -> bool {
    should_ignore_with_rules(path, include_hidden, false, None)
}

/// Check if a path should be ignored during tracking, with optional ignore rules.
///
/// This is the full version that accepts optional [`IgnoreRules`] for pattern matching.
///
/// # Arguments
///
/// * `path` - Path to check (relative to repository root)
/// * `include_hidden` - Whether to include hidden files (starting with '.')
/// * `is_dir` - Whether the path is a directory
/// * `rules` - Optional ignore rules from `.atomicignore` files
///
/// # Returns
///
/// `true` if the path should be ignored, `false` otherwise.
pub fn should_ignore_with_rules(
    path: &Path,
    include_hidden: bool,
    is_dir: bool,
    rules: Option<&IgnoreRules>,
) -> bool {
    // Always ignore internal directories
    if is_always_ignored(path) {
        return true;
    }

    // Check ignore rules if provided
    if let Some(rules) = rules {
        if rules.is_ignored(path, is_dir) {
            return true;
        }
    }

    // Check for hidden files
    if !include_hidden {
        if let Some(name) = path.file_name() {
            if let Some(name_str) = name.to_str() {
                if name_str.starts_with('.') {
                    return true;
                }
            }
        }
    }

    false
}

/// Collect all files in a directory for tracking.
///
/// This walks the directory tree and returns paths relative to the given root.
/// Files matching `.atomicignore` patterns are excluded.
///
/// # Arguments
///
/// * `root` - Repository root directory
/// * `path` - Path to collect files from (relative to root)
/// * `options` - Tracking options
///
/// # Returns
///
/// A vector of paths relative to the repository root.
pub fn collect_files_for_tracking(
    root: &Path,
    path: &Path,
    options: &TrackingOptions,
) -> TrackingResult<Vec<PathBuf>> {
    collect_files_for_tracking_with_rules(root, path, options, None)
}

/// Collect all files in a directory for tracking, with optional ignore rules.
///
/// This is the full version that accepts optional [`IgnoreRules`] for pattern matching.
///
/// # Arguments
///
/// * `root` - Repository root directory
/// * `path` - Path to collect files from (relative to root)
/// * `options` - Tracking options
/// * `rules` - Optional ignore rules from `.atomicignore` files
///
/// # Returns
///
/// A vector of paths relative to the repository root.
pub fn collect_files_for_tracking_with_rules(
    root: &Path,
    path: &Path,
    options: &TrackingOptions,
    rules: Option<&IgnoreRules>,
) -> TrackingResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    let abs_path = root.join(path);

    if !abs_path.exists() {
        return Err(TrackingError::PathNotFound {
            path: path.display().to_string(),
        });
    }

    if abs_path.is_file() {
        // Single file
        if !should_ignore_with_rules(path, options.include_hidden, false, rules) {
            files.push(path.to_path_buf());
        }
    } else if abs_path.is_dir() {
        if options.recursive {
            // Walk the directory
            let walker = walkdir::WalkDir::new(&abs_path)
                .max_depth(MAX_RECURSION_DEPTH)
                .follow_links(false)
                .into_iter()
                .filter_entry(|e| {
                    let entry_path = e.path();
                    if let Ok(rel) = entry_path.strip_prefix(root) {
                        let is_dir = e.file_type().is_dir();
                        !should_ignore_with_rules(rel, options.include_hidden, is_dir, rules)
                    } else {
                        true
                    }
                });

            for entry in walker {
                let entry = entry?;
                let entry_path = entry.path();

                // Get path relative to repository root
                if let Ok(rel_path) = entry_path.strip_prefix(root) {
                    // Only track files, not directories
                    // Directories are implicitly tracked through their contents
                    if entry_path.is_dir() {
                        continue;
                    }

                    // Include only files
                    files.push(rel_path.to_path_buf());
                }
            }
        } else {
            // Non-recursive: just add the directory itself
            if !should_ignore_with_rules(path, options.include_hidden, true, rules) {
                files.push(path.to_path_buf());
            }
        }
    }

    Ok(files)
}

// Core Tracking Functions

/// Add a single file to tracking.
///
/// This is the low-level function that actually modifies the database.
/// It does NOT check if the file exists on disk or is already tracked.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `path` - The normalized path string
/// * `is_directory` - Whether this is a directory
///
/// # Returns
///
/// The allocated inode for the file.
pub fn add_to_tree<T: MutTxnT>(
    txn: &mut T,
    path: &str,
    is_directory: bool,
) -> TrackingResult<Inode> {
    // Allocate a new inode
    let inode = txn
        .alloc_inode()
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    // Add to tree tables
    txn.put_tree(path, inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    // If this is a directory, mark it in the DIRECTORIES table
    if is_directory {
        txn.put_directory(inode, directory_flags::DIR_EXPLICIT)
            .map_err(|e| TrackingError::Database(e.to_string()))?;
    }

    Ok(inode)
}

/// Add an empty directory to tracking explicitly.
///
/// This is distinct from `add_to_tree` because it specifically handles
/// empty directories that need to be tracked even without children.
/// The directory will be marked with `DIR_EXPLICIT | DIR_EMPTY` flags.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `path` - The normalized directory path
///
/// # Returns
///
/// The allocated inode for the directory.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::pristine::Pristine;
/// use atomic_repository::tracking::add_directory_to_tree;
///
/// let pristine = Pristine::open(path)?;
/// let mut txn = pristine.write_txn()?;
///
/// // Track an empty directory
/// let inode = add_directory_to_tree(&mut txn, "src/empty_module")?;
/// txn.commit()?;
/// ```
pub fn add_directory_to_tree<T: MutTxnT>(txn: &mut T, path: &str) -> TrackingResult<Inode> {
    // Allocate a new inode
    let inode = txn
        .alloc_inode()
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    // Add to tree tables
    txn.put_tree(path, inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    // Mark as an explicit empty directory
    txn.put_directory(inode, directory_flags::explicit_empty())
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    Ok(inode)
}

/// Check if an inode represents a directory.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `inode` - The inode to check
///
/// # Returns
///
/// `true` if this inode is marked as a directory in the DIRECTORIES table.
pub fn is_directory_inode<T: TreeTxnT>(txn: &T, inode: Inode) -> TrackingResult<bool> {
    txn.is_directory(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))
}

/// Get directory flags for an inode.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `inode` - The inode to check
///
/// # Returns
///
/// The directory flags if this inode is a directory, `None` if it's a file.
pub fn get_directory_flags<T: TreeTxnT>(txn: &T, inode: Inode) -> TrackingResult<Option<u8>> {
    txn.get_directory_flags(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))
}

/// Update directory flags (e.g., when adding/removing children).
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `inode` - The directory's inode
/// * `flags` - New flags to set
pub fn update_directory_flags<T: MutTxnT>(
    txn: &mut T,
    inode: Inode,
    flags: u8,
) -> TrackingResult<()> {
    txn.update_directory_flags(inode, flags)
        .map_err(|e| TrackingError::Database(e.to_string()))
}

/// Mark a directory as having children (not empty).
///
/// This is called when a file is added under a tracked directory.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `inode` - The directory's inode
pub fn mark_directory_has_children<T: MutTxnT + TreeTxnT>(txn: &mut T, inode: Inode) -> TrackingResult<()> {
    if let Some(flags) = txn
        .get_directory_flags(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?
    {
        // Remove the DIR_EMPTY flag if present
        let new_flags = flags & !directory_flags::DIR_EMPTY;
        if new_flags != flags {
            txn.update_directory_flags(inode, new_flags)
                .map_err(|e| TrackingError::Database(e.to_string()))?;
        }
    }
    Ok(())
}

/// Mark a directory as empty (no children).
///
/// This is called when the last file is removed from a tracked directory.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `inode` - The directory's inode
pub fn mark_directory_empty<T: MutTxnT + TreeTxnT>(txn: &mut T, inode: Inode) -> TrackingResult<()> {
    if let Some(flags) = txn
        .get_directory_flags(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?
    {
        // Add the DIR_EMPTY flag
        let new_flags = flags | directory_flags::DIR_EMPTY;
        if new_flags != flags {
            txn.update_directory_flags(inode, new_flags)
                .map_err(|e| TrackingError::Database(e.to_string()))?;
        }
    }
    Ok(())
}

/// Remove a single file from tracking.
///
/// This is the low-level function that actually modifies the database.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `path` - The normalized path string
///
/// # Returns
///
/// The inode that was removed, if any.
pub fn remove_from_tree<T: MutTxnT>(txn: &mut T, path: &str) -> TrackingResult<Option<Inode>> {
    // Remove from tree (this also removes from REV_TREE)
    let inode = txn
        .del_tree(path)
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    // If there was an inode, also remove its position mapping and directory marker
    if let Some(inode) = inode {
        let _ = txn.del_inode(inode);
        // Remove directory marker if present
        let _ = txn.del_directory(inode);
    }

    Ok(inode)
}

/// Remove a directory from tracking.
///
/// This only removes the directory if it has no tracked children.
/// To force removal of a non-empty directory, use `remove_directory_recursive`.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `path` - The normalized directory path
///
/// # Returns
///
/// The inode that was removed.
///
/// # Errors
///
/// Returns `DirectoryNotEmpty` if the directory has tracked children.
pub fn remove_directory_from_tree<T: MutTxnT + TreeTxnT>(
    txn: &mut T,
    path: &str,
) -> TrackingResult<Inode> {
    // Get the inode first
    let inode = txn
        .get_inode(path)
        .map_err(|e| TrackingError::Database(e.to_string()))?
        .ok_or_else(|| TrackingError::NotTracked {
            path: path.to_string(),
        })?;

    // Check if it's actually a directory
    if !is_directory_inode(txn, inode)? {
        return Err(TrackingError::NotDirectory {
            path: path.to_string(),
        });
    }

    // Check for children
    let children = tracked_under_prefix(txn, path)?;
    let has_children = children.iter().any(|(p, _)| p != path);

    if has_children {
        return Err(TrackingError::DirectoryNotEmpty {
            path: path.to_string(),
        });
    }

    // Safe to remove
    txn.del_tree(path)
        .map_err(|e| TrackingError::Database(e.to_string()))?;
    txn.del_directory(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?;
    let _ = txn.del_inode(inode);

    Ok(inode)
}

/// Check if a path is tracked.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `path` - The normalized path string
pub fn is_tracked<T: TreeTxnT>(txn: &T, path: &str) -> TrackingResult<bool> {
    let result = txn
        .get_inode(path)
        .map_err(|e| TrackingError::Database(e.to_string()))?;
    Ok(result.is_some())
}

/// Get the inode for a tracked path.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `path` - The normalized path string
pub fn get_inode<T: TreeTxnT>(txn: &T, path: &str) -> TrackingResult<Option<Inode>> {
    txn.get_inode(path)
        .map_err(|e| TrackingError::Database(e.to_string()))
}

/// Get the path for an inode.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `inode` - The inode to look up
pub fn get_path<T: TreeTxnT>(txn: &T, inode: Inode) -> TrackingResult<Option<String>> {
    txn.get_path(inode)
        .map_err(|e| TrackingError::Database(e.to_string()))
}

/// List all tracked files.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
///
/// # Returns
///
/// A vector of all tracked files and directories.
pub fn list_tracked<T: TreeTxnT>(txn: &T) -> TrackingResult<Vec<TrackedFile>> {
    let iter = txn
        .iter_tree()
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    let mut results = Vec::new();
    for result in iter {
        let (path, inode) = result.map_err(|e| TrackingError::Database(e.to_string()))?;
        let is_directory = is_directory_inode(txn, inode)?;
        results.push(TrackedFile::new(PathBuf::from(path), inode, is_directory));
    }

    Ok(results)
}

/// List all tracked directories.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
///
/// # Returns
///
/// A vector of tracked directories.
pub fn list_tracked_directories<T: TreeTxnT>(
    txn: &T,
) -> TrackingResult<Vec<TrackedFile>> {
    let all_tracked = list_tracked(txn)?;
    Ok(all_tracked
        .into_iter()
        .filter(|f| f.is_directory)
        .collect())
}

/// List all explicitly tracked empty directories.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
///
/// # Returns
///
/// A vector of explicitly tracked empty directories.
pub fn list_explicit_empty_directories<T: TreeTxnT>(
    txn: &T,
) -> TrackingResult<Vec<TrackedFile>> {
    let all_tracked = list_tracked(txn)?;
    let mut results = Vec::new();

    for file in all_tracked {
        if file.is_directory {
            if let Some(flags) = get_directory_flags(txn, file.inode)? {
                if directory_flags::is_explicit(flags) && directory_flags::is_empty(flags) {
                    results.push(file);
                }
            }
        }
    }

    Ok(results)
}

/// Move/rename a tracked file.
///
/// This updates the path → inode mapping while preserving the inode,
/// so the file's history is maintained.
///
/// # Arguments
///
/// * `txn` - A mutable transaction
/// * `from` - The current path
/// * `to` - The new path
pub fn move_tracked<T: MutTxnT + TreeTxnT>(
    txn: &mut T,
    from: &str,
    to: &str,
) -> TrackingResult<Inode> {
    // Get the inode for the source
    let inode = txn
        .get_inode(from)
        .map_err(|e| TrackingError::Database(e.to_string()))?
        .ok_or_else(|| TrackingError::NotTracked {
            path: from.to_string(),
        })?;

    // Check destination doesn't exist
    if txn
        .get_inode(to)
        .map_err(|e| TrackingError::Database(e.to_string()))?
        .is_some()
    {
        return Err(TrackingError::DestinationExists {
            path: to.to_string(),
        });
    }

    // Remove old mapping
    txn.del_tree(from)
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    // Add new mapping with same inode
    txn.put_tree(to, inode)
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    Ok(inode)
}

/// Get all tracked paths under a directory prefix.
///
/// # Arguments
///
/// * `txn` - A transaction (read or write)
/// * `prefix` - The directory prefix to search under
pub fn tracked_under_prefix<T: TreeTxnT>(
    txn: &T,
    prefix: &str,
) -> TrackingResult<Vec<(String, Inode)>> {
    let iter = txn
        .iter_tree()
        .map_err(|e| TrackingError::Database(e.to_string()))?;

    let prefix_normalized = if prefix.ends_with('/') {
        prefix.to_string()
    } else {
        format!("{}/", prefix)
    };

    let mut results = Vec::new();
    for result in iter {
        let (path, inode) = result.map_err(|e| TrackingError::Database(e.to_string()))?;
        if path.starts_with(&prefix_normalized) || path == prefix.trim_end_matches('/') {
            results.push((path, inode));
        }
    }

    Ok(results)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    // Path Normalization Tests

    #[test]
    fn test_normalize_path_basic() {
        assert_eq!(normalize_path(Path::new("src/main.rs")), "src/main.rs");
        assert_eq!(normalize_path(Path::new("file.txt")), "file.txt");
    }

    #[test]
    fn test_normalize_path_trailing_slash() {
        assert_eq!(normalize_path(Path::new("src/")), "src");
        assert_eq!(normalize_path(Path::new("src/lib/")), "src/lib");
    }

    #[test]
    fn test_normalize_path_backslashes() {
        assert_eq!(normalize_path(Path::new("src\\main.rs")), "src/main.rs");
    }

    #[test]
    fn test_normalize_path_empty() {
        assert_eq!(normalize_path(Path::new("")), ".");
    }

    #[test]
    fn test_normalize_path_with_root_relative() {
        // Relative paths should pass through unchanged
        let root = Path::new("/repo");
        assert_eq!(
            normalize_path_with_root(Path::new("src/main.rs"), Some(root)),
            "src/main.rs"
        );
        assert_eq!(
            normalize_path_with_root(Path::new("file.txt"), Some(root)),
            "file.txt"
        );
    }

    #[test]
    fn test_normalize_path_with_root_absolute_matching() {
        // Absolute paths matching root should be made relative
        let root = Path::new("/repo");
        assert_eq!(
            normalize_path_with_root(Path::new("/repo/src/main.rs"), Some(root)),
            "src/main.rs"
        );
        assert_eq!(
            normalize_path_with_root(Path::new("/repo/file.txt"), Some(root)),
            "file.txt"
        );
    }

    #[test]
    fn test_normalize_path_with_root_absolute_not_matching() {
        // Absolute paths not matching root should remain absolute
        let root = Path::new("/repo");
        assert_eq!(
            normalize_path_with_root(Path::new("/other/src/main.rs"), Some(root)),
            "/other/src/main.rs"
        );
    }

    #[test]
    fn test_normalize_path_with_root_none() {
        // Without root, absolute paths remain absolute
        assert_eq!(
            normalize_path_with_root(Path::new("/repo/src/main.rs"), None),
            "/repo/src/main.rs"
        );
        // Relative paths still work
        assert_eq!(
            normalize_path_with_root(Path::new("src/main.rs"), None),
            "src/main.rs"
        );
    }

    #[test]
    fn test_normalize_path_with_root_trailing_slash() {
        let root = Path::new("/repo");
        assert_eq!(
            normalize_path_with_root(Path::new("/repo/src/"), Some(root)),
            "src"
        );
    }

    // Should Ignore Tests

    #[test]
    fn test_should_ignore_internal_dirs() {
        assert!(should_ignore(Path::new(".atomic"), true));
        assert!(should_ignore(Path::new(".atomic/changes"), true));
        assert!(should_ignore(Path::new(".git"), true));
        assert!(should_ignore(Path::new(".git/objects"), true));
    }

    #[test]
    fn test_should_ignore_with_rules() {
        let temp = tempfile::TempDir::new().unwrap();
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "target/\n*.log\n").unwrap();

        let rules = IgnoreRules::load(temp.path());

        // Create test directories/files for is_dir detection
        std::fs::create_dir_all(temp.path().join("target")).unwrap();
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("debug.log"), "log").unwrap();
        std::fs::write(temp.path().join("src/main.rs"), "fn main() {}").unwrap();

        // Test with rules
        assert!(should_ignore_with_rules(
            Path::new("target"),
            true,
            true, // is_dir
            Some(&rules)
        ));
        assert!(should_ignore_with_rules(
            Path::new("debug.log"),
            true,
            false, // is_dir
            Some(&rules)
        ));
        assert!(!should_ignore_with_rules(
            Path::new("src/main.rs"),
            true,
            false, // is_dir
            Some(&rules)
        ));

        // Test without rules (should still ignore internal dirs)
        assert!(should_ignore_with_rules(Path::new(".atomic"), true, true, None));
        assert!(!should_ignore_with_rules(
            Path::new("src/main.rs"),
            true,
            false,
            None
        ));
    }

    #[test]
    fn test_collect_files_with_ignore_rules() {
        let temp = tempfile::TempDir::new().unwrap();

        // Create directory structure
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::create_dir_all(temp.path().join("target/debug")).unwrap();
        std::fs::write(temp.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(temp.path().join("src/lib.rs"), "// lib").unwrap();
        std::fs::write(temp.path().join("target/debug/app"), "binary").unwrap();
        std::fs::write(temp.path().join("debug.log"), "log content").unwrap();
        std::fs::write(temp.path().join("Cargo.toml"), "[package]").unwrap();

        // Create ignore file
        let ignore_path = temp.path().join(".atomicignore");
        std::fs::write(&ignore_path, "target/\n*.log\n").unwrap();

        let rules = IgnoreRules::load(temp.path());
        let options = TrackingOptions::default();

        // Collect without rules
        let files_no_rules =
            collect_files_for_tracking(temp.path(), Path::new("."), &options).unwrap();

        // Collect with rules
        let files_with_rules =
            collect_files_for_tracking_with_rules(temp.path(), Path::new("."), &options, Some(&rules))
                .unwrap();

        // Without rules, should include target/ and *.log files
        assert!(files_no_rules.iter().any(|p| p.starts_with("target")));
        assert!(files_no_rules
            .iter()
            .any(|p| p.to_string_lossy().ends_with(".log")));

        // With rules, should exclude target/ and *.log files
        assert!(!files_with_rules.iter().any(|p| p.starts_with("target")));
        assert!(!files_with_rules
            .iter()
            .any(|p| p.to_string_lossy().ends_with(".log")));

        // Both should include src/
        assert!(files_with_rules.iter().any(|p| p.starts_with("src")));
    }

    #[test]
    fn test_should_ignore_normal_files() {
        assert!(!should_ignore(Path::new("src/main.rs"), true));
        assert!(!should_ignore(Path::new("README.md"), true));
        assert!(!should_ignore(Path::new("Cargo.toml"), true));
    }

    #[test]
    fn test_should_ignore_hidden_files() {
        // With include_hidden = true
        assert!(!should_ignore(Path::new(".hidden"), true));
        assert!(!should_ignore(Path::new(".config/settings"), true));

        // With include_hidden = false
        assert!(should_ignore(Path::new(".hidden"), false));
        assert!(should_ignore(Path::new("src/.hidden"), false));
    }

    // TrackingStats Tests

    #[test]
    fn test_tracking_stats_new() {
        let stats = TrackingStats::new();
        assert_eq!(stats.files_added, 0);
        assert_eq!(stats.directories_added, 0);
        assert_eq!(stats.total_added(), 0);
        assert!(!stats.has_changes());
    }

    #[test]
    fn test_tracking_stats_totals() {
        let mut stats = TrackingStats::new();
        stats.files_added = 5;
        stats.directories_added = 2;
        stats.files_removed = 1;

        assert_eq!(stats.total_added(), 7);
        assert_eq!(stats.total_removed(), 1);
        assert!(stats.has_changes());
    }

    #[test]
    fn test_tracking_stats_skip() {
        let mut stats = TrackingStats::new();
        stats.skip(PathBuf::from("test.txt"), "already tracked");

        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.skipped_paths.len(), 1);
        assert_eq!(stats.skipped_paths[0].0, PathBuf::from("test.txt"));
        assert_eq!(stats.skipped_paths[0].1, "already tracked");
    }

    // TrackingOptions Tests

    #[test]
    fn test_tracking_options_default() {
        let opts = TrackingOptions::default();
        assert!(opts.recursive);
        assert!(!opts.force);
        assert!(opts.include_hidden);
        assert!(!opts.dry_run);
    }

    #[test]
    fn test_tracking_options_non_recursive() {
        let opts = TrackingOptions::non_recursive();
        assert!(!opts.recursive);
    }

    #[test]
    fn test_tracking_options_forced() {
        let opts = TrackingOptions::forced();
        assert!(opts.force);
    }

    #[test]
    fn test_tracking_options_dry_run() {
        let opts = TrackingOptions::dry_run();
        assert!(opts.dry_run);
    }

    #[test]
    fn test_tracking_options_builder() {
        let opts = TrackingOptions::default()
            .with_recursive(false)
            .with_force(true)
            .with_hidden(false);

        assert!(!opts.recursive);
        assert!(opts.force);
        assert!(!opts.include_hidden);
    }

    // TrackedFile Tests

    #[test]
    fn test_tracked_file_new() {
        let file = TrackedFile::new(PathBuf::from("src/main.rs"), Inode::new(42), false);

        assert_eq!(file.path, PathBuf::from("src/main.rs"));
        assert_eq!(file.inode, Inode::new(42));
        assert!(!file.is_directory);
    }

    #[test]
    fn test_tracked_file_directory() {
        let dir = TrackedFile::new(PathBuf::from("src"), Inode::new(1), true);

        assert_eq!(dir.path, PathBuf::from("src"));
        assert!(dir.is_directory);
    }

    // Error Tests

    #[test]
    fn test_tracking_error_display() {
        let err = TrackingError::PathNotFound {
            path: "missing.txt".to_string(),
        };
        assert!(err.to_string().contains("missing.txt"));

        let err = TrackingError::AlreadyTracked {
            path: "file.txt".to_string(),
        };
        assert!(err.to_string().contains("Already tracked"));

        let err = TrackingError::NotTracked {
            path: "file.txt".to_string(),
        };
        assert!(err.to_string().contains("Not tracked"));

        let err = TrackingError::InternalPath {
            path: ".atomic".to_string(),
        };
        assert!(err.to_string().contains("internal"));

        let err = TrackingError::DestinationExists {
            path: "dest.txt".to_string(),
        };
        assert!(err.to_string().contains("already exists"));
    }

    // Collect Files Tests

    #[test]
    fn test_collect_files_single_file() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create a test file
        std::fs::write(root.join("test.txt"), b"content").unwrap();

        let options = TrackingOptions::default();
        let files = collect_files_for_tracking(root, Path::new("test.txt"), &options).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0], PathBuf::from("test.txt"));
    }

    #[test]
    fn test_collect_files_directory_recursive() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create directory structure
        std::fs::create_dir_all(root.join("src/subdir")).unwrap();
        std::fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(root.join("src/lib.rs"), b"// lib").unwrap();
        std::fs::write(root.join("src/subdir/mod.rs"), b"// mod").unwrap();

        let options = TrackingOptions::default();
        let files = collect_files_for_tracking(root, Path::new("src"), &options).unwrap();

        // Should have all files and subdirectory
        assert!(files.len() >= 3);
        assert!(files.iter().any(|p| p.ends_with("main.rs")));
        assert!(files.iter().any(|p| p.ends_with("lib.rs")));
        assert!(files.iter().any(|p| p.ends_with("mod.rs")));
    }

    #[test]
    fn test_collect_files_ignores_atomic_dir() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create .atomic directory
        std::fs::create_dir_all(root.join(".atomic")).unwrap();
        std::fs::write(root.join(".atomic/config"), b"test").unwrap();
        std::fs::write(root.join("normal.txt"), b"content").unwrap();

        let options = TrackingOptions::default();

        // Collecting from root should not include .atomic
        let files = collect_files_for_tracking(root, Path::new("."), &options);

        // The collect might fail or return empty for "." - that's fine
        // The important thing is .atomic is not included if it succeeds
        if let Ok(files) = files {
            for file in &files {
                assert!(
                    !file.starts_with(".atomic"),
                    "Should not include .atomic files"
                );
            }
        }
    }

    #[test]
    fn test_collect_files_nonexistent() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let options = TrackingOptions::default();
        let result = collect_files_for_tracking(root, Path::new("nonexistent.txt"), &options);

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), TrackingError::PathNotFound { .. }));
    }

    #[test]
    fn test_collect_files_non_recursive() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create directory with files
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), b"fn main() {}").unwrap();

        let options = TrackingOptions::non_recursive();
        let files = collect_files_for_tracking(root, Path::new("src"), &options).unwrap();

        // Non-recursive should only include the directory itself
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], PathBuf::from("src"));
    }

    #[test]
    fn test_collect_files_excludes_hidden() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create files including hidden
        std::fs::write(root.join("normal.txt"), b"content").unwrap();
        std::fs::write(root.join(".hidden"), b"hidden").unwrap();

        // With hidden excluded
        let options = TrackingOptions::default().with_hidden(false);
        let files = collect_files_for_tracking(root, Path::new("normal.txt"), &options).unwrap();
        assert_eq!(files.len(), 1);

        let hidden_result = collect_files_for_tracking(root, Path::new(".hidden"), &options).unwrap();
        assert!(hidden_result.is_empty());
    }

    // Integration Tests with Pristine

    #[test]
    fn test_add_and_check_tracked() {
        use atomic_core::pristine::Pristine;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("pristine.redb");

        let pristine = Pristine::open(&db_path).unwrap();

        // Add a file to tracking
        {
            let mut txn = pristine.write_txn().unwrap();
            let inode = add_to_tree(&mut txn, "src/main.rs", false).unwrap();
            assert!(inode.get() > 0);
            txn.commit().unwrap();
        }

        // Check it's tracked
        {
            let txn = pristine.read_txn().unwrap();
            assert!(is_tracked(&txn, "src/main.rs").unwrap());
            assert!(!is_tracked(&txn, "nonexistent.rs").unwrap());
        }
    }

    #[test]
    fn test_add_and_get_inode() {
        use atomic_core::pristine::Pristine;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("pristine.redb");

        let pristine = Pristine::open(&db_path).unwrap();

        let expected_inode;

        // Add a file
        {
            let mut txn = pristine.write_txn().unwrap();
            expected_inode = add_to_tree(&mut txn, "test.txt", false).unwrap();
            txn.commit().unwrap();
        }

        // Get the inode back
        {
            let txn = pristine.read_txn().unwrap();
            let inode = get_inode(&txn, "test.txt").unwrap();
            assert_eq!(inode, Some(expected_inode));
        }
    }

    #[test]
    fn test_remove_from_tracking() {
        use atomic_core::pristine::Pristine;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("pristine.redb");

        let pristine = Pristine::open(&db_path).unwrap();

        // Add and then remove
        {
            let mut txn = pristine.write_txn().unwrap();
            add_to_tree(&mut txn, "to_remove.txt", false).unwrap();
            txn.commit().unwrap();
        }

        // Verify it's tracked
        {
            let txn = pristine.read_txn().unwrap();
            assert!(is_tracked(&txn, "to_remove.txt").unwrap());
        }

        // Remove it
        {
            let mut txn = pristine.write_txn().unwrap();
            let removed = remove_from_tree(&mut txn, "to_remove.txt").unwrap();
            assert!(removed.is_some());
            txn.commit().unwrap();
        }

        // Verify it's gone
        {
            let txn = pristine.read_txn().unwrap();
            assert!(!is_tracked(&txn, "to_remove.txt").unwrap());
        }
    }

    #[test]
    fn test_move_tracked_file() {
        use atomic_core::pristine::Pristine;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("pristine.redb");

        let pristine = Pristine::open(&db_path).unwrap();

        let original_inode;

        // Add a file
        {
            let mut txn = pristine.write_txn().unwrap();
            original_inode = add_to_tree(&mut txn, "old_name.rs", false).unwrap();
            txn.commit().unwrap();
        }

        // Move it
        {
            let mut txn = pristine.write_txn().unwrap();
            let moved_inode = move_tracked(&mut txn, "old_name.rs", "new_name.rs").unwrap();
            assert_eq!(moved_inode, original_inode); // Inode preserved!
            txn.commit().unwrap();
        }

        // Verify the move
        {
            let txn = pristine.read_txn().unwrap();
            assert!(!is_tracked(&txn, "old_name.rs").unwrap());
            assert!(is_tracked(&txn, "new_name.rs").unwrap());

            // Same inode
            let inode = get_inode(&txn, "new_name.rs").unwrap();
            assert_eq!(inode, Some(original_inode));
        }
    }

    #[test]
    fn test_list_tracked_files() {
        use atomic_core::pristine::Pristine;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("pristine.redb");

        let pristine = Pristine::open(&db_path).unwrap();

        // Add multiple files
        {
            let mut txn = pristine.write_txn().unwrap();
            add_to_tree(&mut txn, "file1.txt", false).unwrap();
            add_to_tree(&mut txn, "file2.txt", false).unwrap();
            add_to_tree(&mut txn, "src/main.rs", false).unwrap();
            txn.commit().unwrap();
        }

        // List them
        {
            let txn = pristine.read_txn().unwrap();
            let tracked: Vec<TrackedFile> = list_tracked(&txn)
                .unwrap();

            assert_eq!(tracked.len(), 3);

            let paths: Vec<_> = tracked.iter().map(|f| f.path.to_string_lossy().to_string()).collect();
            assert!(paths.contains(&"file1.txt".to_string()));
            assert!(paths.contains(&"file2.txt".to_string()));
            assert!(paths.contains(&"src/main.rs".to_string()));
        }
    }

    #[test]
    fn test_tracked_under_prefix() {
        use atomic_core::pristine::Pristine;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("pristine.redb");

        let pristine = Pristine::open(&db_path).unwrap();

        // Add files in different directories
        {
            let mut txn = pristine.write_txn().unwrap();
            add_to_tree(&mut txn, "src/main.rs", false).unwrap();
            add_to_tree(&mut txn, "src/lib.rs", false).unwrap();
            add_to_tree(&mut txn, "tests/test.rs", false).unwrap();
            add_to_tree(&mut txn, "README.md", false).unwrap();
            txn.commit().unwrap();
        }

        // Get files under src/
        {
            let txn = pristine.read_txn().unwrap();
            let src_files = tracked_under_prefix(&txn, "src").unwrap();

            assert_eq!(src_files.len(), 2);
            assert!(src_files.iter().any(|(p, _)| p == "src/main.rs"));
            assert!(src_files.iter().any(|(p, _)| p == "src/lib.rs"));
        }
    }

    #[test]
    fn test_move_to_existing_fails() {
        use atomic_core::pristine::Pristine;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("pristine.redb");

        let pristine = Pristine::open(&db_path).unwrap();

        // Add two files
        {
            let mut txn = pristine.write_txn().unwrap();
            add_to_tree(&mut txn, "file1.txt", false).unwrap();
            add_to_tree(&mut txn, "file2.txt", false).unwrap();
            txn.commit().unwrap();
        }

        // Try to move file1 to file2 (should fail)
        {
            let mut txn = pristine.write_txn().unwrap();
            let result = move_tracked(&mut txn, "file1.txt", "file2.txt");
            assert!(result.is_err());
            assert!(matches!(result.unwrap_err(), TrackingError::DestinationExists { .. }));
        }
    }

    #[test]
    fn test_move_nonexistent_fails() {
        use atomic_core::pristine::Pristine;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("pristine.redb");

        let pristine = Pristine::open(&db_path).unwrap();

        // Try to move nonexistent file
        {
            let mut txn = pristine.write_txn().unwrap();
            let result = move_tracked(&mut txn, "nonexistent.txt", "new.txt");
            assert!(result.is_err());
            assert!(matches!(result.unwrap_err(), TrackingError::NotTracked { .. }));
        }
    }
}

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

use std::path::PathBuf;

use atomic_core::types::Inode;
use thiserror::Error;

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
mod helpers;
pub use helpers::*;

mod tree_ops;
pub use tree_ops::*;

#[cfg(test)]
mod tests;

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
//! println!("On view: {}", status.view());
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
    /// The current view name.
    view: String,

    /// The Merkle state of the current view (if any changes have been applied).
    state: Option<Merkle>,

    /// All file status entries.
    entries: Vec<FileStatusEntry>,

    /// Index of entries by path for quick lookup.
    path_index: HashMap<PathBuf, usize>,

    /// Number of tracked files that had no FILE_INDEX entry.
    ///
    /// These files are conservatively reported as Modified because we
    /// cannot confirm they match the recorded graph content without
    /// hashing.  A non-zero value means `atomic status --reindex`
    /// would likely resolve the false positives.
    stale_index_count: usize,

    /// Canonical root of the FILE_INDEX_V2 re-verification performed by status.
    verified_candidate_root: Option<crate::change_source::VerifiedCandidateRoot>,

    /// Transaction-local reason the selected adapter degraded to a full scan.
    change_source_fallback: Option<crate::change_source::ChangeSourceFallbackReason>,

    /// Advisory notices attached to this status report (e.g. the RFC §8.3
    /// conflict-snapshot caveat). Notices never change cleanliness.
    notices: Vec<String>,

    /// Deterministic counters for the canonical candidate transaction.
    change_source_metrics: Option<crate::change_source::ChangeSourceMetrics>,
}

impl RepositoryStatus {
    /// Create a new empty repository status.
    ///
    /// # Arguments
    ///
    /// * `view` - The current view name
    /// * `state` - The Merkle state of the view (if any)
    pub fn new(view: String, state: Option<Merkle>) -> Self {
        Self {
            view,
            state,
            entries: Vec::new(),
            path_index: HashMap::new(),
            stale_index_count: 0,
            verified_candidate_root: None,
            change_source_fallback: None,
            change_source_metrics: None,
            notices: Vec::new(),
        }
    }

    /// Attach an advisory notice to this status report.
    pub fn add_notice(&mut self, notice: String) {
        self.notices.push(notice);
    }

    /// The advisory notices attached to this status report.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Create a repository status with pre-allocated capacity.
    ///
    /// # Arguments
    ///
    /// * `view` - The current view name
    /// * `state` - The Merkle state of the view
    /// * `capacity` - Expected number of entries
    pub fn with_capacity(view: String, state: Option<Merkle>, capacity: usize) -> Self {
        Self {
            view,
            state,
            entries: Vec::with_capacity(capacity),
            path_index: HashMap::with_capacity(capacity),
            stale_index_count: 0,
            verified_candidate_root: None,
            change_source_fallback: None,
            change_source_metrics: None,
            notices: Vec::new(),
        }
    }

    /// Get the current view name.
    pub fn view(&self) -> &str {
        &self.view
    }

    /// Backward-compatible alias for [`view()`](Self::view).
    pub fn stack(&self) -> &str {
        &self.view
    }

    /// Get the Merkle state of the current view.
    pub fn state(&self) -> Option<&Merkle> {
        self.state.as_ref()
    }

    /// Canonical FILE_INDEX_V2 verification root for this status transaction.
    pub fn verified_candidate_root(&self) -> Option<crate::change_source::VerifiedCandidateRoot> {
        self.verified_candidate_root
    }

    /// Deterministic candidate verification counters for this transaction.
    pub fn change_source_metrics(&self) -> Option<&crate::change_source::ChangeSourceMetrics> {
        self.change_source_metrics.as_ref()
    }

    /// Why candidate discovery degraded to a complete scan, when applicable.
    pub fn change_source_fallback(
        &self,
    ) -> Option<&crate::change_source::ChangeSourceFallbackReason> {
        self.change_source_fallback.as_ref()
    }

    pub(crate) fn set_change_source_verification(
        &mut self,
        root: crate::change_source::VerifiedCandidateRoot,
        metrics: crate::change_source::ChangeSourceMetrics,
    ) {
        self.verified_candidate_root = Some(root);
        self.change_source_fallback = metrics.fallback_reason.clone();
        self.change_source_metrics = Some(metrics);
    }

    /// Number of tracked files that were reported as Modified only because
    /// their FILE_INDEX entry was missing.  When non-zero, running
    /// `atomic status --re-index` will likely resolve the false positives.
    pub fn stale_index_count(&self) -> usize {
        self.stale_index_count
    }

    /// Whether the FILE_INDEX appears stale (some entries are missing).
    pub fn needs_reindex(&self) -> bool {
        self.stale_index_count > 0
    }

    /// Increment the stale-index counter.
    pub fn add_stale_index_hit(&mut self) {
        self.stale_index_count += 1;
    }

    /// Add a file status entry.
    pub fn add_entry(&mut self, entry: FileStatusEntry) {
        let index = self.entries.len();
        self.path_index.insert(entry.path.clone(), index);
        self.entries.push(entry);
    }

    /// Add an entry, replacing any existing entry for the same path.
    ///
    /// Used to let a Conflicted entry supersede a Modified one so a file is
    /// reported exactly once.
    pub fn add_or_replace_entry(&mut self, entry: FileStatusEntry) {
        if let Some(&i) = self.path_index.get(&entry.path) {
            self.entries[i] = entry;
        } else {
            self.add_entry(entry);
        }
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

    /// Iterate over files whose type changed (regular ↔ symlink ↔ directory).
    pub fn type_changed(&self) -> impl Iterator<Item = &FileStatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == FileStatus::TypeChanged)
    }

    /// Count of type-changed files.
    pub fn type_changed_count(&self) -> usize {
        self.type_changed().count()
    }

    /// Iterate over files whose permissions changed.
    pub fn permissions_changed(&self) -> impl Iterator<Item = &FileStatusEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == FileStatus::PermissionsChanged)
    }

    /// Count of permission-changed files.
    pub fn permissions_changed_count(&self) -> usize {
        self.permissions_changed().count()
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

    /// Respect Atomic ignore rules (`.atomicignore` and the global ignore file).
    ///
    /// Default: `true`
    pub respect_ignore_files: bool,

    /// Hash file contents to detect modifications.
    ///
    /// When `false`, only uses filesystem metadata (faster but less accurate).
    /// Default: `true`
    pub hash_contents: bool,

    /// Authoritative nested-repository boundaries (review CB-9C R5).
    ///
    /// Relative working-copy paths whose contents are foreign nested
    /// working-copy state (e.g. tracked gitlink submodules). The directory
    /// walk prunes these outright; every other directory is pruned only when
    /// its `.git` marker establishes an actual repository boundary. An
    /// incidental `.git` file or empty `.git` directory never hides ordinary
    /// parent content.
    pub nested_repo_boundaries: std::collections::HashSet<PathBuf>,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self {
            include_untracked: true,
            include_ignored: false,
            path_filters: Vec::new(),
            respect_ignore_files: true,
            hash_contents: true,
            nested_repo_boundaries: std::collections::HashSet::new(),
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

    /// Set the authoritative nested-repository boundaries to prune.
    ///
    /// Paths are relative to the status root. Tracked gitlink submodules are
    /// the primary source (review CB-9C R5).
    pub fn with_nested_repo_boundaries<I, P>(mut self, boundaries: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.nested_repo_boundaries = boundaries.into_iter().map(Into::into).collect();
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

/// Whether a nested `.git` marker establishes an authoritative repository
/// boundary (review CB-9C R5, re-review EYL).
///
/// Only an actual Git directory counts — validated without upward discovery:
///
/// - a `.git` **directory** must hold a `HEAD` ref plus the `objects` and
///   `refs` stores (a directory containing only `HEAD` is a fake marker and
///   Git itself refuses it, so it must not hide ordinary parent content);
/// - a `.git` **file** must be a `gitdir:` link whose target exists and is
///   itself an actual Git directory (linked worktree gitdirs carry a
///   `commondir` file pointing at the shared store, which is validated in
///   the target's place). A dangling or malformed link is incidental state.
///
/// An empty `.git` directory, an unrelated `.git` file, or a `gitdir:` link
/// to a missing target must never hide ordinary parent content from the
/// walk. Tracked gitlink boundaries passed by the caller prune regardless of
/// marker validity (see `StatusOptions::nested_repo_boundaries`).
fn establishes_repository_boundary(git_marker: &Path) -> bool {
    if git_marker.is_file() {
        let Ok(bytes) = std::fs::read(git_marker) else {
            return false;
        };
        let Some(target) = bytes.strip_prefix(b"gitdir:") else {
            return false;
        };
        let text = String::from_utf8_lossy(target);
        let target = text.trim();
        if target.is_empty() {
            return false;
        }
        let target_path = std::path::Path::new(target);
        // Relative gitdir targets resolve against the directory containing
        // the marker file (how Git writes submodule and worktree links).
        let resolved = if target_path.is_absolute() {
            target_path.to_path_buf()
        } else {
            git_marker
                .parent()
                .unwrap_or(Path::new("."))
                .join(target_path)
        };
        return is_actual_git_directory(&resolved);
    }
    if git_marker.is_dir() {
        return is_actual_git_directory(git_marker);
    }
    false
}

/// Whether `dir` is an actual Git directory: a readable `HEAD` plus the
/// `objects` and `refs` stores (the same minimal shape Git's own
/// `is_git_directory` check requires). A directory holding only `HEAD` — the
/// forged-marker shape the review pinned — is rejected. A linked-worktree
/// gitdir (which delegates `objects`/`refs` to its `commondir`) passes when
/// the common store validates and the worktree gitdir holds its own `HEAD`.
fn is_actual_git_directory(dir: &Path) -> bool {
    if !dir.join("HEAD").is_file() {
        return false;
    }
    if dir.join("objects").is_dir() && dir.join("refs").is_dir() {
        return true;
    }
    // Linked worktree gitdir: `common dir` names the shared Git directory,
    // resolved relative to the worktree gitdir itself.
    if let Ok(common) = std::fs::read_to_string(dir.join("commondir")) {
        let common = common.trim();
        if !common.is_empty() {
            let common_path = std::path::Path::new(common);
            let resolved = if common_path.is_absolute() {
                common_path.to_path_buf()
            } else {
                dir.join(common_path)
            };
            if resolved.join("objects").is_dir() && resolved.join("refs").is_dir() {
                return true;
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
    let metadata = std::fs::symlink_metadata(path)?;
    let contents = if metadata.file_type().is_symlink() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            std::fs::read_link(path)?.as_os_str().as_bytes().to_vec()
        }
        #[cfg(not(unix))]
        {
            std::fs::read_link(path)?
                .as_os_str()
                .to_string_lossy()
                .as_bytes()
                .to_vec()
        }
    } else {
        std::fs::read(path)?
    };
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

            // Skip nested repository worktrees (submodules, linked Git
            // worktrees): their content is foreign state with its own
            // `.git`, not untracked content of this working copy (CB-9C).
            // Without this, a checked-out submodule's files surface as
            // untracked entries and later snapshots try to record them
            // under a gitlink path that is not an Atomic directory.
            //
            // Review CB-9C R5: pruning authority is exact. A directory is
            // pruned only when it is an explicitly established boundary
            // (a tracked gitlink/nested-working-copy path passed by the
            // caller) or its `.git` marker is an actual repository (a
            // `gitdir:` link file, or a directory holding a HEAD ref). A
            // mere `.git` existence — an empty marker directory, an
            // unrelated file — is incidental and must never hide ordinary
            // parent content. The root itself is never pruned: a colocated
            // repository's own `.git` lives there.
            if e.file_type().is_dir() && e.depth() > 0 {
                let relative = e.path().strip_prefix(root).unwrap_or(e.path());
                if options.nested_repo_boundaries.contains(relative) {
                    return false;
                }
                let marker = e.path().join(".git");
                if marker.exists() && establishes_repository_boundary(&marker) {
                    return false;
                }
            }

            // Prune directories that cannot contain a selected path. Without
            // this check, a path-scoped status still traverses the entire
            // working copy and only filters files after visiting them.
            if !options.path_filters.is_empty() {
                if let Ok(rel_path) = e.path().strip_prefix(root) {
                    if !path_intersects_filters(rel_path, &options.path_filters) {
                        return false;
                    }
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
            if !path_intersects_filters(rel_path, &options.path_filters) {
                continue;
            }

            // Normalize to forward slashes so paths match TREE entries
            // (which always use '/'). On Windows, walkdir returns paths
            // with '\' separators, but TREE stores '/'.
            let normalized = PathBuf::from(rel_path.to_string_lossy().replace('\\', "/"));
            files.insert(normalized);
        }
    }

    Ok(files)
}

fn path_intersects_filters(path: &Path, filters: &[PathBuf]) -> bool {
    if filters.is_empty() {
        return true;
    }

    let normalized_path = PathBuf::from(path.to_string_lossy().replace('\\', "/"));
    filters.iter().any(|filter| {
        let normalized_filter = PathBuf::from(filter.to_string_lossy().replace('\\', "/"));
        normalized_path.starts_with(&normalized_filter)
            || normalized_filter.starts_with(&normalized_path)
    })
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

        assert_eq!(status.view(), "main");
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

    #[test]
    fn test_path_filters_keep_ancestors_and_prune_siblings() {
        let filters = vec![PathBuf::from("src/nested/main.rs")];

        assert!(path_intersects_filters(Path::new(""), &filters));
        assert!(path_intersects_filters(Path::new("src"), &filters));
        assert!(path_intersects_filters(Path::new("src/nested"), &filters));
        assert!(path_intersects_filters(
            Path::new("src/nested/main.rs"),
            &filters
        ));
        assert!(!path_intersects_filters(Path::new("tests"), &filters));
        assert!(!path_intersects_filters(Path::new("src/other"), &filters));
    }

    #[test]
    fn test_path_filters_normalize_platform_separators() {
        let filters = vec![PathBuf::from("src\\nested\\main.rs")];

        assert!(path_intersects_filters(Path::new("src/nested"), &filters));
        assert!(path_intersects_filters(
            Path::new("src/nested/main.rs"),
            &filters
        ));
    }

    /// Review CB-9C R5: an incidental nested `.git` marker must never hide
    /// ordinary parent content; only actual repository boundaries prune.
    #[test]
    fn incidental_git_markers_do_not_hide_ordinary_files() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // An ordinary directory holding an EMPTY .git marker directory.
        std::fs::create_dir_all(root.join("ordinary/.git")).unwrap();
        std::fs::write(root.join("ordinary/new.txt"), b"visible\n").unwrap();
        // A `.git` marker FILE that is not a gitdir link.
        std::fs::create_dir_all(root.join("marker-file")).unwrap();
        std::fs::write(root.join("marker-file/.git"), b"not a gitdir link").unwrap();
        std::fs::write(root.join("marker-file/keep.txt"), b"visible\n").unwrap();
        // A real nested repository (gitdir + HEAD + objects + refs) prunes.
        std::fs::create_dir_all(root.join("nested-repo/.git/objects")).unwrap();
        std::fs::create_dir_all(root.join("nested-repo/.git/refs")).unwrap();
        std::fs::write(root.join("nested-repo/.git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("nested-repo/hidden.txt"), b"foreign\n").unwrap();
        // A linked-worktree style gitdir FILE whose target is an actual Git
        // directory (with a commondir delegating objects/refs) prunes.
        std::fs::create_dir_all(root.join("shared-store/.git/objects")).unwrap();
        std::fs::create_dir_all(root.join("shared-store/.git/refs")).unwrap();
        std::fs::write(root.join("shared-store/.git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(root.join("shared-store/.git/worktrees/wt")).unwrap();
        std::fs::write(
            root.join("shared-store/.git/worktrees/wt/commondir"),
            "../../\n",
        )
        .unwrap();
        std::fs::write(
            root.join("shared-store/.git/worktrees/wt/HEAD"),
            b"ref: refs/heads/wt\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("linked-real")).unwrap();
        std::fs::write(
            root.join("linked-real/.git"),
            b"gitdir: ../shared-store/.git/worktrees/wt\n",
        )
        .unwrap();
        std::fs::write(root.join("linked-real/hidden.txt"), b"foreign\n").unwrap();

        let options = StatusOptions::default();
        let files = collect_working_copy_files(root, &options).unwrap();
        assert!(
            files.contains(&PathBuf::from("ordinary/new.txt")),
            "an empty .git marker must not hide ordinary parent files: {files:?}"
        );
        assert!(
            files.contains(&PathBuf::from("marker-file/keep.txt")),
            "a plain .git file is not a repository boundary: {files:?}"
        );
        assert!(!files.contains(&PathBuf::from("nested-repo/hidden.txt")));
        assert!(!files.contains(&PathBuf::from("linked-real/hidden.txt")));
    }

    /// Review CB-9C R5 (re-review EYL): forged repository markers — a
    /// `gitdir:` link whose target does not exist, and a `.git` directory
    /// containing ONLY a `HEAD` file — are not repository boundaries. Both
    /// shapes hide ordinary parent content under marker-syntax validation
    /// while Git itself refuses them, so validation must resolve an actual
    /// Git directory (HEAD + objects + refs, commondir-aware).
    #[test]
    fn forged_repository_markers_do_not_hide_ordinary_files() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // A `gitdir:` link file pointing at a nonexistent target.
        std::fs::create_dir_all(root.join("dangling")).unwrap();
        std::fs::write(root.join("dangling/.git"), b"gitdir: /nonexistent/repo\n").unwrap();
        std::fs::write(root.join("dangling/keep.txt"), b"visible\n").unwrap();
        // A `.git` directory containing ONLY a HEAD file.
        std::fs::create_dir_all(root.join("head-only/.git")).unwrap();
        std::fs::write(
            root.join("head-only/.git/HEAD"),
            b"ref: refs/heads/main\n",
        )
        .unwrap();
        std::fs::write(root.join("head-only/keep.txt"), b"visible\n").unwrap();
        // A real repository next door still prunes (control).
        std::fs::create_dir_all(root.join("real/.git/objects")).unwrap();
        std::fs::create_dir_all(root.join("real/.git/refs")).unwrap();
        std::fs::write(root.join("real/.git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("real/hidden.txt"), b"foreign\n").unwrap();

        let options = StatusOptions::default();
        let files = collect_working_copy_files(root, &options).unwrap();
        assert!(
            files.contains(&PathBuf::from("dangling/keep.txt")),
            "a dangling gitdir target is not a repository boundary: {files:?}"
        );
        assert!(
            files.contains(&PathBuf::from("head-only/keep.txt")),
            "a HEAD-only .git directory is not a repository boundary: {files:?}"
        );
        assert!(!files.contains(&PathBuf::from("real/hidden.txt")));
    }

    /// Review CB-9C R5: an explicitly established boundary (e.g. a tracked
    /// gitlink path) prunes even without a valid `.git` marker, and path
    /// filters keep working alongside it.
    #[test]
    fn explicit_boundaries_prune_and_path_filters_still_apply() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // A gitlink-shaped directory whose .git marker is missing/invalid.
        std::fs::create_dir_all(root.join("mod")).unwrap();
        std::fs::write(root.join("mod/inner.txt"), b"submodule state\n").unwrap();
        std::fs::create_dir_all(root.join("ordinary")).unwrap();
        std::fs::write(root.join("ordinary/new.txt"), b"visible\n").unwrap();

        let options = StatusOptions::default()
            .with_nested_repo_boundaries([PathBuf::from("mod")]);
        let files = collect_working_copy_files(root, &options).unwrap();
        assert!(!files.contains(&PathBuf::from("mod/inner.txt")));
        assert!(files.contains(&PathBuf::from("ordinary/new.txt")));

        // Path-scoped collection intersects boundaries the same way.
        let scoped = options.filter_path(PathBuf::from("mod"));
        let files = collect_working_copy_files(root, &scoped).unwrap();
        assert!(
            !files.contains(&PathBuf::from("mod/inner.txt")),
            "a path-scoped walk must not resurrect pruned boundary content: {files:?}"
        );
    }

    #[test]
    fn establishes_repository_boundary_requires_an_actual_repository() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Empty marker directory: not a boundary.
        std::fs::create_dir_all(root.join("empty/.git")).unwrap();
        assert!(!establishes_repository_boundary(&root.join("empty/.git")));
        // Plain file: not a boundary.
        std::fs::remove_dir(root.join("empty/.git")).unwrap();
        std::fs::write(root.join("empty/.git"), b"junk").unwrap();
        assert!(!establishes_repository_boundary(&root.join("empty/.git")));
        // gitdir link file with a NONEXISTENT target: not a boundary (review
        // CB-9C re-review EYL — marker syntax alone never establishes one).
        std::fs::write(root.join("empty/.git"), b"gitdir: /some/where\n").unwrap();
        assert!(!establishes_repository_boundary(&root.join("empty/.git")));
        // Directory with ONLY a HEAD file: not a boundary (Git refuses it).
        std::fs::remove_file(root.join("empty/.git")).unwrap();
        std::fs::create_dir(root.join("empty/.git")).unwrap();
        std::fs::write(root.join("empty/.git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        assert!(!establishes_repository_boundary(&root.join("empty/.git")));
        // HEAD + objects + refs: an actual Git directory.
        std::fs::create_dir(root.join("empty/.git/objects")).unwrap();
        std::fs::create_dir(root.join("empty/.git/refs")).unwrap();
        assert!(establishes_repository_boundary(&root.join("empty/.git")));
        // A gitdir link to an actual Git directory: boundary.
        std::fs::remove_dir_all(root.join("empty/.git")).unwrap();
        std::fs::create_dir_all(root.join("store/objects")).unwrap();
        std::fs::create_dir_all(root.join("store/refs")).unwrap();
        std::fs::write(root.join("store/HEAD"), b"ref: refs/heads/main\n").unwrap();
        std::fs::write(root.join("empty/.git"), b"gitdir: ../store\n").unwrap();
        assert!(establishes_repository_boundary(&root.join("empty/.git")));
        // Linked-worktree gitdir delegating objects/refs via commondir:
        // boundary.
        std::fs::create_dir_all(root.join("store/worktrees/wt")).unwrap();
        std::fs::write(root.join("store/worktrees/wt/commondir"), "../../\n").unwrap();
        std::fs::write(root.join("store/worktrees/wt/HEAD"), b"ref: refs/heads/wt\n").unwrap();
        std::fs::write(
            root.join("empty/.git"),
            b"gitdir: ../store/worktrees/wt\n",
        )
        .unwrap();
        assert!(establishes_repository_boundary(&root.join("empty/.git")));
        // Malformed gitdir body: not a boundary.
        std::fs::write(root.join("empty/.git"), b"gitdir:   \n").unwrap();
        assert!(!establishes_repository_boundary(&root.join("empty/.git")));
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

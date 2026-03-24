//! Tree traversal for repository output.
//!
//! This module provides functionality for traversing the repository tree
//! structure to collect files and directories that need to be output to
//! the working copy.
//!
//! # Overview
//!
//! The repository tree maps paths to inodes, and inodes to graph positions.
//! When outputting the repository, we need to:
//!
//! 1. Traverse the tree to find all files under a given prefix
//! 2. Resolve each file's inode to its graph position
//! 3. Determine the file type and metadata
//! 4. Collect items for output processing
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                       Tree Traversal Pipeline                            │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │                                                                         │
//! │  TREE Table              Processing                  OutputItems        │
//! │  ┌──────────────┐       ┌─────────────────┐        ┌────────────────┐  │
//! │  │ path → inode │ ────► │ For each entry: │ ─────► │ Files to write │  │
//! │  └──────────────┘       │ 1. Check prefix │        │ Dirs to create │  │
//! │                         │ 2. Get position │        └────────────────┘  │
//! │  INODES Table           │ 3. Get metadata │                            │
//! │  ┌──────────────┐       │ 4. Build item   │                            │
//! │  │ inode → pos  │ ────► └─────────────────┘                            │
//! │  └──────────────┘                                                      │
//! │                                                                         │
//! │  Edge Flags:                                                            │
//! │  - FOLDER: Directory edges in the graph                                 │
//! │  - PSEUDO: Transitive edges (can be ignored for tree structure)        │
//! │  - DELETED: Removed content (skip unless include_deleted)              │
//! │                                                                         │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::output::repo::tree::{collect_tree_children, TreeCollectOptions};
//!
//! // Collect all files in the repository
//! let items = collect_tree_children(&txn, Inode::ROOT, "", TreeCollectOptions::new())?;
//!
//! for item in items {
//!     if item.is_directory {
//!         println!("Directory: {}", item.path);
//!     } else {
//!         println!("File: {} (inode {})", item.path, item.inode);
//!     }
//! }
//!
//! // Collect only files under src/
//! let options = TreeCollectOptions::new().prefix("src/");
//! let src_items = collect_tree_children(&txn, Inode::ROOT, "", options)?;
//! ```
//!
//! # Performance
//!
//! Tree traversal is O(n) where n is the number of tracked files. The TREE
//! table provides efficient path-based lookup, and the INODES table provides
//! O(1) inode-to-position resolution.

use std::collections::{HashMap, HashSet};

use crate::output::traits::FileMetadata;
use crate::pristine::{GraphTxnT, PristineError, TreeTxnT};
use crate::types::{Inode, NodeId, Position};

// ============================================================================
// TREE COLLECT OPTIONS
// ============================================================================

/// Options for tree collection operations.
///
/// Controls which files are collected and how they are filtered.
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::TreeCollectOptions;
///
/// // Default options - collect everything
/// let opts = TreeCollectOptions::new();
///
/// // Filter by prefix
/// let opts = TreeCollectOptions::new().prefix("src/lib/");
///
/// // Include hidden files
/// let opts = TreeCollectOptions::new().include_hidden(true);
///
/// // Limit depth
/// let opts = TreeCollectOptions::new().max_depth(3);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeCollectOptions {
    /// Prefix to filter paths.
    ///
    /// Only paths starting with this prefix will be collected.
    /// Empty string means all paths.
    pub prefix: String,

    /// Whether to include hidden files (starting with '.').
    ///
    /// Defaults to true.
    pub include_hidden: bool,

    /// Maximum depth to traverse.
    ///
    /// `None` means unlimited depth.
    pub max_depth: Option<usize>,

    /// Whether to collect directories.
    ///
    /// Defaults to true.
    pub collect_directories: bool,

    /// Whether to collect files.
    ///
    /// Defaults to true.
    pub collect_files: bool,

    /// Patterns to exclude.
    ///
    /// Paths matching any of these patterns will be skipped.
    pub exclude_patterns: Vec<String>,
}

impl TreeCollectOptions {
    /// Create new options with defaults.
    ///
    /// Default configuration:
    /// - No prefix filter
    /// - Include hidden files
    /// - Unlimited depth
    /// - Collect both files and directories
    /// - No exclude patterns
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// let opts = TreeCollectOptions::new();
    /// assert!(opts.prefix.is_empty());
    /// assert!(opts.include_hidden);
    /// assert!(opts.max_depth.is_none());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the prefix filter.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path prefix to filter
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// let opts = TreeCollectOptions::new().prefix("src/");
    /// assert_eq!(opts.prefix, "src/");
    /// ```
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Set whether to include hidden files.
    ///
    /// # Arguments
    ///
    /// * `include` - Whether to include files starting with '.'
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// let opts = TreeCollectOptions::new().include_hidden(false);
    /// assert!(!opts.include_hidden);
    /// ```
    pub fn include_hidden(mut self, include: bool) -> Self {
        self.include_hidden = include;
        self
    }

    /// Set the maximum traversal depth.
    ///
    /// # Arguments
    ///
    /// * `depth` - Maximum depth (0 = only root level)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// let opts = TreeCollectOptions::new().max_depth(2);
    /// assert_eq!(opts.max_depth, Some(2));
    /// ```
    pub fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = Some(depth);
        self
    }

    /// Set whether to collect directories.
    ///
    /// # Arguments
    ///
    /// * `collect` - Whether to include directories in results
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// let opts = TreeCollectOptions::new().collect_directories(false);
    /// assert!(!opts.collect_directories);
    /// ```
    pub fn collect_directories(mut self, collect: bool) -> Self {
        self.collect_directories = collect;
        self
    }

    /// Set whether to collect files.
    ///
    /// # Arguments
    ///
    /// * `collect` - Whether to include files in results
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// let opts = TreeCollectOptions::new().collect_files(false);
    /// assert!(!opts.collect_files);
    /// ```
    pub fn collect_files(mut self, collect: bool) -> Self {
        self.collect_files = collect;
        self
    }

    /// Add an exclude pattern.
    ///
    /// # Arguments
    ///
    /// * `pattern` - Pattern to exclude (simple prefix matching)
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// let opts = TreeCollectOptions::new()
    ///     .exclude("target/")
    ///     .exclude(".git/");
    /// assert_eq!(opts.exclude_patterns.len(), 2);
    /// ```
    pub fn exclude(mut self, pattern: impl Into<String>) -> Self {
        self.exclude_patterns.push(pattern.into());
        self
    }

    /// Check if a path matches the prefix filter.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to check
    ///
    /// # Returns
    ///
    /// `true` if the path is under the prefix.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// let opts = TreeCollectOptions::new().prefix("src/");
    /// assert!(opts.matches_prefix("src/main.rs"));
    /// assert!(opts.matches_prefix("src/lib/mod.rs"));
    /// assert!(!opts.matches_prefix("tests/test.rs"));
    /// ```
    pub fn matches_prefix(&self, path: &str) -> bool {
        if self.prefix.is_empty() {
            true
        } else {
            path.starts_with(&self.prefix)
        }
    }

    /// Check if a path should be excluded.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to check
    ///
    /// # Returns
    ///
    /// `true` if the path matches any exclude pattern.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// let opts = TreeCollectOptions::new().exclude("target/");
    /// assert!(opts.is_excluded("target/debug/main"));
    /// assert!(!opts.is_excluded("src/main.rs"));
    /// ```
    pub fn is_excluded(&self, path: &str) -> bool {
        for pattern in &self.exclude_patterns {
            if path.starts_with(pattern) || path.contains(pattern) {
                return true;
            }
        }
        false
    }

    /// Check if a filename is hidden.
    ///
    /// # Arguments
    ///
    /// * `name` - Filename (not full path)
    ///
    /// # Returns
    ///
    /// `true` if the filename starts with '.'.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// assert!(TreeCollectOptions::is_hidden_name(".gitignore"));
    /// assert!(TreeCollectOptions::is_hidden_name(".hidden"));
    /// assert!(!TreeCollectOptions::is_hidden_name("visible.txt"));
    /// ```
    pub fn is_hidden_name(name: &str) -> bool {
        name.starts_with('.')
    }

    /// Check if a path should be included based on all filters.
    ///
    /// # Arguments
    ///
    /// * `path` - Full path to check
    ///
    /// # Returns
    ///
    /// `true` if the path passes all filters.
    pub fn should_include(&self, path: &str) -> bool {
        // Check prefix
        if !self.matches_prefix(path) {
            return false;
        }

        // Check exclude patterns
        if self.is_excluded(path) {
            return false;
        }

        // Check hidden
        if !self.include_hidden {
            if let Some(name) = path.rsplit('/').next() {
                if Self::is_hidden_name(name) {
                    return false;
                }
            }
        }

        true
    }

    /// Get the depth of a path.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to measure
    ///
    /// # Returns
    ///
    /// Number of path components (0 for empty path).
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeCollectOptions;
    ///
    /// assert_eq!(TreeCollectOptions::path_depth(""), 0);
    /// assert_eq!(TreeCollectOptions::path_depth("file.txt"), 1);
    /// assert_eq!(TreeCollectOptions::path_depth("src/main.rs"), 2);
    /// assert_eq!(TreeCollectOptions::path_depth("a/b/c/d.rs"), 4);
    /// ```
    pub fn path_depth(path: &str) -> usize {
        if path.is_empty() {
            0
        } else {
            path.split('/').filter(|s| !s.is_empty()).count()
        }
    }

    /// Check if a path exceeds the maximum depth.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to check
    ///
    /// # Returns
    ///
    /// `true` if the path exceeds max_depth.
    pub fn exceeds_depth(&self, path: &str) -> bool {
        if let Some(max) = self.max_depth {
            Self::path_depth(path) > max
        } else {
            false
        }
    }
}

impl Default for TreeCollectOptions {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            include_hidden: true,
            max_depth: None,
            collect_directories: true,
            collect_files: true,
            exclude_patterns: Vec::new(),
        }
    }
}

// ============================================================================
// TREE ITEM
// ============================================================================

/// An item collected from the tree.
///
/// Represents either a file or directory that was found during tree traversal.
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::TreeItem;
/// use atomic_core::types::{Inode, Position, NodeId};
///
/// // Create a file item
/// let file = TreeItem::file("src/main.rs", Inode::ROOT, Position::ROOT);
/// assert!(!file.is_directory);
/// assert!(file.is_file());
///
/// // Create a directory item
/// let dir = TreeItem::directory("src/lib", Inode::ROOT);
/// assert!(dir.is_directory);
/// assert!(!dir.is_file());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeItem {
    /// Path relative to the repository root.
    pub path: String,

    /// Inode for this item.
    pub inode: Inode,

    /// Position in the graph (for files).
    ///
    /// This is the root position of the file's content in the graph.
    /// For directories, this is Position::ROOT.
    pub position: Position<NodeId>,

    /// Whether this is a directory.
    pub is_directory: bool,

    /// File metadata (permissions, type).
    pub metadata: FileMetadata,

    /// Depth in the tree (0 = root level).
    pub depth: usize,
}

impl TreeItem {
    /// Create a new file item.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to repository root
    /// * `inode` - File's stable identifier
    /// * `position` - Root position in the graph
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeItem;
    /// use atomic_core::types::{Inode, Position};
    ///
    /// let item = TreeItem::file("src/main.rs", Inode::ROOT, Position::ROOT);
    /// assert_eq!(item.path, "src/main.rs");
    /// assert!(!item.is_directory);
    /// ```
    pub fn file(path: impl Into<String>, inode: Inode, position: Position<NodeId>) -> Self {
        let path = path.into();
        let depth = TreeCollectOptions::path_depth(&path);
        Self {
            path,
            inode,
            position,
            is_directory: false,
            metadata: FileMetadata::file(),
            depth,
        }
    }

    /// Create a new directory item.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to repository root
    /// * `inode` - Directory's stable identifier
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeItem;
    /// use atomic_core::types::Inode;
    ///
    /// let item = TreeItem::directory("src/lib", Inode::ROOT);
    /// assert_eq!(item.path, "src/lib");
    /// assert!(item.is_directory);
    /// ```
    pub fn directory(path: impl Into<String>, inode: Inode) -> Self {
        let path = path.into();
        let depth = TreeCollectOptions::path_depth(&path);
        Self {
            path,
            inode,
            position: Position::ROOT,
            is_directory: true,
            metadata: FileMetadata::directory(),
            depth,
        }
    }

    /// Check if this is a file.
    ///
    /// # Returns
    ///
    /// `true` if this item represents a file.
    pub fn is_file(&self) -> bool {
        !self.is_directory
    }

    /// Set the metadata for this item.
    ///
    /// # Arguments
    ///
    /// * `metadata` - File or directory metadata
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::output::repo::TreeItem;
    /// use atomic_core::output::traits::FileMetadata;
    /// use atomic_core::types::{Inode, Position};
    ///
    /// let item = TreeItem::file("script.sh", Inode::ROOT, Position::ROOT)
    ///     .with_metadata(FileMetadata::executable());
    /// assert!(item.metadata.is_executable());
    /// ```
    pub fn with_metadata(mut self, metadata: FileMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Get the filename (last component of path).
    ///
    /// # Returns
    ///
    /// The filename without the directory path.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeItem;
    /// use atomic_core::types::{Inode, Position};
    ///
    /// let item = TreeItem::file("src/lib/mod.rs", Inode::ROOT, Position::ROOT);
    /// assert_eq!(item.filename(), "mod.rs");
    /// ```
    pub fn filename(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// Get the parent directory path.
    ///
    /// # Returns
    ///
    /// The parent directory, or empty string if at root.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeItem;
    /// use atomic_core::types::{Inode, Position};
    ///
    /// let item = TreeItem::file("src/lib/mod.rs", Inode::ROOT, Position::ROOT);
    /// assert_eq!(item.parent_path(), "src/lib");
    ///
    /// let root_file = TreeItem::file("Cargo.toml", Inode::ROOT, Position::ROOT);
    /// assert_eq!(root_file.parent_path(), "");
    /// ```
    pub fn parent_path(&self) -> &str {
        if let Some(idx) = self.path.rfind('/') {
            &self.path[..idx]
        } else {
            ""
        }
    }

    /// Check if this item is hidden (filename starts with '.').
    ///
    /// # Returns
    ///
    /// `true` if the filename starts with '.'.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_core::output::repo::TreeItem;
    /// use atomic_core::types::{Inode, Position};
    ///
    /// let hidden = TreeItem::file(".gitignore", Inode::ROOT, Position::ROOT);
    /// assert!(hidden.is_hidden());
    ///
    /// let visible = TreeItem::file("README.md", Inode::ROOT, Position::ROOT);
    /// assert!(!visible.is_hidden());
    /// ```
    pub fn is_hidden(&self) -> bool {
        TreeCollectOptions::is_hidden_name(self.filename())
    }
}

// ============================================================================
// TREE COLLECT RESULT
// ============================================================================

/// Result of a tree collection operation.
///
/// Contains the collected items and statistics about the traversal.
#[derive(Debug, Clone, Default)]
pub struct TreeCollectResult {
    /// Collected items (files and directories).
    pub items: Vec<TreeItem>,

    /// Number of files collected.
    pub files_count: usize,

    /// Number of directories collected.
    pub directories_count: usize,

    /// Number of items skipped due to filters.
    pub skipped_count: usize,

    /// Maximum depth encountered.
    pub max_depth_reached: usize,

    /// Paths that had errors during collection.
    pub errors: Vec<(String, String)>,
}

impl TreeCollectResult {
    /// Create a new empty result.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a file item.
    pub fn add_file(&mut self, item: TreeItem) {
        if item.depth > self.max_depth_reached {
            self.max_depth_reached = item.depth;
        }
        self.files_count += 1;
        self.items.push(item);
    }

    /// Add a directory item.
    pub fn add_directory(&mut self, item: TreeItem) {
        if item.depth > self.max_depth_reached {
            self.max_depth_reached = item.depth;
        }
        self.directories_count += 1;
        self.items.push(item);
    }

    /// Record a skipped item.
    pub fn record_skipped(&mut self) {
        self.skipped_count += 1;
    }

    /// Record an error.
    pub fn record_error(&mut self, path: String, error: String) {
        self.errors.push((path, error));
    }

    /// Check if any errors occurred.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Get the total number of items collected.
    pub fn total_items(&self) -> usize {
        self.items.len()
    }

    /// Get only files from the result.
    pub fn files(&self) -> impl Iterator<Item = &TreeItem> {
        self.items.iter().filter(|item| item.is_file())
    }

    /// Get only directories from the result.
    pub fn directories(&self) -> impl Iterator<Item = &TreeItem> {
        self.items.iter().filter(|item| item.is_directory)
    }

    /// Sort items by path.
    pub fn sort_by_path(&mut self) {
        self.items.sort_by(|a, b| a.path.cmp(&b.path));
    }

    /// Sort items by depth (shallowest first).
    pub fn sort_by_depth(&mut self) {
        self.items.sort_by(|a, b| a.depth.cmp(&b.depth));
    }
}

// ============================================================================
// TREE COLLECTION FUNCTIONS
// ============================================================================

/// Collect children from the tree under a given path.
///
/// This function traverses the TREE table to find all files and directories
/// under the specified parent path, applying the given options for filtering.
///
/// # Arguments
///
/// * `txn` - Transaction providing tree access
/// * `parent_path` - Path to start from (empty for root)
/// * `options` - Collection options
///
/// # Returns
///
/// A `TreeCollectResult` containing all matching items.
///
/// # Errors
///
/// Returns an error if database access fails.
///
/// # Example
///
/// ```rust,ignore
/// use atomic_core::output::repo::tree::{collect_tree, TreeCollectOptions};
///
/// // Collect all files
/// let result = collect_tree(&txn, "", TreeCollectOptions::new())?;
/// println!("Found {} files and {} directories",
///     result.files_count, result.directories_count);
///
/// // Collect only src/ files
/// let options = TreeCollectOptions::new().prefix("src/");
/// let src_result = collect_tree(&txn, "", options)?;
/// ```
pub fn collect_tree<T: TreeTxnT + GraphTxnT>(
    txn: &T,
    parent_path: &str,
    options: TreeCollectOptions,
) -> Result<TreeCollectResult, PristineError> {
    let mut result = TreeCollectResult::new();

    // Track directories we've seen to avoid duplicates
    let mut seen_directories: HashSet<String> = HashSet::new();

    // Iterate over all tree entries
    for entry in txn.iter_tree()? {
        let (path, inode) = entry?;

        // Skip if path doesn't start with parent_path
        if !parent_path.is_empty() && !path.starts_with(parent_path) {
            continue;
        }

        // Check filters
        if !options.should_include(&path) {
            result.record_skipped();
            continue;
        }

        // Check depth
        if options.exceeds_depth(&path) {
            result.record_skipped();
            continue;
        }

        // Collect parent directories if needed
        if options.collect_directories {
            let mut current = String::new();
            for component in path.split('/') {
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(component);

                // Don't add the file itself as a directory
                if current == path {
                    break;
                }

                // Check if we should include this directory
                if !seen_directories.contains(&current) && options.should_include(&current) {
                    if !options.exceeds_depth(&current) {
                        seen_directories.insert(current.clone());
                        // We don't have the inode for intermediate directories
                        // In a full implementation, we'd look this up
                        result.add_directory(TreeItem::directory(&current, Inode::ROOT));
                    }
                }
            }
        }

        // Collect the file if we're collecting files
        if options.collect_files {
            // Get the graph position for this inode
            let position = match txn.inode_position(inode)? {
                Some(pos) => pos,
                None => {
                    result.record_error(path.clone(), "No graph position for inode".to_string());
                    continue;
                }
            };

            result.add_file(TreeItem::file(path, inode, position));
        }
    }

    Ok(result)
}

/// Collect only files from the tree.
///
/// Convenience function that collects only files, not directories.
///
/// # Arguments
///
/// * `txn` - Transaction providing tree access
/// * `prefix` - Optional prefix filter
///
/// # Returns
///
/// A vector of file TreeItems.
pub fn collect_files<T: TreeTxnT + GraphTxnT>(
    txn: &T,
    prefix: &str,
) -> Result<Vec<TreeItem>, PristineError> {
    let options = TreeCollectOptions::new()
        .prefix(prefix)
        .collect_directories(false);

    let result = collect_tree(txn, "", options)?;
    Ok(result.items)
}

/// Collect only directories from the tree.
///
/// Convenience function that collects only directories, not files.
///
/// # Arguments
///
/// * `txn` - Transaction providing tree access
/// * `prefix` - Optional prefix filter
///
/// # Returns
///
/// A vector of directory TreeItems.
pub fn collect_directories<T: TreeTxnT + GraphTxnT>(
    txn: &T,
    prefix: &str,
) -> Result<Vec<TreeItem>, PristineError> {
    let options = TreeCollectOptions::new()
        .prefix(prefix)
        .collect_files(false);

    let result = collect_tree(txn, "", options)?;
    Ok(result.items)
}

/// Build a tree structure from flat paths.
///
/// Organizes items into a hierarchical structure by parent-child relationships.
///
/// # Arguments
///
/// * `items` - Flat list of tree items
///
/// # Returns
///
/// A map from parent path to child items.
///
/// # Example
///
/// ```rust
/// use atomic_core::output::repo::{build_tree_hierarchy, TreeItem};
/// use atomic_core::types::{Inode, Position};
///
/// let items = vec![
///     TreeItem::file("src/main.rs", Inode::ROOT, Position::ROOT),
///     TreeItem::file("src/lib.rs", Inode::ROOT, Position::ROOT),
///     TreeItem::file("Cargo.toml", Inode::ROOT, Position::ROOT),
/// ];
///
/// let hierarchy = build_tree_hierarchy(&items);
///
/// // Root level has Cargo.toml
/// assert!(hierarchy.get("").is_some());
/// // src/ has main.rs and lib.rs
/// assert!(hierarchy.get("src").is_some());
/// ```
pub fn build_tree_hierarchy(items: &[TreeItem]) -> HashMap<String, Vec<&TreeItem>> {
    let mut hierarchy: HashMap<String, Vec<&TreeItem>> = HashMap::new();

    for item in items {
        let parent = item.parent_path().to_string();
        hierarchy.entry(parent).or_default().push(item);
    }

    hierarchy
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // TreeCollectOptions Tests
    // ========================================================================

    #[test]
    fn test_options_new() {
        let opts = TreeCollectOptions::new();

        assert!(opts.prefix.is_empty());
        assert!(opts.include_hidden);
        assert!(opts.max_depth.is_none());
        assert!(opts.collect_directories);
        assert!(opts.collect_files);
        assert!(opts.exclude_patterns.is_empty());
    }

    #[test]
    fn test_options_default() {
        let opts = TreeCollectOptions::default();

        assert!(opts.prefix.is_empty());
        assert!(opts.include_hidden);
    }

    #[test]
    fn test_options_prefix() {
        let opts = TreeCollectOptions::new().prefix("src/");

        assert_eq!(opts.prefix, "src/");
    }

    #[test]
    fn test_options_include_hidden() {
        let opts = TreeCollectOptions::new().include_hidden(false);

        assert!(!opts.include_hidden);
    }

    #[test]
    fn test_options_max_depth() {
        let opts = TreeCollectOptions::new().max_depth(3);

        assert_eq!(opts.max_depth, Some(3));
    }

    #[test]
    fn test_options_collect_directories() {
        let opts = TreeCollectOptions::new().collect_directories(false);

        assert!(!opts.collect_directories);
    }

    #[test]
    fn test_options_collect_files() {
        let opts = TreeCollectOptions::new().collect_files(false);

        assert!(!opts.collect_files);
    }

    #[test]
    fn test_options_exclude() {
        let opts = TreeCollectOptions::new()
            .exclude("target/")
            .exclude(".git/");

        assert_eq!(opts.exclude_patterns.len(), 2);
        assert!(opts.exclude_patterns.contains(&"target/".to_string()));
        assert!(opts.exclude_patterns.contains(&".git/".to_string()));
    }

    #[test]
    fn test_options_chaining() {
        let opts = TreeCollectOptions::new()
            .prefix("src/")
            .include_hidden(false)
            .max_depth(5)
            .collect_directories(true)
            .collect_files(true)
            .exclude("target/");

        assert_eq!(opts.prefix, "src/");
        assert!(!opts.include_hidden);
        assert_eq!(opts.max_depth, Some(5));
        assert!(opts.collect_directories);
        assert!(opts.collect_files);
        assert_eq!(opts.exclude_patterns.len(), 1);
    }

    #[test]
    fn test_options_matches_prefix_empty() {
        let opts = TreeCollectOptions::new();

        assert!(opts.matches_prefix("anything"));
        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix(""));
    }

    #[test]
    fn test_options_matches_prefix_with_prefix() {
        let opts = TreeCollectOptions::new().prefix("src/");

        assert!(opts.matches_prefix("src/main.rs"));
        assert!(opts.matches_prefix("src/lib/mod.rs"));
        assert!(!opts.matches_prefix("tests/test.rs"));
        assert!(!opts.matches_prefix("Cargo.toml"));
    }

    #[test]
    fn test_options_is_excluded() {
        let opts = TreeCollectOptions::new()
            .exclude("target/")
            .exclude(".git/");

        assert!(opts.is_excluded("target/debug/main"));
        assert!(opts.is_excluded(".git/config"));
        assert!(!opts.is_excluded("src/main.rs"));
    }

    #[test]
    fn test_options_is_hidden_name() {
        assert!(TreeCollectOptions::is_hidden_name(".gitignore"));
        assert!(TreeCollectOptions::is_hidden_name(".hidden"));
        assert!(TreeCollectOptions::is_hidden_name(".."));
        assert!(!TreeCollectOptions::is_hidden_name("visible.txt"));
        assert!(!TreeCollectOptions::is_hidden_name("file.gitignore"));
    }

    #[test]
    fn test_options_should_include() {
        let opts = TreeCollectOptions::new()
            .prefix("src/")
            .include_hidden(false)
            .exclude("target/");

        assert!(opts.should_include("src/main.rs"));
        assert!(!opts.should_include("tests/test.rs")); // wrong prefix
        assert!(!opts.should_include("src/.hidden")); // hidden
        assert!(!opts.should_include("target/debug")); // excluded
    }

    #[test]
    fn test_options_path_depth() {
        assert_eq!(TreeCollectOptions::path_depth(""), 0);
        assert_eq!(TreeCollectOptions::path_depth("file.txt"), 1);
        assert_eq!(TreeCollectOptions::path_depth("src/main.rs"), 2);
        assert_eq!(TreeCollectOptions::path_depth("a/b/c/d.rs"), 4);
        assert_eq!(TreeCollectOptions::path_depth("a/b/c/"), 3);
    }

    #[test]
    fn test_options_exceeds_depth() {
        let opts = TreeCollectOptions::new().max_depth(2);

        assert!(!opts.exceeds_depth("file.txt")); // depth 1
        assert!(!opts.exceeds_depth("a/b.txt")); // depth 2
        assert!(opts.exceeds_depth("a/b/c.txt")); // depth 3
    }

    #[test]
    fn test_options_exceeds_depth_unlimited() {
        let opts = TreeCollectOptions::new();

        assert!(!opts.exceeds_depth("a/b/c/d/e/f/g.txt"));
    }

    #[test]
    fn test_options_clone() {
        let opts = TreeCollectOptions::new().prefix("test/");
        let cloned = opts.clone();

        assert_eq!(opts, cloned);
    }

    #[test]
    fn test_options_debug() {
        let opts = TreeCollectOptions::new();
        let debug = format!("{:?}", opts);

        assert!(debug.contains("TreeCollectOptions"));
    }

    // ========================================================================
    // TreeItem Tests
    // ========================================================================

    #[test]
    fn test_item_file() {
        let item = TreeItem::file("src/main.rs", Inode::ROOT, Position::ROOT);

        assert_eq!(item.path, "src/main.rs");
        assert_eq!(item.inode, Inode::ROOT);
        assert!(!item.is_directory);
        assert!(item.is_file());
        assert_eq!(item.depth, 2);
    }

    #[test]
    fn test_item_directory() {
        let item = TreeItem::directory("src/lib", Inode::ROOT);

        assert_eq!(item.path, "src/lib");
        assert!(item.is_directory);
        assert!(!item.is_file());
        assert_eq!(item.depth, 2);
    }

    #[test]
    fn test_item_with_metadata() {
        let item = TreeItem::file("script.sh", Inode::ROOT, Position::ROOT)
            .with_metadata(FileMetadata::executable());

        assert!(item.metadata.is_executable());
    }

    #[test]
    fn test_item_filename() {
        let item = TreeItem::file("src/lib/mod.rs", Inode::ROOT, Position::ROOT);
        assert_eq!(item.filename(), "mod.rs");

        let root_file = TreeItem::file("Cargo.toml", Inode::ROOT, Position::ROOT);
        assert_eq!(root_file.filename(), "Cargo.toml");
    }

    #[test]
    fn test_item_parent_path() {
        let item = TreeItem::file("src/lib/mod.rs", Inode::ROOT, Position::ROOT);
        assert_eq!(item.parent_path(), "src/lib");

        let root_file = TreeItem::file("Cargo.toml", Inode::ROOT, Position::ROOT);
        assert_eq!(root_file.parent_path(), "");
    }

    #[test]
    fn test_item_is_hidden() {
        let hidden = TreeItem::file(".gitignore", Inode::ROOT, Position::ROOT);
        assert!(hidden.is_hidden());

        let visible = TreeItem::file("README.md", Inode::ROOT, Position::ROOT);
        assert!(!visible.is_hidden());

        let hidden_dir = TreeItem::directory(".git", Inode::ROOT);
        assert!(hidden_dir.is_hidden());
    }

    #[test]
    fn test_item_clone() {
        let item = TreeItem::file("test.rs", Inode::ROOT, Position::ROOT);
        let cloned = item.clone();

        assert_eq!(item, cloned);
    }

    #[test]
    fn test_item_debug() {
        let item = TreeItem::file("test.rs", Inode::ROOT, Position::ROOT);
        let debug = format!("{:?}", item);

        assert!(debug.contains("TreeItem"));
        assert!(debug.contains("test.rs"));
    }

    // ========================================================================
    // TreeCollectResult Tests
    // ========================================================================

    #[test]
    fn test_result_new() {
        let result = TreeCollectResult::new();

        assert!(result.items.is_empty());
        assert_eq!(result.files_count, 0);
        assert_eq!(result.directories_count, 0);
        assert_eq!(result.skipped_count, 0);
        assert_eq!(result.max_depth_reached, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_result_add_file() {
        let mut result = TreeCollectResult::new();
        let item = TreeItem::file("test.rs", Inode::ROOT, Position::ROOT);

        result.add_file(item);

        assert_eq!(result.files_count, 1);
        assert_eq!(result.total_items(), 1);
    }

    #[test]
    fn test_result_add_directory() {
        let mut result = TreeCollectResult::new();
        let item = TreeItem::directory("src", Inode::ROOT);

        result.add_directory(item);

        assert_eq!(result.directories_count, 1);
        assert_eq!(result.total_items(), 1);
    }

    #[test]
    fn test_result_record_skipped() {
        let mut result = TreeCollectResult::new();

        result.record_skipped();
        result.record_skipped();

        assert_eq!(result.skipped_count, 2);
    }

    #[test]
    fn test_result_record_error() {
        let mut result = TreeCollectResult::new();

        result.record_error("test.rs".to_string(), "error message".to_string());

        assert!(result.has_errors());
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_result_has_errors() {
        let mut result = TreeCollectResult::new();
        assert!(!result.has_errors());

        result.record_error("test.rs".to_string(), "error".to_string());
        assert!(result.has_errors());
    }

    #[test]
    fn test_result_files_iterator() {
        let mut result = TreeCollectResult::new();
        result.add_file(TreeItem::file("a.rs", Inode::ROOT, Position::ROOT));
        result.add_directory(TreeItem::directory("src", Inode::ROOT));
        result.add_file(TreeItem::file("b.rs", Inode::ROOT, Position::ROOT));

        let files: Vec<_> = result.files().collect();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_result_directories_iterator() {
        let mut result = TreeCollectResult::new();
        result.add_file(TreeItem::file("a.rs", Inode::ROOT, Position::ROOT));
        result.add_directory(TreeItem::directory("src", Inode::ROOT));
        result.add_directory(TreeItem::directory("tests", Inode::ROOT));

        let dirs: Vec<_> = result.directories().collect();
        assert_eq!(dirs.len(), 2);
    }

    #[test]
    fn test_result_sort_by_path() {
        let mut result = TreeCollectResult::new();
        result.add_file(TreeItem::file("z.rs", Inode::ROOT, Position::ROOT));
        result.add_file(TreeItem::file("a.rs", Inode::ROOT, Position::ROOT));
        result.add_file(TreeItem::file("m.rs", Inode::ROOT, Position::ROOT));

        result.sort_by_path();

        assert_eq!(result.items[0].path, "a.rs");
        assert_eq!(result.items[1].path, "m.rs");
        assert_eq!(result.items[2].path, "z.rs");
    }

    #[test]
    fn test_result_sort_by_depth() {
        let mut result = TreeCollectResult::new();
        result.add_file(TreeItem::file("a/b/c.rs", Inode::ROOT, Position::ROOT));
        result.add_file(TreeItem::file("x.rs", Inode::ROOT, Position::ROOT));
        result.add_file(TreeItem::file("d/e.rs", Inode::ROOT, Position::ROOT));

        result.sort_by_depth();

        assert_eq!(result.items[0].depth, 1);
        assert_eq!(result.items[1].depth, 2);
        assert_eq!(result.items[2].depth, 3);
    }

    #[test]
    fn test_result_max_depth_tracked() {
        let mut result = TreeCollectResult::new();
        result.add_file(TreeItem::file("a.rs", Inode::ROOT, Position::ROOT));
        assert_eq!(result.max_depth_reached, 1);

        result.add_file(TreeItem::file("a/b/c.rs", Inode::ROOT, Position::ROOT));
        assert_eq!(result.max_depth_reached, 3);

        // Adding shallower item shouldn't decrease max depth
        result.add_file(TreeItem::file("x.rs", Inode::ROOT, Position::ROOT));
        assert_eq!(result.max_depth_reached, 3);
    }

    #[test]
    fn test_result_clone() {
        let mut result = TreeCollectResult::new();
        result.add_file(TreeItem::file("test.rs", Inode::ROOT, Position::ROOT));

        let cloned = result.clone();

        assert_eq!(result.files_count, cloned.files_count);
        assert_eq!(result.items.len(), cloned.items.len());
    }

    #[test]
    fn test_result_debug() {
        let result = TreeCollectResult::new();
        let debug = format!("{:?}", result);

        assert!(debug.contains("TreeCollectResult"));
    }

    // ========================================================================
    // build_tree_hierarchy Tests
    // ========================================================================

    #[test]
    fn test_build_hierarchy_empty() {
        let items: Vec<TreeItem> = vec![];
        let hierarchy = build_tree_hierarchy(&items);

        assert!(hierarchy.is_empty());
    }

    #[test]
    fn test_build_hierarchy_root_files() {
        let items = vec![
            TreeItem::file("a.rs", Inode::ROOT, Position::ROOT),
            TreeItem::file("b.rs", Inode::ROOT, Position::ROOT),
        ];

        let hierarchy = build_tree_hierarchy(&items);

        assert!(hierarchy.contains_key(""));
        assert_eq!(hierarchy.get("").unwrap().len(), 2);
    }

    #[test]
    fn test_build_hierarchy_nested() {
        let items = vec![
            TreeItem::file("src/main.rs", Inode::ROOT, Position::ROOT),
            TreeItem::file("src/lib.rs", Inode::ROOT, Position::ROOT),
            TreeItem::file("Cargo.toml", Inode::ROOT, Position::ROOT),
        ];

        let hierarchy = build_tree_hierarchy(&items);

        assert!(hierarchy.contains_key("")); // Cargo.toml
        assert!(hierarchy.contains_key("src")); // main.rs, lib.rs
        assert_eq!(hierarchy.get("").unwrap().len(), 1);
        assert_eq!(hierarchy.get("src").unwrap().len(), 2);
    }

    #[test]
    fn test_build_hierarchy_deep_nesting() {
        let items = vec![TreeItem::file("a/b/c/d.rs", Inode::ROOT, Position::ROOT)];

        let hierarchy = build_tree_hierarchy(&items);

        assert!(hierarchy.contains_key("a/b/c"));
        assert_eq!(hierarchy.get("a/b/c").unwrap().len(), 1);
    }
}

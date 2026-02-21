//! Repository management for Atomic VCS
//!
//! This module provides the main `Repository` abstraction that coordinates
//! all VCS operations including initialization, opening existing repositories,
//! recording changes, and working copy management.
//!
//! # Stacks vs Branches
//!
//! Atomic uses **Stacks** instead of branches. This is a fundamental conceptual
//! difference from Git:
//!
//! | Concept | Git Branches | Atomic Stacks |
//! |---------|--------------|---------------|
//! | Nature | Fork of history | View of the graph |
//! | Data | Duplicates commits | References same changes |
//! | Merge | Combines divergent histories | Applies missing changes |
//! | Identity | Pointer to a commit | Ordered sequence + Merkle state |
//!
//! Stacks are **views** of the graph - they represent which changes have been
//! applied and in what order. Multiple stacks can coexist, each showing a
//! different perspective on the same underlying data.
//!
//! # Change Storage
//!
//! Changes are stored in a content-addressed manner under `.atomic/changes/`.
//! The repository provides convenient methods for saving and loading changes
//! that integrate with the underlying [`ChangeStore`](crate::ChangeStore).
//!
//! ```rust,ignore
//! // Save a change
//! let hash = repo.save_change(&change)?;
//!
//! // Load it back
//! let loaded = repo.load_change(&hash)?;
//!
//! // Check if it exists
//! assert!(repo.has_change(&hash));
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use atomic_core::change::{Author, Change, ChangeHeader, GraphOp};
use atomic_core::output::repo::{
    output_repository, RepositoryOutputOptions, RepositoryOutputResult,
};
use atomic_core::output::FileSystem;
use atomic_core::output::WorkingCopy;
use atomic_core::pristine::{
    GraphTxnT, MutTxnT, OverlayTxn, Pristine, StackKind, StackTxnT, TreeTxnT,
};
use atomic_core::record::workflow::retrieve::{RetrieveContentOptions, RetrieveResult};
use atomic_core::types::{Base32, Hash, Inode, Merkle, NodeId, Position};

use crate::apply::{
    apply_change_to_graph, filter_missing_in_stack, get_missing_changes, get_stack_changes,
    ApplyOptions, ApplyOutcome, ApplyStats, CrossStackApplyOptions, CrossStackApplyOutcome,
};
use crate::archive::{
    Archive, ArchiveEntry, ArchiveManifest, ArchiveOptions, ArchiveOutcome, DirectoryArchive,
};
use crate::changestore::{ChangeStore, ChangeStoreError, DEFAULT_CACHE_CAPACITY};
use crate::history::{HistoryOptions, HistorySummary};
use crate::ignore::IgnoreRules;
use crate::record::{
    build_header, filter_files, RecordError, RecordOptions, RecordOutcome, RecordStats,
};
use crate::remote::{RemoteConfig, RemoteEntry};
use crate::status::{
    collect_working_copy_files_with_rules, hash_file_contents, FileStatus, FileStatusEntry,
    RepositoryStatus, StatusOptions,
};
use crate::tags::{save_tag, save_tag_force, validate_tag_name, Tag, TagFilter, TagOptions};
use crate::tracking::{
    add_to_tree, collect_files_for_tracking_with_rules, get_inode, is_tracked, list_tracked,
    move_tracked, normalize_path, normalize_path_with_root, remove_from_tree,
    should_ignore_with_rules, tracked_under_prefix, TrackedFile, TrackingError, TrackingOptions,
    TrackingStats,
};
use crate::unrecord::{UnrecordOptions, UnrecordOutcome};
use crate::RepositoryError;

/// The name of the Atomic directory
pub const DOT_DIR: &str = ".atomic";

/// The default stack name
pub const DEFAULT_STACK: &str = "dev";

/// Information about a stack.
///
/// This struct provides metadata about a stack including its current
/// Merkle state and the number of changes applied to it.
#[derive(Debug, Clone)]
pub struct StackInfo {
    /// The name of the stack
    pub name: String,
    /// The current Merkle state (hash of all applied changes)
    pub state: Merkle,
    /// The number of changes applied to this stack
    pub change_count: u64,
    /// Stack kind (Local or Shared)
    pub kind: StackKind,
    /// Parent stack name, if any
    pub parent_name: Option<String>,
}

impl StackInfo {
    /// Get the Merkle state as a base32-encoded string.
    pub fn state_base32(&self) -> String {
        self.state.to_base32()
    }

    /// Get a short version of the Merkle state (first 12 characters).
    pub fn state_short(&self) -> String {
        let full = self.state.to_base32();
        if full.len() > 12 {
            full[..12].to_string()
        } else {
            full
        }
    }

    /// Check if the stack is empty (has no changes).
    pub fn is_empty(&self) -> bool {
        self.change_count == 0
    }

    /// Get a human-readable label for the stack kind.
    pub fn kind_label(&self) -> &str {
        match self.kind {
            StackKind::Shared => "shared",
            StackKind::Local => "local",
        }
    }

    /// Get the parent name for display, or "—" if root.
    pub fn parent_display(&self) -> &str {
        self.parent_name.as_deref().unwrap_or("—")
    }
}

/// An Atomic repository.
///
/// The Repository struct is the main entry point for all VCS operations.
/// It manages the repository's pristine (database), changes directory,
/// working copy, and configuration.
///
/// # Components
///
/// - **Pristine**: The graph database storing all version control data
/// - **ChangeStore**: Content-addressed storage for change files
/// - **Working Copy**: The actual files in the repository (future)
///
/// # Thread Safety
///
/// The `Repository` struct is `!Sync` due to the internal caching in
/// [`ChangeStore`]. For concurrent access, use separate `Repository`
/// instances or wrap in appropriate synchronization primitives.
pub struct Repository {
    /// Root path of the repository (contains .atomic/)
    root: PathBuf,
    /// Path to the .atomic directory
    dot_dir: PathBuf,
    /// Current stack name
    current_stack: String,
    /// The pristine database handle
    pristine: Pristine,
    /// The change store for persisting changes
    change_store: ChangeStore,
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repository")
            .field("root", &self.root)
            .field("dot_dir", &self.dot_dir)
            .field("current_stack", &self.current_stack)
            .field("pristine", &"<Pristine>")
            .field("change_store", &self.change_store)
            .finish()
    }
}

impl Repository {
    /// Initialize a new repository at the given path.
    ///
    /// This creates the `.atomic` directory structure and initializes
    /// the database with an empty graph.
    ///
    /// # Arguments
    ///
    /// * `path` - The directory to initialize as a repository
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A repository already exists at the path
    /// - The directory cannot be created
    /// - The database cannot be initialized
    pub fn init<P: AsRef<Path>>(path: P) -> Result<Self, RepositoryError> {
        let root = path.as_ref().to_path_buf();
        let dot_dir = root.join(DOT_DIR);

        // Check if repository already exists
        if dot_dir.exists() {
            return Err(RepositoryError::AlreadyExists {
                path: root.display().to_string(),
            });
        }

        // Create directory structure
        std::fs::create_dir_all(&dot_dir)?;
        std::fs::create_dir_all(dot_dir.join("changes"))?;

        // Create initial config
        let config_path = dot_dir.join("config.toml");
        let initial_config = format!(
            r#"# Atomic repository configuration

[stack]
default = "{}"
"#,
            DEFAULT_STACK
        );
        std::fs::write(&config_path, initial_config)?;

        // Create working copy ID file
        let wc_id_path = dot_dir.join("working_copy_id");
        std::fs::write(&wc_id_path, "")?;

        // Initialize the pristine database (redb creates the file)
        let pristine = Pristine::open(dot_dir.join("pristine.redb"))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Create the default stack
        {
            let mut txn = pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.open_or_create_stack(DEFAULT_STACK)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Initialize the change store
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Self {
            root,
            dot_dir,
            current_stack: DEFAULT_STACK.to_string(),
            pristine,
            change_store,
        })
    }

    /// Open an existing repository.
    ///
    /// This searches for a `.atomic` directory starting from the given path
    /// and walking up to parent directories.
    ///
    /// # Arguments
    ///
    /// * `path` - A path inside the repository (or the repository root)
    ///
    /// # Errors
    ///
    /// Returns an error if no repository is found.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, RepositoryError> {
        let root = Self::find_root(path.as_ref())?;
        let dot_dir = root.join(DOT_DIR);

        // Open the pristine database
        let pristine = Pristine::open(dot_dir.join("pristine.redb"))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Read current stack from config or use default
        let current_stack =
            Self::read_current_stack(&dot_dir).unwrap_or_else(|_| DEFAULT_STACK.to_string());

        // Open the change store
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Self {
            root,
            dot_dir,
            current_stack,
            pristine,
            change_store,
        })
    }

    /// Open an existing repository in read-only mode.
    ///
    /// This method opens the repository without acquiring a write lock on the
    /// database, allowing concurrent read access from multiple processes. It's
    /// suitable for read-only operations like `status`, `diff`, `log`, and `change`.
    ///
    /// Use this method when you only need to query the repository state and don't
    /// need to make any modifications. This is especially useful for:
    /// - CLI commands that only display information
    /// - Integration tools that poll repository status
    /// - Concurrent access scenarios where write operations are happening elsewhere
    ///
    /// # Arguments
    ///
    /// * `path` - Any path within the repository
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No repository is found at or above the given path
    /// - The database file doesn't exist or is corrupted
    /// - Read access cannot be obtained
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Open for read-only status check
    /// let repo = Repository::open_readonly(".")?;
    /// let status = repo.status(StatusOptions::default())?;
    /// ```
    pub fn open_readonly<P: AsRef<Path>>(path: P) -> Result<Self, RepositoryError> {
        let root = Self::find_root(path.as_ref())?;
        let dot_dir = root.join(DOT_DIR);

        // Open the pristine database in read-only mode
        let pristine = Pristine::open_readonly(dot_dir.join("pristine.redb"))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Read current stack from config or use default
        let current_stack =
            Self::read_current_stack(&dot_dir).unwrap_or_else(|_| DEFAULT_STACK.to_string());

        // Open the change store
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Self {
            root,
            dot_dir,
            current_stack,
            pristine,
            change_store,
        })
    }

    /// Find the repository root by searching for .atomic directory.
    ///
    /// Starts at the given path and walks up to parent directories until
    /// a `.atomic` directory is found that contains `pristine.redb` (indicating
    /// it's a repository, not just a config directory like `~/.atomic/`).
    ///
    /// The search stops at the user's home directory to prevent accidentally
    /// treating the entire home directory as a repository.
    pub fn find_root(start: &Path) -> Result<PathBuf, RepositoryError> {
        let mut current = if start.is_file() {
            start.parent().map(Path::to_path_buf)
        } else {
            Some(start.to_path_buf())
        };

        // Get the home directory to use as a boundary
        let home_dir = dirs::home_dir();

        while let Some(dir) = current {
            // Stop searching if we've reached the home directory
            // We don't want ~/.atomic/ (config dir) to be treated as a repository
            if let Some(ref home) = home_dir {
                if dir == *home {
                    break;
                }
            }

            let dot_dir = dir.join(DOT_DIR);
            // Check that .atomic/ exists AND contains pristine.redb
            // This distinguishes a repository from a config directory
            if dot_dir.is_dir() && dot_dir.join("pristine.redb").exists() {
                return Ok(dir);
            }
            current = dir.parent().map(Path::to_path_buf);
        }

        Err(RepositoryError::NotFound {
            path: start.display().to_string(),
        })
    }

    /// Check if a path is inside an Atomic repository.
    pub fn is_repository<P: AsRef<Path>>(path: P) -> bool {
        Self::find_root(path.as_ref()).is_ok()
    }

    /// Get the repository root path.
    #[inline]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get the .atomic directory path.
    #[inline]
    pub fn dot_dir(&self) -> &Path {
        &self.dot_dir
    }

    /// Get the pristine (database) file path.
    #[inline]
    pub fn pristine_path(&self) -> PathBuf {
        self.dot_dir.join("pristine.redb")
    }

    /// Get the changes directory path.
    #[inline]
    pub fn changes_dir(&self) -> PathBuf {
        self.dot_dir.join("changes")
    }

    /// Get the current stack name.
    #[inline]
    pub fn current_stack(&self) -> &str {
        &self.current_stack
    }

    /// Get the config file path.
    #[inline]
    pub fn config_path(&self) -> PathBuf {
        self.dot_dir.join("config.toml")
    }

    /// Set the current stack (internal, does not update working copy).
    ///
    /// This updates both the in-memory state and persists the change to disk,
    /// but does NOT update the working copy. Use `switch_stack` instead for
    /// the full switch operation that also updates the working copy.
    ///
    /// # Arguments
    ///
    /// * `stack` - The name of the stack to switch to
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The stack does not exist in the pristine database
    /// - The stack file cannot be written
    pub fn set_current_stack(&mut self, stack: &str) -> Result<(), RepositoryError> {
        // Verify the stack exists in the pristine database
        {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            if txn
                .get_stack(stack)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .is_none()
            {
                return Err(RepositoryError::StackNotFound {
                    name: stack.to_string(),
                });
            }
        }

        self.current_stack = stack.to_string();
        self.write_current_stack(stack)?;
        Ok(())
    }

    /// Switch to a different stack and update the working copy.
    ///
    /// This is the primary method for switching stacks. It:
    /// 1. Validates the stack exists
    /// 2. Updates the current stack pointer
    /// 3. Outputs the working copy to match the new stack's state
    ///
    /// This behavior matches Pijul's channel switching, where switching
    /// channels also updates the working copy to reflect that channel's state.
    ///
    /// # Arguments
    ///
    /// * `stack` - The name of the stack to switch to
    ///
    /// # Returns
    ///
    /// Statistics about the output operation (files written, etc.)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The stack does not exist
    /// - The working copy cannot be updated
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut repo = Repository::open(".")?;
    ///
    /// // Switch to feature stack and update working copy
    /// let result = repo.switch_stack("feature")?;
    /// println!("Updated {} files", result.files_written);
    /// ```
    pub fn switch_stack(&mut self, stack: &str) -> Result<RepositoryOutputResult, RepositoryError> {
        let old_stack_name = self.current_stack.clone();

        // Compute files visible on the OLD stack via its overlay.
        let old_files = self.visible_file_paths(&old_stack_name)?;

        // Set the current stack (validates it exists)
        self.set_current_stack(stack)?;

        // Compute files visible on the NEW stack via its overlay.
        let new_files = self.visible_file_paths(stack)?;

        // Delete from disk: files visible on the old stack but NOT visible
        // on the new stack.  Untracked files (not recorded on any stack)
        // are left alone because they never appear in either set.
        let working_copy = FileSystem::from_root(&self.root);
        let mut files_removed: usize = 0;
        for path in old_files.difference(&new_files) {
            let abs_path = self.root.join(path);
            if abs_path.exists() && !abs_path.is_dir() {
                if working_copy.remove_path(path, false).is_ok() {
                    files_removed += 1;
                }
            }
        }

        // Clean up empty parent directories left behind after file removal.
        if files_removed > 0 {
            let mut dirs_to_try: HashSet<PathBuf> = HashSet::new();
            for path in old_files.difference(&new_files) {
                let p = PathBuf::from(path);
                let mut ancestor = p.parent();
                while let Some(dir) = ancestor {
                    if dir == Path::new("") || dir == Path::new(".") {
                        break;
                    }
                    dirs_to_try.insert(dir.to_path_buf());
                    ancestor = dir.parent();
                }
            }
            let mut sorted_dirs: Vec<PathBuf> = dirs_to_try.into_iter().collect();
            sorted_dirs.sort_by(|a, b| b.components().count().cmp(&a.components().count()));
            for dir in sorted_dirs {
                let abs_dir = self.root.join(&dir);
                if abs_dir.is_dir() {
                    let _ = std::fs::remove_dir(&abs_dir);
                }
            }
        }

        // Output the working copy using the overlay model.
        self.output_working_copy()
    }

    /// Compute the set of file paths whose creating change is on a stack.
    ///
    /// Visibility is determined by the stack's **change log**, not the
    /// overlay chain.  The overlay provides graph-level read access for
    /// record / diff operations, but file *materialization* (what shows
    /// up on disk after `switch_stack`) is governed by which changes have
    /// been explicitly applied to the stack.
    ///
    /// A file is visible on a stack when:
    /// 1. It appears in the global TREE table (has been `add`ed).
    /// 2. Its inode has a graph position in the INODES table (has been
    ///    `record`ed).
    /// 3. The change that introduced that position is present in the
    ///    stack's change log (via `iter_changes`).
    ///
    /// Files that have been `add`ed but not yet `record`ed (no INODES
    /// entry) are NOT returned — they persist across switches as
    /// working-copy state.
    pub fn visible_file_paths(&self, stack_name: &str) -> Result<HashSet<String>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = match txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(s) => s,
            None => return Ok(HashSet::new()),
        };

        // Collect every change NodeId on this stack's log.
        let mut stack_change_ids: HashSet<NodeId> = HashSet::new();
        let iter = txn
            .iter_changes(&stack, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        for result in iter {
            let (_seq, node_id, _merkle) =
                result.map_err(|e| RepositoryError::Database(e.to_string()))?;
            stack_change_ids.insert(node_id);
        }

        // Walk TREE and keep paths whose introducing change is in the log.
        let mut paths: HashSet<String> = HashSet::new();
        let tree_iter = txn
            .iter_tree()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in tree_iter {
            let (path, inode) = result.map_err(|e| RepositoryError::Database(e.to_string()))?;
            if let Some(position) = txn
                .inode_position(inode)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                if stack_change_ids.contains(&position.change) {
                    paths.insert(path);
                }
            }
        }

        Ok(paths)
    }

    /// Output the working copy to match the current stack's state.
    ///
    /// This synchronizes the working copy files with the repository graph
    /// state for the current stack. Files are created, updated, or deleted
    /// to match what's recorded in the stack.
    ///
    /// # Returns
    ///
    /// Statistics about the output operation including:
    /// - Number of files written
    /// - Number of directories created
    /// - Any conflicts detected
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The database cannot be read
    /// - Files cannot be written to the working copy
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    ///
    /// // Reset working copy to current stack's state
    /// let result = repo.output_working_copy()?;
    /// println!("Output {} files", result.files_written);
    ///
    /// if result.has_conflicts() {
    ///     println!("Warning: {} conflicts detected", result.conflict_count());
    /// }
    /// ```
    pub fn output_working_copy(&self) -> Result<RepositoryOutputResult, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the current stack
        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        // Build the overlay for this stack's perspective.
        //
        // For Local stacks the overlay reads STACK_GRAPH[this] ∪ ... ∪ GRAPH.
        // For Shared stacks the overlay is empty and falls through to GRAPH.
        // This is the architectural foundation of per-stack file isolation:
        // edges written by a Local stack live in its STACK_GRAPH and are
        // invisible to other stacks.
        let overlay = OverlayTxn::from_stack(&txn, &stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Collect all change NodeIds in the current stack for the
        // change_filter.  Even though the overlay isolates edges, we
        // still need the filter so that `output_repository` can skip
        // TREE entries whose creating change is not on this stack.
        let mut change_filter: HashSet<NodeId> = HashSet::new();
        let iter = txn
            .iter_changes(&stack, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in iter {
            let (_seq, node_id, _merkle) =
                result.map_err(|e| RepositoryError::Database(e.to_string()))?;
            change_filter.insert(node_id);
        }

        let working_copy = FileSystem::from_root(&self.root);
        let options = RepositoryOutputOptions::new().with_change_filter(change_filter);

        // Use the overlay transaction for graph reads so that Local
        // stacks see their own STACK_GRAPH edges while Shared stacks
        // read from the global GRAPH as before.
        let result = output_repository(&overlay, &self.change_store, &working_copy, options)
            .map_err(|e| RepositoryError::Output(format!("{}", e)))?;

        Ok(result)
    }

    /// Output the working copy for a specific prefix only.
    ///
    /// This is useful for partial updates when you only want to sync
    /// a subset of files.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path prefix to output (e.g., "src/")
    ///
    /// # Returns
    ///
    /// Statistics about the output operation.
    pub fn output_working_copy_prefix(
        &self,
        prefix: &str,
    ) -> Result<RepositoryOutputResult, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let working_copy = FileSystem::from_root(&self.root);
        let options = RepositoryOutputOptions::new().prefix(prefix);

        let result = output_repository(&txn, &self.change_store, &working_copy, options)
            .map_err(|e| RepositoryError::Output(format!("{}", e)))?;

        Ok(result)
    }

    /// Create a new stack.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the stack to create
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The stack already exists
    /// - The database operation fails
    pub fn create_stack(&mut self, name: &str) -> Result<(), RepositoryError> {
        // Create a **Local** workspace parented on the nearest Shared
        // ancestor of the current stack.  The change log starts EMPTY —
        // no changes are inherited automatically.
        //
        // The parent link gives the stack read-access to the shared
        // graph content (via the overlay chain) so that `record` can
        // compute diffs against the existing state.  But no files are
        // *materialised* on disk until changes are explicitly `apply`-ed
        // to this stack (which copies them into the stack's change log
        // and writes edges to its STACK_GRAPH).
        //
        // This means:
        //   `stack new feature`            → empty workspace, no files
        //   `apply from-stack dev feature` → inherits dev's files
        //
        // Using the nearest Shared ancestor (instead of the current
        // stack directly) prevents sibling Local stacks from seeing
        // each other's STACK_GRAPH edges through the overlay chain.
        let parent_name = self.nearest_shared_ancestor(&self.current_stack.clone())?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if txn
            .get_stack(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some()
        {
            return Err(RepositoryError::StackAlreadyExists {
                name: name.to_string(),
            });
        }

        let parent_stack = txn
            .get_stack(&parent_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: parent_name.clone(),
            })?;

        txn.create_stack(name, StackKind::Local, Some(parent_stack.id))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Walk the parent chain from `stack_name` and return the name of the
    /// first Shared stack encountered.  If `stack_name` is itself Shared,
    /// it is returned immediately.  This is used to determine the correct
    /// parent for newly created Local stacks.
    pub fn nearest_shared_ancestor(&self, stack_name: &str) -> Result<String, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        // Already Shared → use it directly.
        if stack.kind.is_shared() {
            return Ok(stack_name.to_string());
        }

        // Walk up the parent chain looking for a Shared ancestor.
        let mut cursor = stack.parent;
        while let Some(parent_id) = cursor {
            if let Some(parent) = txn
                .get_stack_by_id(parent_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                if parent.kind.is_shared() {
                    return Ok(parent.name.clone());
                }
                cursor = parent.parent;
            } else {
                break;
            }
        }

        // Fallback: if no Shared ancestor found (shouldn't happen in
        // normal use — dev is always Shared), use the current stack.
        Ok(stack_name.to_string())
    }

    /// Create a new stack that inherits changes from another stack.
    ///
    /// This creates a new stack and copies all changes from the source stack
    /// to the new stack. The new stack will have the same content state as
    /// the source stack at the time of creation.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the new stack to create
    /// * `from_stack` - The name of the stack to inherit changes from
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The new stack already exists
    /// - The source stack does not exist
    /// - The database operation fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Create a feature stack that starts with dev's changes
    /// repo.create_stack_from("feature", "dev")?;
    /// ```
    pub fn create_stack_from(
        &mut self,
        name: &str,
        from_stack: &str,
    ) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if the new stack already exists
        if txn
            .get_stack(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some()
        {
            return Err(RepositoryError::StackAlreadyExists {
                name: name.to_string(),
            });
        }

        // Get the source stack
        let source_stack = txn
            .get_stack(from_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: from_stack.to_string(),
            })?;

        let source_id = source_stack.id;

        // Collect all changes from the source stack
        let changes: Vec<(NodeId, Hash)> = {
            let iter = txn
                .iter_changes(&source_stack, 0)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            let mut result = Vec::new();
            for item in iter {
                let (_seq, node_id, _merkle) =
                    item.map_err(|e| RepositoryError::Database(e.to_string()))?;
                let hash = txn
                    .get_external(node_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| {
                        RepositoryError::Database(format!(
                            "Change {} has no external hash",
                            node_id.0
                        ))
                    })?;
                result.push((node_id, hash));
            }
            result
        };

        // Create the new stack as a **Local** workspace parented on the
        // source stack.  Local stacks write edges to STACK_GRAPH which
        // isolates them from other stacks.  The parent link means the
        // overlay chain (STACK_GRAPH[self] ∪ ... ∪ GRAPH) includes the
        // source's content.
        let mut new_stack = txn
            .create_stack(name, StackKind::Local, Some(source_id))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Copy all changes from the source to the new stack's log.
        // This does NOT re-apply hunks — the edges already exist in
        // GRAPH (for Shared sources) or in the source's STACK_GRAPH.
        // The new stack sees them via the overlay chain.
        for (node_id, hash) in changes {
            txn.put_change(&mut new_stack, node_id, &hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Update the stack state
        txn.update_stack(&new_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// List all stacks in the repository.
    ///
    /// # Returns
    ///
    /// A vector of stack names.
    pub fn list_stacks(&self) -> Result<Vec<String>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.list_stacks()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Check if a stack exists.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the stack to check
    pub fn stack_exists(&self, name: &str) -> Result<bool, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(txn
            .get_stack(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some())
    }

    /// Delete a stack from the repository.
    ///
    /// This removes the stack and all its associated metadata, but does not
    /// delete the changes themselves. Changes remain in the graph and may be
    /// referenced by other stacks.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the stack to delete
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The stack does not exist
    /// - The stack is the current stack (cannot delete current stack)
    /// - The database operation fails
    pub fn delete_stack(&mut self, name: &str) -> Result<(), RepositoryError> {
        // Cannot delete the current stack
        if name == self.current_stack {
            return Err(RepositoryError::CannotDeleteCurrentStack {
                name: name.to_string(),
            });
        }

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the stack to delete
        let stack = txn
            .get_stack(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: name.to_string(),
            })?;

        // Delete the stack.
        //
        // `del_stack` enforces:
        // - Shared stacks cannot be deleted (returns CannotDeleteSharedStack)
        // - Stacks with children cannot be deleted (returns StackHasChildren)
        // - Local workspaces cascade-delete all STACK_GRAPH edges (zero orphans)
        txn.del_stack(&stack).map_err(|e| match &e {
            atomic_core::pristine::PristineError::CannotDeleteSharedStack { name } => {
                RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot delete shared stack '{}': shared stacks are permanent. \
                         Use 'stack new' to create an local workspace instead.",
                        name
                    ),
                }
            }
            atomic_core::pristine::PristineError::StackHasChildren { name, children } => {
                RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot delete stack '{}': has child stacks ({}). \
                         Delete or reparent children first.",
                        name,
                        children.join(", ")
                    ),
                }
            }
            _ => RepositoryError::Database(e.to_string()),
        })?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Get information about a stack.
    ///
    /// Returns the stack's metadata including its Merkle state and change count.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the stack to query
    ///
    /// # Returns
    ///
    /// A tuple of (merkle_state_hex, change_count) or an error if the stack
    /// doesn't exist.
    pub fn get_stack_info(&self, name: &str) -> Result<StackInfo, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = txn
            .get_stack(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: name.to_string(),
            })?;

        // Resolve parent name if the stack has a parent
        let parent_name = if let Some(parent_id) = stack.parent {
            txn.get_stack_by_id(parent_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .map(|p| p.name)
        } else {
            None
        };

        Ok(StackInfo {
            name: stack.name.clone(),
            state: stack.state,
            change_count: stack.change_count,
            kind: stack.kind,
            parent_name,
        })
    }

    /// Get a reference to the pristine database.
    #[inline]
    pub fn pristine(&self) -> &Pristine {
        &self.pristine
    }

    /// Read the current stack from the config file.
    fn read_current_stack(dot_dir: &Path) -> Result<String, RepositoryError> {
        let current_path = dot_dir.join("current_stack");
        if current_path.exists() {
            let content = std::fs::read_to_string(&current_path)?;
            Ok(content.trim().to_string())
        } else {
            Ok(DEFAULT_STACK.to_string())
        }
    }

    /// Write the current stack to disk.
    fn write_current_stack(&self, stack: &str) -> Result<(), RepositoryError> {
        let current_path = self.dot_dir.join("current_stack");
        std::fs::write(&current_path, stack)?;
        Ok(())
    }

    /// Get the path where a change file should be stored.
    ///
    /// Changes are stored in a two-level directory structure based on their hash:
    /// `.atomic/changes/AB/CDEF...` where AB is the first two characters of the
    /// base32-encoded hash.
    pub fn change_path(&self, hash_base32: &str) -> PathBuf {
        let prefix = &hash_base32[..2.min(hash_base32.len())];
        self.changes_dir().join(prefix).join(hash_base32)
    }

    /// Convert an absolute path to a repository-relative path.
    pub fn to_relative<P: AsRef<Path>>(&self, path: P) -> Option<PathBuf> {
        path.as_ref()
            .strip_prefix(&self.root)
            .ok()
            .map(Path::to_path_buf)
    }

    /// Convert a repository-relative path to an absolute path.
    pub fn to_absolute<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        self.root.join(path)
    }

    /// Check if a path is inside the .atomic directory.
    pub fn is_internal_path<P: AsRef<Path>>(&self, path: P) -> bool {
        path.as_ref().starts_with(&self.dot_dir)
    }
}

mod apply;
mod archive;
mod changes;
mod content;
mod history;
mod record;
mod remotes;
mod status;
mod tags;
mod tracking;

#[cfg(test)]
mod tests;

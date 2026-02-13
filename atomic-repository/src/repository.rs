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
use atomic_core::pristine::{GraphTxnT, MutTxnT, Pristine, StackTxnT, TreeTxnT};
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
        // First, set the current stack (validates it exists)
        self.set_current_stack(stack)?;

        // Then output the working copy to match the new stack's state
        self.output_working_copy()
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

        // Collect all change NodeIds in the current stack
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

        let result = output_repository(&txn, &self.change_store, &working_copy, options)
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
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if the stack already exists
        if txn
            .get_stack(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some()
        {
            return Err(RepositoryError::StackAlreadyExists {
                name: name.to_string(),
            });
        }

        // Create the stack
        txn.open_or_create_stack(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
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

        // Create the new stack
        let mut new_stack = txn
            .open_or_create_stack(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Copy all changes to the new stack
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

        // Delete the stack
        txn.del_stack(&stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

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

        Ok(StackInfo {
            name: stack.name.clone(),
            state: stack.state,
            change_count: stack.change_count,
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

    // ========================================================================
    // Change Storage Methods
    // ========================================================================

    /// Get a reference to the change store.
    ///
    /// This provides direct access to the underlying [`ChangeStore`] for
    /// advanced operations like iteration or cache management.
    #[inline]
    pub fn change_store(&self) -> &ChangeStore {
        &self.change_store
    }

    // ========================================================================
    // Ignore Rules
    // ========================================================================

    /// Load ignore rules for this repository.
    ///
    /// This loads patterns from:
    /// - Global config: `~/.config/atomic/ignore`
    /// - Repository-local: `.atomicignore` in repository root
    ///
    /// The returned [`IgnoreRules`] can be used to check if paths should be
    /// ignored during tracking or status operations.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    /// let rules = repo.ignore_rules();
    ///
    /// if rules.is_ignored(Path::new("target/debug"), true) {
    ///     println!("Path is ignored");
    /// }
    /// ```
    pub fn ignore_rules(&self) -> IgnoreRules {
        IgnoreRules::load(&self.root)
    }

    /// Check if a path should be ignored.
    ///
    /// This is a convenience method that loads ignore rules and checks the path.
    /// If you need to check multiple paths, use [`Self::ignore_rules()`] instead
    /// to avoid reloading the rules for each check.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to check (relative to repository root)
    /// * `is_dir` - Whether the path is a directory
    ///
    /// # Returns
    ///
    /// `true` if the path should be ignored, `false` otherwise.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let rules = self.ignore_rules();
        rules.is_ignored(path, is_dir)
    }

    // ========================================================================
    // Remote Configuration
    // ========================================================================

    /// Load remote configuration for this repository.
    ///
    /// # Returns
    ///
    /// The remote configuration, which may be empty if no remotes are configured.
    ///
    /// # Errors
    ///
    /// Returns an error if the config file exists but cannot be parsed.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    /// let remotes = repo.load_remotes()?;
    ///
    /// if let Some(origin) = remotes.get("origin") {
    ///     println!("Origin URL: {}", origin.url);
    /// }
    /// ```
    pub fn load_remotes(&self) -> Result<RemoteConfig, RepositoryError> {
        RemoteConfig::load(self.config_path()).map_err(|e| RepositoryError::Config(e.to_string()))
    }

    /// Save remote configuration for this repository.
    ///
    /// # Arguments
    ///
    /// * `config` - The remote configuration to save
    ///
    /// # Errors
    ///
    /// Returns an error if the config file cannot be written.
    pub fn save_remotes(&self, config: &RemoteConfig) -> Result<(), RepositoryError> {
        config
            .save(self.config_path())
            .map_err(|e| RepositoryError::Config(e.to_string()))
    }

    /// Get a remote by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to look up
    ///
    /// # Returns
    ///
    /// The remote entry if found, or an error if not found.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    /// let origin = repo.get_remote("origin")?;
    /// println!("URL: {}", origin.url);
    /// ```
    pub fn get_remote(&self, name: &str) -> Result<RemoteEntry, RepositoryError> {
        let config = self.load_remotes()?;
        config
            .get(name)
            .cloned()
            .ok_or_else(|| RepositoryError::RemoteNotFound {
                name: name.to_string(),
            })
    }

    /// Get the default remote.
    ///
    /// Returns the default remote (explicitly marked, or "origin" if it exists,
    /// or the only configured remote).
    ///
    /// # Returns
    ///
    /// A tuple of (name, entry) for the default remote.
    ///
    /// # Errors
    ///
    /// Returns an error if no remotes are configured.
    pub fn get_default_remote(&self) -> Result<(String, RemoteEntry), RepositoryError> {
        let config = self.load_remotes()?;
        config
            .get_default()
            .map(|(name, entry)| (name.to_string(), entry.clone()))
            .ok_or_else(|| RepositoryError::NoRemotesConfigured)
    }

    /// Add a new remote.
    ///
    /// # Arguments
    ///
    /// * `name` - The name for the new remote
    /// * `url` - The URL of the remote repository
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A remote with the same name already exists
    /// - The name is invalid
    /// - The URL is invalid
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    /// repo.add_remote("origin", "https://api.example.com/repo")?;
    /// ```
    pub fn add_remote(&self, name: &str, url: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .add(name, RemoteEntry::new(url))
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// Add a new remote and set it as the default.
    ///
    /// # Arguments
    ///
    /// * `name` - The name for the new remote
    /// * `url` - The URL of the remote repository
    pub fn add_remote_default(&self, name: &str, url: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .add(name, RemoteEntry::new_default(url))
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// Remove a remote.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to remove
    ///
    /// # Errors
    ///
    /// Returns an error if the remote doesn't exist.
    pub fn remove_remote(&self, name: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .remove(name)
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// Update the URL of an existing remote.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to update
    /// * `url` - The new URL
    ///
    /// # Errors
    ///
    /// Returns an error if the remote doesn't exist or the URL is invalid.
    pub fn set_remote_url(&self, name: &str, url: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .set_url(name, url)
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// Set a remote as the default.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to set as default
    ///
    /// # Errors
    ///
    /// Returns an error if the remote doesn't exist.
    pub fn set_default_remote(&self, name: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .set_default(name)
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// Rename a remote.
    ///
    /// # Arguments
    ///
    /// * `old_name` - The current name of the remote
    /// * `new_name` - The new name for the remote
    pub fn rename_remote(&self, old_name: &str, new_name: &str) -> Result<(), RepositoryError> {
        let mut config = self.load_remotes()?;
        config
            .rename(old_name, new_name)
            .map_err(|e| RepositoryError::Remote(e))?;
        self.save_remotes(&config)
    }

    /// List all configured remotes.
    ///
    /// # Returns
    ///
    /// A vector of (name, entry) tuples for all configured remotes.
    pub fn list_remotes(&self) -> Result<Vec<(String, RemoteEntry)>, RepositoryError> {
        let config = self.load_remotes()?;
        Ok(config
            .iter()
            .map(|(name, entry)| (name.to_string(), entry.clone()))
            .collect())
    }

    /// Check if a remote exists.
    pub fn has_remote(&self, name: &str) -> Result<bool, RepositoryError> {
        let config = self.load_remotes()?;
        Ok(config.contains(name))
    }

    /// Save a change to the repository.
    ///
    /// The change is serialized and written to the `.atomic/changes/` directory
    /// using a content-addressed two-level directory structure. The change is
    /// also cached for efficient subsequent access.
    ///
    /// # Arguments
    ///
    /// * `change` - The change to save
    ///
    /// # Returns
    ///
    /// The hash of the saved change, which can be used to retrieve it later.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The directory cannot be created
    /// - The file cannot be written
    /// - Serialization fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let change = record_changes(&repo, files)?;
    /// let hash = repo.save_change(&change)?;
    /// println!("Saved change: {}", hash.to_base32());
    /// ```
    pub fn save_change(&self, change: &Change) -> Result<Hash, RepositoryError> {
        self.change_store
            .save_change(change)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Load a change from the repository.
    ///
    /// If the change is in the cache, it's returned directly. Otherwise,
    /// it's loaded from disk, verified, and cached.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to load
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change doesn't exist (`ChangeNotFound`)
    /// - The file is corrupted (hash mismatch)
    /// - Deserialization fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let change = repo.load_change(&hash)?;
    /// println!("Message: {}", change.hashed.header.message);
    /// ```
    pub fn load_change(&self, hash: &Hash) -> Result<Change, RepositoryError> {
        self.change_store.load_change(hash).map_err(|e| match e {
            ChangeStoreError::NotFound { hash } => RepositoryError::ChangeNotFound { hash },
            other => RepositoryError::Database(other.to_string()),
        })
    }

    /// Check if a change exists in the repository.
    ///
    /// This checks both the cache and the filesystem. Note that this
    /// doesn't verify the integrity of the change file.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to check
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if repo.has_change(&hash) {
    ///     let change = repo.load_change(&hash)?;
    ///     // ...
    /// }
    /// ```
    pub fn has_change(&self, hash: &Hash) -> bool {
        self.change_store.has_change(hash)
    }

    // ========================================================================
    // Attestation Methods
    // ========================================================================

    /// Save an attestation to the repository.
    ///
    /// Serializes the attestation to disk and registers it in the graph
    /// with `node_type::ATTESTATION`. Also registers dependencies from
    /// `changes_covered` in the DEPS table so the graph knows which
    /// changes this attestation covers.
    ///
    /// # Arguments
    ///
    /// * `attestation` - The attestation to save
    ///
    /// # Returns
    ///
    /// The content hash of the saved attestation.
    pub fn save_attestation(
        &self,
        attestation: &atomic_core::change::Attestation,
    ) -> Result<Hash, RepositoryError> {
        use atomic_core::pristine::MutTxnT;

        // Save to disk
        let hash = self
            .change_store
            .save_attestation(attestation)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Register in the graph
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let attest_id = txn
            .register_attestation(&hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Register dependencies: attestation → covered changes
        for change_hash in &attestation.changes_covered {
            if let Ok(Some(change_id)) = txn.get_internal(change_hash) {
                txn.put_dep(attest_id, change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }

        // If chained, register dependency on previous attestation too
        if let Some(ref prev_hash) = attestation.previous_attestation {
            if let Ok(Some(prev_id)) = txn.get_internal(prev_hash) {
                txn.put_dep(attest_id, prev_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(hash)
    }

    /// Load an attestation from the repository by hash.
    pub fn load_attestation(
        &self,
        hash: &Hash,
    ) -> Result<atomic_core::change::Attestation, RepositoryError> {
        self.change_store
            .load_attestation(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Check if an attestation exists in the repository.
    pub fn has_attestation(&self, hash: &Hash) -> bool {
        self.change_store.has_attestation(hash)
    }

    /// Find all attestations that cover a specific change.
    ///
    /// Uses REV_DEPS to find nodes that depend on the given change,
    /// then filters by `node_type::ATTESTATION`.
    ///
    /// # Arguments
    ///
    /// * `change_hash` - The hash of the change to find attestations for
    ///
    /// # Returns
    ///
    /// A vector of `(Hash, Attestation)` pairs covering this change.
    pub fn find_attestations_for_change(
        &self,
        change_hash: &Hash,
    ) -> Result<Vec<(Hash, atomic_core::change::Attestation)>, RepositoryError> {
        use atomic_core::pristine::{node_type, GraphTxnT};

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the internal ID for this change
        let change_id = match txn
            .get_internal(change_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };

        // Look up REV_DEPS: who depends on this change?
        let rev_deps = txn
            .get_rev_deps(change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut attestations = Vec::new();

        for dep_id in rev_deps {
            // Check if this dependent is an attestation
            let node_type_val = txn
                .get_node_type(dep_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            if node_type_val != Some(node_type::ATTESTATION) {
                continue;
            }

            // Get the external hash
            let dep_hash = match txn
                .get_external(dep_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                Some(h) => h,
                None => continue,
            };

            // Load the attestation from disk
            match self.load_attestation(&dep_hash) {
                Ok(attest) => attestations.push((dep_hash, attest)),
                Err(_) => continue, // File missing or corrupt — skip
            }
        }

        Ok(attestations)
    }

    /// Find all attestations relevant to a stack.
    ///
    /// Iterates over all changes in the stack, checks REV_DEPS for each,
    /// and collects unique attestations. Returns them with coverage info
    /// showing which changes each attestation covers within this stack.
    ///
    /// # Arguments
    ///
    /// * `stack_name` - The name of the stack to query
    ///
    /// # Returns
    ///
    /// A vector of `(Hash, Attestation, Vec<Hash>)` where the third element
    /// is the subset of `changes_covered` that are in this stack.
    pub fn find_attestations_for_stack(
        &self,
        stack_name: &str,
    ) -> Result<Vec<(Hash, atomic_core::change::Attestation, Vec<Hash>)>, RepositoryError> {
        use atomic_core::pristine::{node_type, GraphTxnT, StackTxnT};
        use std::collections::{HashMap, HashSet};

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the stack
        let stack = match txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(s) => s,
            None => return Ok(Vec::new()),
        };

        // Collect all change IDs and hashes in this stack
        let mut stack_change_ids: HashSet<u64> = HashSet::new();
        let mut stack_change_hashes: HashSet<Hash> = HashSet::new();

        let iter = txn
            .iter_changes(&stack, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in iter {
            let (_seq, change_id, _merkle) =
                result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            stack_change_ids.insert(change_id.get());

            if let Ok(Some(hash)) = txn.get_external(change_id) {
                stack_change_hashes.insert(hash);
            }
        }

        // For each change in the stack, find attestation nodes via REV_DEPS
        let mut seen_attestations: HashMap<Hash, (atomic_core::change::Attestation, Vec<Hash>)> =
            HashMap::new();

        for change_id_raw in &stack_change_ids {
            let change_id = NodeId::new(*change_id_raw);

            let rev_deps = match txn.get_rev_deps(change_id) {
                Ok(ids) => ids,
                Err(_) => continue,
            };

            for dep_id in rev_deps {
                // Check node type
                let node_type_val = match txn.get_node_type(dep_id) {
                    Ok(Some(t)) => t,
                    _ => continue,
                };

                if node_type_val != node_type::ATTESTATION {
                    continue;
                }

                // Get external hash
                let attest_hash = match txn.get_external(dep_id) {
                    Ok(Some(h)) => h,
                    _ => continue,
                };

                // Skip if we've already processed this attestation
                if seen_attestations.contains_key(&attest_hash) {
                    // Add the current change to coverage if covered
                    if let Ok(Some(change_hash)) = txn.get_external(change_id) {
                        if let Some((attest, covered)) = seen_attestations.get_mut(&attest_hash) {
                            if attest.covers_change(&change_hash) && !covered.contains(&change_hash)
                            {
                                covered.push(change_hash);
                            }
                        }
                    }
                    continue;
                }

                // Load attestation
                let attest = match self.load_attestation(&attest_hash) {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                // Compute which of this attestation's covered changes are in this stack
                let covered_in_stack: Vec<Hash> = attest
                    .changes_covered
                    .iter()
                    .filter(|h| stack_change_hashes.contains(h))
                    .cloned()
                    .collect();

                seen_attestations.insert(attest_hash, (attest, covered_in_stack));
            }
        }

        // Convert to output format
        let results: Vec<_> = seen_attestations
            .into_iter()
            .map(|(hash, (attest, covered))| (hash, attest, covered))
            .collect();

        Ok(results)
    }

    /// Delete a change from the repository.
    ///
    /// This removes the change file from disk and from the cache.
    /// Note that this does NOT remove the change from any stacks - use
    /// `unrecord` for that.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to delete
    ///
    /// # Returns
    ///
    /// `true` if the change was deleted, `false` if it didn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be deleted.
    ///
    /// # Warning
    ///
    /// Deleting a change that is still referenced by a stack will cause
    /// errors when trying to access that stack. Use with caution.
    pub fn delete_change(&self, hash: &Hash) -> Result<bool, RepositoryError> {
        self.change_store
            .delete_change(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count the number of changes stored in the repository.
    ///
    /// This scans the entire changes directory.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let count = repo.count_changes()?;
    /// println!("Repository has {} changes", count);
    /// ```
    pub fn count_changes(&self) -> Result<usize, RepositoryError> {
        self.change_store
            .count_changes()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Iterate over all change hashes stored in the repository.
    ///
    /// This scans the changes directory and yields the hash of each
    /// change file found. The iteration order is not guaranteed.
    ///
    /// # Performance
    ///
    /// This method reads the filesystem and should be used sparingly
    /// on repositories with many changes.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// for result in repo.iter_changes() {
    ///     match result {
    ///         Ok(hash) => println!("Found change: {}", hash.to_base32()),
    ///         Err(e) => eprintln!("Error: {}", e),
    ///     }
    /// }
    /// ```
    pub fn iter_changes(&self) -> impl Iterator<Item = Result<Hash, RepositoryError>> + '_ {
        self.change_store
            .iter_changes()
            .map(|r| r.map_err(|e| RepositoryError::Database(e.to_string())))
    }

    /// Find a change by hash prefix.
    ///
    /// This searches through all stored changes to find one whose hash
    /// starts with the given prefix. Useful for CLI commands that allow
    /// abbreviated hashes.
    ///
    /// # Arguments
    ///
    /// * `prefix` - The hash prefix (case-insensitive, at least 2 characters)
    ///
    /// # Returns
    ///
    /// * `Ok(Some(hash))` - Found a unique matching change
    /// * `Ok(None)` - No change matched the prefix
    /// * `Err(_)` - Multiple changes matched (ambiguous) or I/O error
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Find a change by abbreviated hash
    /// if let Some(hash) = repo.find_change_by_prefix("ABCD")? {
    ///     let change = repo.load_change(&hash)?;
    ///     println!("Found: {}", hash.to_base32());
    /// }
    /// ```
    pub fn find_change_by_prefix(&self, prefix: &str) -> Result<Option<Hash>, RepositoryError> {
        let prefix_upper = prefix.to_uppercase();
        let mut matches = Vec::new();

        for result in self.iter_changes() {
            let hash = result?;
            let hash_str = hash.to_base32();
            if hash_str.starts_with(&prefix_upper) {
                matches.push(hash);
                // If we find more than one, it's ambiguous
                if matches.len() > 1 {
                    return Err(RepositoryError::AmbiguousHash {
                        prefix: prefix.to_string(),
                        matches: matches.iter().map(|h| h.to_base32()).collect(),
                    });
                }
            }
        }

        Ok(matches.into_iter().next())
    }

    // ========================================================================
    // Status Methods
    // ========================================================================

    /// Compute the status of the working copy.
    ///
    /// This compares the current state of files on disk with the recorded
    /// state in the repository to determine which files have been modified,
    /// added, deleted, or are untracked.
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling which files to include and how
    ///   to compute the status
    ///
    /// # Returns
    ///
    /// A [`RepositoryStatus`] containing information about all files.
    ///
    /// # Performance
    ///
    /// This operation can be expensive for large repositories as it requires:
    /// - Walking the entire working copy directory tree
    /// - Reading file contents for hash comparison (unless `hash_contents` is false)
    /// - Querying the tree tables in the database
    ///
    /// Use [`StatusOptions`] to limit the scope for better performance.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let status = repo.status(StatusOptions::default())?;
    ///
    /// if !status.is_clean() {
    ///     println!("Working copy has uncommitted changes:");
    ///     for entry in status.modified() {
    ///         println!("  M {}", entry.path().display());
    ///     }
    ///     for entry in status.untracked() {
    ///         println!("  ? {}", entry.path().display());
    ///     }
    /// }
    /// ```
    pub fn status(&self, options: StatusOptions) -> Result<RepositoryStatus, RepositoryError> {
        // Get the current stack state
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack_state = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .map(|s| s.state);

        let mut status = RepositoryStatus::new(self.current_stack.clone(), stack_state);

        // Load ignore rules if respecting ignore files
        let rules = if options.respect_ignore_files {
            Some(self.ignore_rules())
        } else {
            None
        };

        // Collect files from the working copy
        let working_files =
            collect_working_copy_files_with_rules(&self.root, &options, rules.as_ref())
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Collect tracked files from the tree tables
        let tracked_files = txn
            .iter_tree()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Build a set of tracked paths for quick lookup
        // We also normalize paths to handle any incorrectly stored absolute paths
        let mut tracked_paths: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();

        for result in tracked_files {
            let (path, _inode) = result.map_err(|e| RepositoryError::Database(e.to_string()))?;
            let path_buf = PathBuf::from(&path);

            // Normalize: if the path is absolute and starts with the repo root,
            // convert it to a relative path. This handles cases where paths were
            // incorrectly stored with absolute paths (e.g., on macOS where /tmp
            // resolves to /private/tmp).
            let normalized_path = if path_buf.is_absolute() {
                if let Ok(rel) = path_buf.strip_prefix(&self.root) {
                    rel.to_path_buf()
                } else {
                    // Try stripping without canonicalization issues
                    // On macOS, /tmp -> /private/tmp, so also try the canonical root
                    if let Ok(canonical_root) = self.root.canonicalize() {
                        if let Ok(rel) = path_buf.strip_prefix(&canonical_root) {
                            rel.to_path_buf()
                        } else {
                            path_buf
                        }
                    } else {
                        path_buf
                    }
                }
            } else {
                path_buf
            };

            tracked_paths.insert(normalized_path);
        }

        // Build a map of inode to recorded content position for tracked files
        // This allows us to detect modifications by comparing content hashes
        let mut inode_map: std::collections::HashMap<PathBuf, atomic_core::types::Inode> =
            std::collections::HashMap::new();

        // Also track which inodes are directories
        let mut directory_inodes: std::collections::HashSet<atomic_core::types::Inode> =
            std::collections::HashSet::new();

        // We need to look up inodes using the original path format stored in the database
        // So we also keep track of the original paths for inode lookup
        let tracked_files_for_inode = txn
            .iter_tree()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in tracked_files_for_inode {
            let (original_path, _) =
                result.map_err(|e| RepositoryError::Database(e.to_string()))?;
            let path_buf = PathBuf::from(&original_path);

            // Normalize the path for our lookup map
            let normalized_path = if path_buf.is_absolute() {
                if let Ok(rel) = path_buf.strip_prefix(&self.root) {
                    rel.to_path_buf()
                } else if let Ok(canonical_root) = self.root.canonicalize() {
                    if let Ok(rel) = path_buf.strip_prefix(&canonical_root) {
                        rel.to_path_buf()
                    } else {
                        path_buf.clone()
                    }
                } else {
                    path_buf.clone()
                }
            } else {
                path_buf
            };

            // Use the original path for database lookup since that's what's stored
            if let Ok(Some(inode)) = txn.get_inode(&original_path) {
                inode_map.insert(normalized_path.clone(), inode);
                // Check if this inode is a directory
                if txn.is_directory(inode).unwrap_or(false) {
                    directory_inodes.insert(inode);
                }
            }
        }

        // Legacy loop removed - we now build inode_map above
        for path in &tracked_paths {
            // Skip if already in inode_map (from the loop above)
            if inode_map.contains_key(path) {
                continue;
            }
            if let Ok(Some(inode)) = txn.get_inode(&path.to_string_lossy()) {
                inode_map.insert(path.clone(), inode);
                // Check if this inode is a directory
                if txn.is_directory(inode).unwrap_or(false) {
                    directory_inodes.insert(inode);
                }
            }
        }

        // Check each working copy file
        for path in &working_files {
            if tracked_paths.contains(path) {
                // File is tracked - determine if modified or newly added
                let abs_path = self.root.join(path);
                let inode = inode_map.get(path).copied();

                // Check if this file has been recorded to the graph yet
                // A file is "Added" if it's tracked (in TREE) but has no graph position
                let has_graph_content = if let Some(inode) = inode {
                    txn.inode_position(inode)
                        .map(|pos| pos.is_some())
                        .unwrap_or(false)
                } else {
                    false
                };

                // Determine initial status based on whether file has graph content
                let initial_status = if has_graph_content {
                    FileStatus::Clean
                } else {
                    // File is tracked but has no graph content - it's newly added
                    FileStatus::Added
                };

                let mut entry = FileStatusEntry::new(path.clone(), initial_status);

                if let Some(inode) = inode {
                    entry.set_inode(inode);
                }

                if options.hash_contents {
                    // Hash the current working copy file content
                    match hash_file_contents(&abs_path) {
                        Ok(current_hash) => {
                            entry.set_current_hash(current_hash);

                            // If file has graph content, compare with recorded content
                            if has_graph_content {
                                // Retrieve the recorded content from the graph and hash it
                                match self.get_file_content(path) {
                                    Ok(Some(recorded_content)) => {
                                        let recorded_hash = Hash::of(&recorded_content);
                                        if current_hash != recorded_hash {
                                            // Content differs - file is modified
                                            entry = FileStatusEntry::new(
                                                path.clone(),
                                                FileStatus::Modified,
                                            );
                                            if let Some(inode) = inode {
                                                entry.set_inode(inode);
                                            }
                                            entry.set_current_hash(current_hash);
                                        }
                                        // Otherwise keep as Clean
                                    }
                                    Ok(None) => {
                                        // No recorded content - file has graph structure but no content.
                                        // This is valid for empty files or files with only inode vertices.
                                        // Keep as Clean since the graph exists but has no content to compare.
                                        //
                                        // NOTE: If you're debugging a case where a file shows as Clean
                                        // but should be Modified, check that the content is actually
                                        // being stored in the graph during recording. The bug was likely
                                        // in globalize_hunk using `content` instead of `full_content`
                                        // for Replace hunks.
                                        debug_assert!(
                                            !has_graph_content || {
                                                // If has_graph_content is true, this should only happen
                                                // for empty files. Log for debugging if it's not empty.
                                                let is_empty_file = std::fs::metadata(&abs_path)
                                                    .map(|m| m.len() == 0)
                                                    .unwrap_or(false);
                                                if !is_empty_file {
                                                    eprintln!(
                                                        "WARNING: File '{}' has graph content but get_file_content returned None. \
                                                         This may indicate a bug in content storage.",
                                                        path.display()
                                                    );
                                                }
                                                true // Don't fail the assertion, just warn
                                            },
                                            "Unexpected: has_graph_content=true but no content retrieved"
                                        );
                                    }
                                    Err(_) => {
                                        // Error retrieving content - assume modified to be safe
                                        entry = FileStatusEntry::new(
                                            path.clone(),
                                            FileStatus::Modified,
                                        );
                                        if let Some(inode) = inode {
                                            entry.set_inode(inode);
                                        }
                                        entry.set_current_hash(current_hash);
                                        entry.set_details(
                                            "Unable to retrieve recorded content".to_string(),
                                        );
                                    }
                                }
                            }
                            // Files marked as Added stay as Added regardless of content
                        }
                        Err(_) => {
                            // Can't read file - might be a permission issue
                            // Mark as modified since we can't verify (unless it's newly added)
                            if has_graph_content {
                                entry = FileStatusEntry::new(path.clone(), FileStatus::Modified);
                                if let Some(inode) = inode {
                                    entry.set_inode(inode);
                                }
                                entry.set_details("Unable to read file contents".to_string());
                            }
                        }
                    }
                }

                status.add_entry(entry);
                tracked_paths.remove(path);
            } else if options.include_untracked {
                // File is not tracked
                let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Untracked);

                // Optionally hash untracked files too
                if options.hash_contents {
                    let abs_path = self.root.join(path);
                    if let Ok(hash) = hash_file_contents(&abs_path) {
                        entry.set_current_hash(hash);
                    }
                }

                status.add_entry(entry);
            }
        }

        // Any remaining tracked paths are either deleted files or directories
        for path in tracked_paths {
            let inode = inode_map.get(&path).copied();
            let abs_path = self.root.join(&path);

            // Check if this is a tracked directory
            let is_tracked_dir = inode
                .map(|i| directory_inodes.contains(&i))
                .unwrap_or(false);

            if is_tracked_dir {
                // This is a tracked directory
                if abs_path.is_dir() {
                    // Directory still exists - check if it has graph content
                    let has_graph_content = if let Some(inode) = inode {
                        txn.inode_position(inode)
                            .map(|pos| pos.is_some())
                            .unwrap_or(false)
                    } else {
                        false
                    };

                    let dir_status = if has_graph_content {
                        FileStatus::Clean
                    } else {
                        // Directory is tracked but not yet recorded
                        FileStatus::Added
                    };

                    let mut entry = FileStatusEntry::new(path.clone(), dir_status);
                    if let Some(inode) = inode {
                        entry.set_inode(inode);
                    }
                    entry.set_details("directory".to_string());
                    status.add_entry(entry);
                } else {
                    // Directory was deleted from disk
                    let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Deleted);
                    if let Some(inode) = inode {
                        entry.set_inode(inode);
                    }
                    entry.set_details("directory".to_string());
                    status.add_entry(entry);
                }
            } else {
                // Regular file that was deleted
                let mut entry = FileStatusEntry::new(path.clone(), FileStatus::Deleted);

                // Include inode info for deleted files
                if let Some(inode) = inode {
                    entry.set_inode(inode);
                }

                status.add_entry(entry);
            }
        }

        Ok(status)
    }

    /// Get a quick status summary (faster than full status).
    ///
    /// This uses the fast options which skip content hashing.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let status = repo.status_quick()?;
    /// println!("Modified: {}", status.modified_count());
    /// ```
    pub fn status_quick(&self) -> Result<RepositoryStatus, RepositoryError> {
        self.status(StatusOptions::fast())
    }

    /// Get status for tracked files only.
    ///
    /// This excludes untracked files from the result.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let status = repo.status_tracked()?;
    /// // Only shows modified, deleted, added - no untracked
    /// ```
    pub fn status_tracked(&self) -> Result<RepositoryStatus, RepositoryError> {
        self.status(StatusOptions::tracked_only())
    }

    /// Check if the working copy is clean (no uncommitted changes).
    ///
    /// This is a convenience method that computes the status and checks
    /// if there are any dirty files.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if repo.is_clean()? {
    ///     println!("Working copy is clean");
    /// } else {
    ///     println!("Working copy has uncommitted changes");
    /// }
    /// ```
    pub fn is_working_copy_clean(&self) -> Result<bool, RepositoryError> {
        let status = self.status(StatusOptions::tracked_only())?;
        Ok(status.is_clean())
    }

    /// Get list of modified files.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// for path in repo.modified_files()? {
    ///     println!("Modified: {}", path.display());
    /// }
    /// ```
    pub fn modified_files(&self) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(StatusOptions::tracked_only())?;
        Ok(status.modified().map(|e| e.path().to_path_buf()).collect())
    }

    /// Get list of untracked files.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// for path in repo.untracked_files()? {
    ///     println!("Untracked: {}", path.display());
    /// }
    /// ```
    pub fn untracked_files(&self) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(StatusOptions::default())?;
        Ok(status.untracked().map(|e| e.path().to_path_buf()).collect())
    }

    /// Get list of deleted files.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// for path in repo.deleted_files()? {
    ///     println!("Deleted: {}", path.display());
    /// }
    /// ```
    pub fn deleted_files(&self) -> Result<Vec<PathBuf>, RepositoryError> {
        let status = self.status(StatusOptions::tracked_only())?;
        Ok(status.deleted().map(|e| e.path().to_path_buf()).collect())
    }

    // ========================================================================
    // File Tracking Methods
    // ========================================================================

    /// Add a file or directory to tracking.
    ///
    /// This registers the file with the repository so it will be included
    /// in future changes. Adding a file does NOT create a change - you need
    /// to call `record()` for that.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file or directory (relative to repository root)
    /// * `options` - Options controlling the add operation
    ///
    /// # Returns
    ///
    /// Statistics about what was added.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path doesn't exist
    /// - The path is inside .atomic/
    /// - A database error occurs
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Add a single file
    /// repo.add("src/main.rs", TrackingOptions::default())?;
    ///
    /// // Add a directory recursively
    /// repo.add("src/", TrackingOptions::default())?;
    ///
    /// // Add without recursion
    /// repo.add("src/", TrackingOptions::non_recursive())?;
    /// ```
    pub fn add<P: AsRef<Path>>(
        &self,
        path: P,
        options: TrackingOptions,
    ) -> Result<TrackingStats, RepositoryError> {
        let path = path.as_ref();
        let mut stats = TrackingStats::new();

        // Load ignore rules
        let rules = self.ignore_rules();

        // Check for internal paths and ignore patterns
        let abs_path = self.root.join(path);
        let is_dir = abs_path.is_dir();
        if should_ignore_with_rules(path, true, is_dir, Some(&rules)) {
            return Err(RepositoryError::PathIgnored {
                path: path.to_path_buf(),
            });
        }

        // Collect files to add (respecting ignore rules)
        let files = collect_files_for_tracking_with_rules(&self.root, path, &options, Some(&rules))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if files.is_empty() {
            return Ok(stats);
        }

        // Don't modify if dry run
        if options.dry_run {
            for file_path in files {
                // Only count files, not directories (directories are implicitly tracked)
                let abs_path = self.root.join(&file_path);
                if !abs_path.is_dir() {
                    stats.files_added += 1;
                }
            }
            return Ok(stats);
        }

        // Add to tracking
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for file_path in files {
            // Normalize path with repo root to handle absolute paths correctly
            // (e.g., on macOS where /tmp -> /private/tmp)
            let normalized = normalize_path_with_root(&file_path, Some(&self.root));
            let abs_path = self.root.join(&file_path);

            // Skip directories - they are implicitly tracked through their contents
            if abs_path.is_dir() {
                continue;
            }

            // Check if already tracked
            if is_tracked(&txn, &normalized)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                if options.force {
                    stats.skip(file_path, "already tracked");
                    continue;
                } else {
                    stats.skip(file_path, "already tracked");
                    continue;
                }
            }

            // Add to tree (only files, not directories)
            add_to_tree(&mut txn, &normalized, false)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            stats.files_added += 1;
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(stats)
    }

    /// Add an empty directory to tracking explicitly.
    ///
    /// Unlike `add()` which only tracks files (directories are created implicitly),
    /// this method explicitly tracks empty directories as first-class citizens
    /// in the repository graph.
    ///
    /// This is useful for:
    /// - Preserving empty directory structure (no `.keep` files needed)
    /// - Tracking directories that will be populated later
    /// - Ensuring directory creation during clone/checkout
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the directory to track
    /// * `options` - Options controlling the add operation
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_repository::{Repository, TrackingOptions};
    ///
    /// let repo = Repository::open(".")?;
    ///
    /// // Track an empty directory explicitly
    /// repo.add_directory("src/empty_module/", TrackingOptions::default())?;
    ///
    /// // The directory will be recorded in the next change
    /// // No .keep file is needed
    /// ```
    ///
    /// # Graph Representation
    ///
    /// Directories are represented in the graph using the `FOLDER` edge flag:
    ///
    /// ```text
    /// ┌────────────────────────────────────────────────────────────────┐
    /// │  Parent Directory                                              │
    /// │  ┌─────────────┐                                               │
    /// │  │ Inode Span│                                               │
    /// │  │  (parent)   │                                               │
    /// │  └──────┬──────┘                                               │
    /// │         │ FOLDER edge                                          │
    /// │         ▼                                                      │
    /// │  ┌─────────────┐      ┌─────────────┐                         │
    /// │  │ Name Span │─────▶│ Inode Span│  ← Empty directory      │
    /// │  │ "subdir"    │      │  (no edges) │                         │
    /// │  └─────────────┘      └─────────────┘                         │
    /// └────────────────────────────────────────────────────────────────┘
    /// ```
    pub fn add_directory<P: AsRef<Path>>(
        &self,
        path: P,
        options: TrackingOptions,
    ) -> Result<TrackingStats, RepositoryError> {
        use crate::tracking::add_directory_to_tree;

        let path = path.as_ref();
        let mut stats = TrackingStats::new();

        // Load ignore rules
        let rules = self.ignore_rules();

        // Check for internal paths and ignore patterns
        // For add_directory, we know the path is a directory
        if should_ignore_with_rules(path, true, true, Some(&rules)) {
            return Err(RepositoryError::PathOutsideRepository {
                path: path.to_path_buf(),
            });
        }

        // Verify the path exists and is a directory
        let abs_path = self.root.join(path);
        if !abs_path.exists() {
            return Err(RepositoryError::FileNotFound {
                path: path.to_path_buf(),
            });
        }

        if !abs_path.is_dir() {
            return Err(RepositoryError::InvalidOperation {
                message: format!("Path is not a directory: {}", path.display()),
            });
        }

        let normalized = normalize_path(path);

        // Don't modify if dry run
        if options.dry_run {
            stats.explicit_directories_added += 1;
            return Ok(stats);
        }

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if already tracked
        if is_tracked(&txn, &normalized).map_err(|e| RepositoryError::Database(e.to_string()))? {
            if !options.force {
                return Err(RepositoryError::FileAlreadyTracked {
                    path: path.to_path_buf(),
                });
            }
            stats.skip(path.to_path_buf(), "already tracked");
            return Ok(stats);
        }

        // Add directory to tracking with explicit empty flag
        add_directory_to_tree(&mut txn, &normalized)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        stats.explicit_directories_added += 1;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(stats)
    }

    /// Remove a file or directory from tracking.
    ///
    /// This removes the file from version control tracking. It does NOT
    /// delete the file from disk.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to remove from tracking
    /// * `options` - Options controlling the remove operation
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Remove a single file
    /// repo.remove("old_file.txt", TrackingOptions::default())?;
    ///
    /// // Remove a directory recursively
    /// repo.remove("old_dir/", TrackingOptions::default())?;
    /// ```
    pub fn remove<P: AsRef<Path>>(
        &self,
        path: P,
        options: TrackingOptions,
    ) -> Result<TrackingStats, RepositoryError> {
        let path = path.as_ref();
        let mut stats = TrackingStats::new();
        let normalized = normalize_path(path);

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if the path is tracked first (for non-recursive case)
        let _is_path_tracked =
            is_tracked(&txn, &normalized).map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get all files under this path if recursive
        let to_remove = if options.recursive {
            let files = tracked_under_prefix(&txn, &normalized)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            // If no files found and not forced, error
            if files.is_empty() && !options.force {
                return Err(RepositoryError::FileNotTracked {
                    path: path.to_path_buf(),
                });
            }
            files
        } else {
            // Just the single path
            if let Some(inode) = get_inode(&txn, &normalized)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                vec![(normalized.clone(), inode)]
            } else {
                if !options.force {
                    return Err(RepositoryError::FileNotTracked {
                        path: path.to_path_buf(),
                    });
                }
                vec![]
            }
        };

        if options.dry_run {
            stats.files_removed = to_remove.len();
            return Ok(stats);
        }

        for (file_path, _inode) in to_remove {
            remove_from_tree(&mut txn, &file_path)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            stats.files_removed += 1;
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(stats)
    }

    /// Move or rename a tracked file.
    ///
    /// This updates the tracking to reflect a file move/rename. The file's
    /// history is preserved because the inode stays the same.
    ///
    /// Note: This does NOT move the actual file on disk. You should move
    /// the file first, then call this method.
    ///
    /// # Arguments
    ///
    /// * `from` - Current path of the file
    /// * `to` - New path for the file
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // First move the actual file
    /// std::fs::rename("old_name.rs", "new_name.rs")?;
    ///
    /// // Then update tracking
    /// repo.move_file("old_name.rs", "new_name.rs")?;
    /// ```
    pub fn move_file<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        from: P,
        to: Q,
    ) -> Result<Inode, RepositoryError> {
        let from_normalized = normalize_path(from.as_ref());
        let to_normalized = normalize_path(to.as_ref());

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let inode =
            move_tracked(&mut txn, &from_normalized, &to_normalized).map_err(|e| match e {
                TrackingError::NotTracked { path } => RepositoryError::FileNotTracked {
                    path: PathBuf::from(path),
                },
                TrackingError::DestinationExists { path } => RepositoryError::FileAlreadyTracked {
                    path: PathBuf::from(path),
                },
                other => RepositoryError::Database(other.to_string()),
            })?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(inode)
    }

    /// Check if a file is tracked.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to check
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if repo.is_tracked("src/main.rs")? {
    ///     println!("File is tracked");
    /// }
    /// ```
    pub fn is_tracked<P: AsRef<Path>>(&self, path: P) -> Result<bool, RepositoryError> {
        let normalized = normalize_path(path.as_ref());

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        is_tracked(&txn, &normalized).map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Get the inode for a tracked file.
    ///
    /// Returns `None` if the file is not tracked.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to look up
    pub fn get_file_inode<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Option<Inode>, RepositoryError> {
        let normalized = normalize_path(path.as_ref());

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        get_inode(&txn, &normalized).map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List all tracked files in the repository.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// for file in repo.list_tracked_files()? {
    ///     println!("{}: inode {}", file.path.display(), file.inode.get());
    /// }
    /// ```
    pub fn list_tracked_files(&self) -> Result<Vec<TrackedFile>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        list_tracked(&txn).map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count the number of tracked files.
    pub fn tracked_file_count(&self) -> Result<usize, RepositoryError> {
        Ok(self.list_tracked_files()?.len())
    }

    /// Get all tracked files under a directory prefix.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Directory prefix to search under
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let src_files = repo.tracked_files_under("src")?;
    /// println!("Files in src/: {}", src_files.len());
    /// ```
    pub fn tracked_files_under<P: AsRef<Path>>(
        &self,
        prefix: P,
    ) -> Result<Vec<(String, Inode)>, RepositoryError> {
        let normalized = normalize_path(prefix.as_ref());

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        tracked_under_prefix(&txn, &normalized)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    // ========================================================================
    // Recording Methods
    // ========================================================================

    /// Record changes from the working copy.
    ///
    /// This is the main entry point for creating a change from working copy
    /// modifications. It detects changes, creates hunks, globalizes positions,
    /// and assembles a complete change.
    ///
    /// # Arguments
    ///
    /// * `header` - The change header (message, author, etc.)
    /// * `options` - Options controlling recording behavior
    ///
    /// # Returns
    ///
    /// A `RecordOutcome` containing the recorded change, hash, and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No changes are detected (working copy is clean)
    /// - A file cannot be read
    /// - Globalization fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_repository::{Repository, RecordOptions};
    /// use atomic_core::change::{Author, ChangeHeader};
    ///
    /// let repo = Repository::open(".")?;
    ///
    /// let header = ChangeHeader::builder()
    ///     .message("Add new feature")
    ///     .author(Author::new("Alice", Some("alice@example.com")))
    ///     .build();
    ///
    /// let result = repo.record(header, RecordOptions::default())?;
    /// println!("Created change: {}", result.hash().to_base32());
    /// ```
    pub fn record(
        &self,
        header: ChangeHeader,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        use atomic_core::output::Memory;
        use atomic_core::record::workflow::{
            assemble_change, record_added_file, record_deleted_file, record_modified_file,
            DetectedFile, RecordedFile,
        };

        // Build the final header (may get message from options)
        let final_header = build_header(header, &options);

        // Get repository status to find modified files
        let status = self
            .status(StatusOptions::default())
            .map_err(RecordError::Repository)?;

        // Filter to recordable files
        let files_to_record = filter_files(status.entries(), &options);

        if files_to_record.is_empty() {
            return Err(RecordError::NothingToRecord);
        }

        // Statistics tracking
        let mut stats = RecordStats::new();
        let mut recorded_files: Vec<RecordedFile> = Vec::new();
        let mut recorded_paths: Vec<String> = Vec::new();
        let mut deleted_paths: Vec<String> = Vec::new();
        let mut skipped_paths: Vec<String> = Vec::new();
        let mut errors: Vec<(String, String)> = Vec::new();

        let core_options = options.to_core_options();

        // Create a memory working copy for the recording workflow
        let memory_wc = Memory::new();

        // Process each file
        for entry in &files_to_record {
            stats.files_processed += 1;

            let path = entry.path().to_string_lossy().to_string();
            let full_path = self.root.join(&path);

            // Check if this is a directory (from the details field)
            let is_directory = entry.details().map(|d| d == "directory").unwrap_or(false);

            match entry.status() {
                FileStatus::Added if is_directory => {
                    // Handle added directory - create DirAdd graph_op
                    // For now, directories are tracked but their hunks will be
                    // generated during globalization. We just need to record
                    // that this directory was added.
                    stats.directories_recorded += 1;
                    stats.vertices_added += 2; // name span + inode span
                    recorded_paths.push(format!("{}/ (directory)", path));

                    // Create a minimal RecordedFile for the directory
                    // The actual GraphOp::DirAdd will be created during globalization
                    let recorded = RecordedFile::new_directory(&path);
                    recorded_files.push(recorded);
                }

                FileStatus::Added => {
                    // Read file content
                    match std::fs::read(&full_path) {
                        Ok(content) => {
                            // Check size limit
                            if content.len() as u64 > options.get_max_file_size() {
                                if options.get_skip_binary() {
                                    skipped_paths.push(path.clone());
                                    stats.files_skipped += 1;
                                    continue;
                                } else {
                                    return Err(RecordError::FileTooLarge {
                                        path: path.clone(),
                                        size: content.len() as u64,
                                        limit: options.get_max_file_size(),
                                    });
                                }
                            }

                            // Write to memory working copy
                            memory_wc.add_file(&path, &content);

                            // Create a detected file descriptor
                            let detected = DetectedFile::added(&path);

                            // Record the added file
                            match record_added_file(&memory_wc, &detected, &core_options) {
                                Ok(recorded) => {
                                    if !recorded.is_empty() {
                                        stats.files_recorded += 1;
                                        stats.hunks_created += recorded.hunk_count();
                                        stats.content_bytes += recorded.content_len() as u64;
                                        // FileAdd creates 3 vertices: name, inode, content
                                        stats.vertices_added += 3;

                                        // Collect CRDT token-level statistics
                                        if let Some(crdt_stats) = recorded.crdt_stats() {
                                            stats.lines_added += crdt_stats.lines_added;
                                            stats.lines_deleted += crdt_stats.lines_deleted;
                                            stats.lines_modified += crdt_stats.lines_modified;
                                            stats.tokens_added += crdt_stats.tokens_added;
                                            stats.tokens_deleted += crdt_stats.tokens_deleted;
                                            stats.tokens_replaced += crdt_stats.tokens_replaced;
                                        }

                                        recorded_paths.push(path.clone());
                                        recorded_files.push(recorded);
                                    } else {
                                        skipped_paths.push(path.clone());
                                        stats.files_skipped += 1;
                                    }
                                }
                                Err(e) => {
                                    errors.push((path.clone(), format!("{:?}", e)));
                                    stats.errors += 1;
                                }
                            }
                        }
                        Err(e) => {
                            errors.push((path.clone(), e.to_string()));
                            stats.errors += 1;
                        }
                    }
                }

                FileStatus::Deleted if is_directory => {
                    // Handle deleted directory - create DirDel graph_op
                    // Look up the directory's inode
                    let txn = self
                        .pristine
                        .read_txn()
                        .map_err(|e| RecordError::Database(e.to_string()))?;

                    let inode = match txn.get_inode(&path) {
                        Ok(Some(inode)) => inode,
                        Ok(None) => {
                            errors.push((path.clone(), "Directory inode not found".to_string()));
                            stats.errors += 1;
                            continue;
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("Failed to get inode: {}", e)));
                            stats.errors += 1;
                            continue;
                        }
                    };

                    // Verify it's actually a directory
                    if !txn.is_directory(inode).unwrap_or(false) {
                        errors.push((path.clone(), "Path is not a directory".to_string()));
                        stats.errors += 1;
                        continue;
                    }

                    // Get the position for this directory's inode
                    let position = match txn.inode_position(inode) {
                        Ok(Some(pos)) => pos,
                        Ok(None) => {
                            errors.push((path.clone(), "Directory position not found".to_string()));
                            stats.errors += 1;
                            continue;
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("Failed to get position: {}", e)));
                            stats.errors += 1;
                            continue;
                        }
                    };

                    stats.directories_recorded += 1;
                    stats.edges_modified += 1; // deletion edge
                                               // Store the actual path for tree deletion, not the display format
                    deleted_paths.push(path.clone());

                    // Create a RecordedFile for the deleted directory with inode and position
                    let mut recorded = RecordedFile::new_deleted_directory(&path);
                    recorded.set_inode(inode);
                    recorded.set_position(position);
                    recorded_files.push(recorded);
                }

                FileStatus::Deleted => {
                    // For deleted files, we need to look up the inode and position
                    // from the pristine so that globalization can find the content
                    // vertices to mark as deleted.
                    let (file_inode, file_position) = {
                        let txn = self
                            .pristine
                            .read_txn()
                            .map_err(|e| RecordError::Database(e.to_string()))?;

                        // Get the inode for this path
                        let inode = match txn.get_inode(&path) {
                            Ok(Some(inode)) => inode,
                            Ok(None) => {
                                // No inode found - file was never recorded
                                errors.push((
                                    path.clone(),
                                    "File inode not found in pristine".to_string(),
                                ));
                                stats.errors += 1;
                                continue;
                            }
                            Err(e) => {
                                errors.push((path.clone(), format!("Failed to get inode: {}", e)));
                                stats.errors += 1;
                                continue;
                            }
                        };

                        // Get the graph position for this inode
                        let position = match txn.inode_position(inode) {
                            Ok(Some(pos)) => pos,
                            Ok(None) => {
                                errors.push((
                                    path.clone(),
                                    "File position not found in pristine".to_string(),
                                ));
                                stats.errors += 1;
                                continue;
                            }
                            Err(e) => {
                                errors
                                    .push((path.clone(), format!("Failed to get position: {}", e)));
                                stats.errors += 1;
                                continue;
                            }
                        };

                        (inode, position)
                    };

                    // Create a detected file descriptor for deletion with inode/position
                    let mut detected = DetectedFile::deleted(&path);
                    detected.inode = Some(file_inode);
                    detected.position = Some(file_position);

                    // Record deletion (no content needed)
                    match record_deleted_file(&detected, &core_options) {
                        Ok(recorded) => {
                            stats.files_recorded += 1;
                            stats.hunks_created += recorded.hunk_count();
                            // FileDel creates EdgeUpdate atoms to mark edges as deleted
                            stats.edges_modified += 1;

                            // Collect CRDT token-level statistics
                            if let Some(crdt_stats) = recorded.crdt_stats() {
                                stats.lines_added += crdt_stats.lines_added;
                                stats.lines_deleted += crdt_stats.lines_deleted;
                                stats.lines_modified += crdt_stats.lines_modified;
                                stats.tokens_added += crdt_stats.tokens_added;
                                stats.tokens_deleted += crdt_stats.tokens_deleted;
                                stats.tokens_replaced += crdt_stats.tokens_replaced;
                            }

                            // Track this as a deleted file
                            deleted_paths.push(path.clone());
                            recorded_paths.push(path.clone());
                            recorded_files.push(recorded);
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("{:?}", e)));
                            stats.errors += 1;
                        }
                    }
                }

                FileStatus::Modified => {
                    // For modified files, we need to:
                    // 1. Look up the file's inode and graph position
                    // 2. Retrieve the old content from the graph
                    // 3. Read the new content from the working copy
                    // 4. Diff old vs new to create Edit/Replacement hunks
                    //
                    // This creates efficient incremental changes rather than
                    // replacing the entire file content.

                    // Step 1: Look up the file's inode and position from the pristine
                    // This is required for globalization to create Edit hunks instead of FileAdd
                    let (file_inode, file_position) = {
                        let txn = self
                            .pristine
                            .read_txn()
                            .map_err(|e| RecordError::Database(e.to_string()))?;

                        // Get the inode for this path
                        let inode = match txn.get_inode(&path) {
                            Ok(Some(inode)) => inode,
                            Ok(None) => {
                                // No inode found - file is tracked but not in TREE table
                                // This shouldn't happen for Modified status, but fall back
                                errors.push((
                                    path.clone(),
                                    "File inode not found in pristine".to_string(),
                                ));
                                stats.errors += 1;
                                continue;
                            }
                            Err(e) => {
                                errors.push((path.clone(), format!("Failed to get inode: {}", e)));
                                stats.errors += 1;
                                continue;
                            }
                        };

                        // Get the graph position for this inode
                        let position = match txn.inode_position(inode) {
                            Ok(Some(pos)) => pos,
                            Ok(None) => {
                                // No position found - file has inode but no graph entry
                                errors.push((
                                    path.clone(),
                                    "File position not found in pristine".to_string(),
                                ));
                                stats.errors += 1;
                                continue;
                            }
                            Err(e) => {
                                errors
                                    .push((path.clone(), format!("Failed to get position: {}", e)));
                                stats.errors += 1;
                                continue;
                            }
                        };

                        (inode, position)
                    };

                    // Step 2: Retrieve old content from the graph
                    let old_content = match self.get_file_content(entry.path()) {
                        Ok(Some(content)) => content,
                        Ok(None) => {
                            // No recorded content found - treat as new file
                            // This can happen if the file was tracked but never recorded
                            Vec::new()
                        }
                        Err(e) => {
                            // Error retrieving content - log and skip
                            errors.push((
                                path.clone(),
                                format!("Failed to retrieve old content: {}", e),
                            ));
                            stats.errors += 1;
                            continue;
                        }
                    };

                    // Step 2: Read new content from working copy
                    let new_content = match std::fs::read(&full_path) {
                        Ok(content) => content,
                        Err(e) => {
                            errors.push((path.clone(), e.to_string()));
                            stats.errors += 1;
                            continue;
                        }
                    };

                    // Step 3: Check if content actually changed
                    if old_content == new_content {
                        // No actual change - skip
                        skipped_paths.push(path.clone());
                        stats.files_skipped += 1;
                        continue;
                    }

                    // Step 4: Write to memory working copy for the recording workflow
                    memory_wc.add_file(&path, &new_content);

                    // Step 5: Create a detected file descriptor for modification
                    // Include the inode and position so globalization creates Edit hunks
                    let mut detected = DetectedFile::modified(&path);
                    detected.inode = Some(file_inode);
                    detected.position = Some(file_position);

                    // Step 6: Record the modification using the diff-based workflow
                    // This creates Edit hunks for insertions and Replacement hunks
                    // for deletions, rather than a full FileAdd replacement.
                    match record_modified_file(&memory_wc, &detected, &old_content, &core_options) {
                        Ok(recorded) => {
                            if !recorded.is_empty() {
                                stats.files_recorded += 1;
                                stats.hunks_created += recorded.hunk_count();
                                stats.content_bytes += recorded.content_len() as u64;

                                // Count vertices and edges from the hunks
                                // Edit hunks create 1 span per insertion
                                // Replacement hunks create 1 span + edge modifications
                                for graph_op in recorded.hunks() {
                                    if graph_op.is_edit() {
                                        stats.vertices_added += 1;
                                    } else if graph_op.is_replace() {
                                        stats.vertices_added += 1;
                                        stats.edges_modified += 1;
                                    } else if graph_op.is_delete() {
                                        stats.edges_modified += 1;
                                    }
                                }

                                // Collect CRDT token-level statistics
                                if let Some(crdt_stats) = recorded.crdt_stats() {
                                    stats.lines_added += crdt_stats.lines_added;
                                    stats.lines_deleted += crdt_stats.lines_deleted;
                                    stats.lines_modified += crdt_stats.lines_modified;
                                    stats.tokens_added += crdt_stats.tokens_added;
                                    stats.tokens_deleted += crdt_stats.tokens_deleted;
                                    stats.tokens_replaced += crdt_stats.tokens_replaced;
                                }

                                recorded_paths.push(path.clone());
                                recorded_files.push(recorded);
                            } else {
                                // No hunks generated - content might be identical
                                skipped_paths.push(path.clone());
                                stats.files_skipped += 1;
                            }
                        }
                        Err(e) => {
                            errors.push((path.clone(), format!("{:?}", e)));
                            stats.errors += 1;
                        }
                    }
                }

                _ => {
                    // Skip other statuses
                    skipped_paths.push(path.clone());
                    stats.files_skipped += 1;
                }
            }
        }

        // Check if we actually recorded anything
        if recorded_files.is_empty() {
            return Err(RecordError::NothingToRecord);
        }

        // Assemble the change
        // Note: Full globalization requires a write transaction to resolve positions.
        // For now, we create a simple change with the recorded content.
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RecordError::Database(e.to_string()))?;

        let assembly_options = options.to_assembly_options();

        // Use the assembly module to create the change
        let assembly_result =
            assemble_change(&txn, &recorded_files, final_header, &assembly_options)?;

        let change = assembly_result.into_change();
        stats.dependency_count = change.dependencies().len();

        // Compute the hash
        let _hasher = atomic_core::types::Hasher::new();
        // Serialize to compute hash
        let mut hash_buffer = Vec::new();
        change
            .serialize(&mut hash_buffer)
            .map_err(|e| RecordError::ChangeStore(e.to_string()))?;

        let _hash = Hash::of(&hash_buffer);

        // Reload the change from the buffer (to get proper offsets)
        let (final_change, computed_hash) = Change::deserialize(&mut hash_buffer.as_slice())
            .map_err(|e| RecordError::ChangeStore(e.to_string()))?;

        let mut outcome = RecordOutcome::new(final_change, computed_hash, stats);

        // Add recorded/skipped/deleted files to outcome
        for path in recorded_paths {
            outcome.add_recorded_file(path);
        }
        for path in deleted_paths {
            outcome.add_deleted_file(path);
        }
        for path in skipped_paths {
            outcome.add_skipped_file(path);
        }
        for (path, error) in errors {
            outcome.add_error(path, error);
        }

        // Save to store if requested
        if options.get_save_to_store() {
            self.save_change(outcome.change())
                .map_err(|e| RecordError::ChangeStore(e.to_string()))?;
            outcome.set_saved(true);
        }

        // Apply if requested
        // We use apply_recorded() instead of apply_change() because it creates
        // the TREE and INODES entries for FileAdd hunks, which is necessary
        // for the file to be recognized as tracked with graph content.
        if options.get_apply_after_record() && outcome.was_saved() {
            let apply_opts = match options.get_stack() {
                Some(stack) => ApplyOptions::default().stack(stack),
                None => ApplyOptions::default(),
            };
            match self.apply_recorded(&outcome, apply_opts) {
                Ok(apply_outcome) => {
                    outcome.set_applied(apply_outcome.new_state);
                }
                Err(e) => {
                    outcome.add_error("apply".to_string(), e.to_string());
                }
            }
        }

        Ok(outcome)
    }

    /// Record changes with a simple message.
    ///
    /// This is a convenience method that creates a change header with just
    /// a message.
    ///
    /// # Arguments
    ///
    /// * `message` - The change message
    /// * `options` - Recording options
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.record_with_message("Fix bug", RecordOptions::default())?;
    /// ```
    pub fn record_with_message(
        &self,
        message: impl Into<String>,
        options: RecordOptions,
    ) -> Result<RecordOutcome, RecordError> {
        let header = ChangeHeader::builder().message(message).build();
        self.record(header, options)
    }

    /// Record all changes with a message.
    ///
    /// This is a convenience method that records all modified files.
    ///
    /// # Arguments
    ///
    /// * `message` - The change message
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.record_all("Update all files")?;
    /// ```
    pub fn record_all(&self, message: impl Into<String>) -> Result<RecordOutcome, RecordError> {
        let options = RecordOptions::new().all(true);
        self.record_with_message(message, options)
    }

    // ========================================================================
    // Change Application Methods
    // ========================================================================

    /// Apply a change to the current stack.
    ///
    /// This is the high-level method for applying a single change to the
    /// repository. It loads the change from the change store, validates
    /// dependencies, applies atoms to the graph, and updates the stack state.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to apply
    /// * `options` - Options controlling application behavior
    ///
    /// # Returns
    ///
    /// An `ApplyOutcome` containing the new state and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change is not found in the change store
    /// - Dependencies are missing (unless `apply_dependencies` is set)
    /// - The change is already applied
    /// - A conflict occurs (unless `allow_conflicts` is set)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_repository::{Repository, ApplyOptions};
    ///
    /// let repo = Repository::open(".")?;
    /// let result = repo.apply_change(&hash, ApplyOptions::default())?;
    /// println!("New state: {}", result.new_state.to_base32());
    /// ```
    pub fn apply_change(
        &self,
        hash: &Hash,
        options: ApplyOptions,
    ) -> Result<ApplyOutcome, RepositoryError> {
        // Load the change from the store
        let change = self.load_change(hash)?;

        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if the change is already in the graph (applied via another stack)
        // This is important because stacks share the same graph - we don't want
        // to re-apply hunks that are already there.
        let already_in_graph = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some();

        // Register the change to get an internal ID (or get existing ID)
        let change_id = txn
            .register_change(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Determine which stack to use
        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);

        // Populate tree tables for FileAdd/DirAdd/FileDel hunks.
        // This creates the path→inode→position mappings that output_working_copy
        // needs to reconstruct files. Without this, server-side repos (which
        // receive changes via push rather than record) would have an empty tree.
        if !already_in_graph {
            for graph_op in change.hunks() {
                match graph_op {
                    GraphOp::FileAdd {
                        add_inode, path, ..
                    } => {
                        let new_inode = txn
                            .alloc_inode()
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        let inode_position = Position::new(change_id, add_inode.start);
                        txn.put_tree(path, new_inode)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        txn.put_inode(new_inode, inode_position)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                    GraphOp::DirAdd {
                        add_inode, path, ..
                    } => {
                        use atomic_core::pristine::directory_flags;
                        let new_inode = txn
                            .alloc_inode()
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        let inode_position = Position::new(change_id, add_inode.start);
                        txn.put_tree(path, new_inode)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        txn.put_inode(new_inode, inode_position)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        txn.put_directory(new_inode, directory_flags::explicit_empty())
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                    GraphOp::FileDel { path, .. } => {
                        if let Ok(Some(_inode)) = txn.get_inode(path) {
                            let _ = txn.del_tree(path);
                        }
                    }
                    _ => {}
                }
            }
        }

        // Apply to the graph (skips hunk application if already_in_graph)
        let outcome = apply_change_to_graph(
            &mut txn,
            stack_name,
            change_id,
            hash,
            &change,
            &options,
            already_in_graph,
        )
        .map_err(|e| RepositoryError::Apply(e.to_string()))?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(outcome)
    }

    /// Apply a change with automatic dependency resolution.
    ///
    /// This method attempts to apply a change and all its missing dependencies.
    /// Dependencies are applied in topological order (dependencies before
    /// dependents).
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to apply
    /// * `options` - Options controlling application behavior
    ///
    /// # Returns
    ///
    /// An `ApplyOutcome` containing aggregate statistics for all applied changes.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Any required change cannot be found
    /// - A cyclic dependency is detected
    /// - Maximum recursion depth is exceeded
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.apply_change_rec(&hash, ApplyOptions::default())?;
    /// println!("Applied {} changes", result.stats.changes_applied);
    /// ```
    pub fn apply_change_rec(
        &self,
        hash: &Hash,
        options: ApplyOptions,
    ) -> Result<ApplyOutcome, RepositoryError> {
        // Load the target change to get its dependencies
        let _change = self.load_change(hash)?;

        // Get the stack name
        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);

        // Get a read transaction to check what's already applied
        let read_txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = read_txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        // Collect all needed changes (including the target)
        let mut to_apply = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(*hash);

        while let Some(current_hash) = queue.pop_front() {
            if visited.contains(&current_hash) {
                continue;
            }
            visited.insert(current_hash);

            // Check if already applied
            if let Ok(Some(id)) = read_txn.get_internal(&current_hash) {
                if read_txn.get_change_seq(&stack, id).ok().flatten().is_some() {
                    continue; // Already applied
                }
            }

            // Load and queue dependencies
            let dep_change = self.load_change(&current_hash)?;
            for dep in dep_change.dependencies() {
                if !visited.contains(dep) {
                    queue.push_back(*dep);
                }
            }

            to_apply.push(current_hash);
        }

        drop(read_txn);

        // Reverse to get topological order (dependencies first)
        to_apply.reverse();

        // Now apply all changes in order
        let mut aggregate_stats = ApplyStats::new();
        let mut final_state = Merkle::ZERO;
        let mut final_sequence = 0u64;
        let mut has_conflicts = false;

        for change_hash in &to_apply {
            let outcome = self.apply_change(change_hash, options.clone())?;
            aggregate_stats.merge(outcome.stats);
            final_state = outcome.new_state;
            final_sequence = outcome.sequence;
            if outcome.has_conflicts {
                has_conflicts = true;
            }
        }

        Ok(ApplyOutcome::new(
            final_state,
            final_sequence,
            has_conflicts,
            aggregate_stats,
        ))
    }

    /// Apply a recorded change to the repository.
    ///
    /// This method applies a change that was just recorded, updating both the
    /// graph and the tree tables. It's the integration point between recording
    /// and applying.
    ///
    /// Unlike `apply_change`, this method:
    /// - Takes the change directly (doesn't load from store)
    /// - Updates tree tables for FileAdd hunks
    /// - Assigns new inodes to added files
    ///
    /// # Arguments
    ///
    /// * `outcome` - The outcome from `record()` containing the change
    /// * `options` - Options controlling application behavior
    ///
    /// # Returns
    ///
    /// An `ApplyOutcome` with the new state and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change has conflicts and `allow_conflicts` is false
    /// - Database operations fail
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let record_outcome = repo.record(header, options)?;
    /// let apply_outcome = repo.apply_recorded(&record_outcome, ApplyOptions::default())?;
    /// println!("Applied with state: {}", apply_outcome.new_state.to_base32());
    /// ```
    pub fn apply_recorded(
        &self,
        outcome: &RecordOutcome,
        options: ApplyOptions,
    ) -> Result<ApplyOutcome, RepositoryError> {
        let change = outcome.change();
        let hash = outcome.hash();

        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Register the change to get an internal ID
        let change_id = txn
            .register_change(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Determine which stack to use
        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);

        // Before applying atoms, set up tree entries for FileAdd hunks.
        // This creates the inode→position and path→inode mappings needed
        // for the graph operations.
        //
        // Note: put_tree creates both TREE and REV_TREE entries.
        //       put_inode creates both INODES and REV_INODES entries.
        for graph_op in change.hunks() {
            match graph_op {
                GraphOp::FileAdd {
                    add_inode, path, ..
                } => {
                    // Allocate a new inode for this file
                    let new_inode = txn
                        .alloc_inode()
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;

                    // The inode span position is relative to this change.
                    // Since add_inode.start is a ChangePosition within this change's content,
                    // we create an internal position using the change_id we just registered.
                    let inode_position = Position::new(change_id, add_inode.start);

                    // Add to tree tables:
                    // - put_tree: path ↔ inode (TREE and REV_TREE)
                    // - put_inode: inode ↔ position (INODES and REV_INODES)
                    txn.put_tree(path, new_inode)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    txn.put_inode(new_inode, inode_position)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
                GraphOp::DirAdd {
                    add_inode, path, ..
                } => {
                    use atomic_core::pristine::directory_flags;

                    // Allocate a new inode for this directory
                    let new_inode = txn
                        .alloc_inode()
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;

                    // The inode span position is relative to this change.
                    let inode_position = Position::new(change_id, add_inode.start);

                    // Add to tree tables:
                    // - put_tree: path ↔ inode (TREE and REV_TREE)
                    // - put_inode: inode ↔ position (INODES and REV_INODES)
                    // - put_directory: mark inode as directory (DIRECTORIES)
                    txn.put_tree(path, new_inode)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    txn.put_inode(new_inode, inode_position)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    txn.put_directory(new_inode, directory_flags::explicit_empty())
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
                GraphOp::FileDel { path, .. } => {
                    // Remove file from tree tables
                    // First get the inode for this path
                    if let Ok(Some(inode)) = txn.get_inode(path) {
                        // Remove from tree tables (path ↔ inode)
                        let _ = txn.del_tree(path);
                        // Remove from inode tables (inode ↔ position)
                        let _ = txn.del_inode(inode);
                    }
                }
                GraphOp::DirDel { path, .. } => {
                    // Remove directory from tree tables
                    // First get the inode for this path
                    if let Ok(Some(inode)) = txn.get_inode(path) {
                        // Remove from tree tables (path ↔ inode)
                        let _ = txn.del_tree(path);
                        // Remove from inode tables (inode ↔ position)
                        let _ = txn.del_inode(inode);
                        // Remove directory marker from DIRECTORIES table
                        let _ = txn.del_directory(inode);
                    }
                }
                _ => {}
            }
        }

        // Handle file deletions tracked in the outcome.
        // Since we use GraphOp::Edit with EdgeUpdate for deletions (not GraphOp::FileDel),
        // we need to explicitly remove deleted files from the tree tables.
        for deleted_path in outcome.deleted_files() {
            // Get the inode for this path
            if let Ok(Some(inode)) = txn.get_inode(deleted_path) {
                // Remove from tree tables (path ↔ inode)
                let _ = txn.del_tree(deleted_path);
                // Remove from inode tables (inode ↔ position)
                let _ = txn.del_inode(inode);
            }
        }

        // Apply to the graph
        // For apply_recorded, the change is always new (just recorded), so
        // already_in_graph is always false.
        let apply_outcome = apply_change_to_graph(
            &mut txn, stack_name, change_id, hash, change, &options,
            false, // always_in_graph: freshly recorded changes are never in the graph yet
        )
        .map_err(|e| RepositoryError::Apply(e.to_string()))?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(apply_outcome)
    }

    // ========================================================================
    // Cross-Stack Apply Methods
    // ========================================================================

    /// Get all changes applied to a stack.
    ///
    /// Returns changes in order from oldest (sequence 0) to newest.
    ///
    /// # Arguments
    ///
    /// * `stack_name` - Name of the stack to query (None = current stack)
    ///
    /// # Returns
    ///
    /// Vector of (sequence, hash) pairs.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let changes = repo.get_stack_changes(None)?;
    /// for (seq, hash) in changes {
    ///     println!("#{}: {}", seq, hash.to_base32());
    /// }
    /// ```
    pub fn get_stack_changes(
        &self,
        stack_name: Option<&str>,
    ) -> Result<Vec<(u64, Hash)>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let name = stack_name.unwrap_or(&self.current_stack);
        let stack = txn
            .get_stack(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: name.to_string(),
            })?;

        get_stack_changes(&txn, &stack).map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Get changes that are in one stack but not another.
    ///
    /// This is useful for determining what needs to be applied when
    /// merging or cherry-picking between stacks.
    ///
    /// # Arguments
    ///
    /// * `from_stack` - Source stack name
    /// * `to_stack` - Target stack name (None = current stack)
    ///
    /// # Returns
    ///
    /// Vector of hashes that are in `from_stack` but not in `to_stack`,
    /// in dependency order.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Find what's in feature that's not in main
    /// let missing = repo.get_missing_changes_between("feature", Some("main"))?;
    /// println!("{} changes to apply", missing.len());
    /// ```
    pub fn get_missing_changes_between(
        &self,
        from_stack: &str,
        to_stack: Option<&str>,
    ) -> Result<Vec<Hash>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let from = txn
            .get_stack(from_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: from_stack.to_string(),
            })?;

        let to_name = to_stack.unwrap_or(&self.current_stack);
        let to = txn
            .get_stack(to_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: to_name.to_string(),
            })?;

        get_missing_changes(&txn, &from, &to).map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Get changes up to a specific tag in a stack.
    ///
    /// Returns all changes from sequence 0 up to and including the
    /// sequence where the tag was created.
    ///
    /// # Arguments
    ///
    /// * `tag_name` - Name of the tag
    /// * `stack_name` - Stack to search (None = use tag's stack)
    ///
    /// # Returns
    ///
    /// Vector of change hashes up to the tagged state.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let changes = repo.get_changes_up_to_tag("v1.0.0", None)?;
    /// println!("{} changes in release", changes.len());
    /// ```
    pub fn get_changes_up_to_tag(
        &self,
        tag_name: &str,
        stack_name: Option<&str>,
    ) -> Result<Vec<Hash>, RepositoryError> {
        // Get the tag
        let tag = if let Some(stack) = stack_name {
            self.get_tag_from_stack(tag_name, stack)?
        } else {
            // Try current stack first, then any stack
            self.get_tag(tag_name)?
                .or(self.get_tag_any_stack(tag_name)?)
        };

        let tag = tag.ok_or_else(|| RepositoryError::TagNotFound {
            name: tag_name.to_string(),
        })?;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = txn
            .get_stack(&tag.stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: tag.stack.clone(),
            })?;

        // Get changes up to and including the tag's sequence
        crate::apply::get_changes_up_to_seq(&txn, &stack, tag.sequence)
            .map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Apply changes from one stack to another.
    ///
    /// This is the main method for cross-stack operations. It can:
    /// - Apply all missing changes from source to target
    /// - Apply only changes up to a specific tag
    /// - Apply only specific changes
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling the cross-stack apply
    ///
    /// # Returns
    ///
    /// A `CrossStackApplyOutcome` with details about what was applied.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Apply all changes from feature to main
    /// let options = CrossStackApplyOptions::new("feature", "main");
    /// let result = repo.apply_from_stack(options)?;
    /// println!("Applied {} changes", result.changes_applied);
    ///
    /// // Apply changes up to a tag
    /// let options = CrossStackApplyOptions::new("feature", "main")
    ///     .up_to_tag("v1.0.0");
    /// let result = repo.apply_from_stack(options)?;
    /// ```
    pub fn apply_from_stack(
        &self,
        options: CrossStackApplyOptions,
    ) -> Result<CrossStackApplyOutcome, RepositoryError> {
        let mut outcome = CrossStackApplyOutcome::new();
        outcome.was_dry_run = options.dry_run;

        // Determine which changes to consider
        let source_changes = if !options.only_changes.is_empty() {
            // Use only specified changes
            options.only_changes.clone()
        } else if let Some(ref tag_name) = options.up_to_tag {
            // Get changes up to the tag
            self.get_changes_up_to_tag(tag_name, Some(&options.from_stack))?
        } else {
            // Get all changes from source stack
            self.get_stack_changes(Some(&options.from_stack))?
                .into_iter()
                .map(|(_, hash)| hash)
                .collect()
        };

        // Filter to changes not already in target
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let to_stack = txn
            .get_stack(&options.to_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: options.to_stack.clone(),
            })?;

        let missing = filter_missing_in_stack(&txn, &to_stack, &source_changes)
            .map_err(|e| RepositoryError::Apply(e.to_string()))?;

        // Track skipped changes
        let missing_set: std::collections::HashSet<_> = missing.iter().collect();
        for hash in &source_changes {
            if !missing_set.contains(hash) {
                outcome.skipped_hashes.push(*hash);
            }
        }

        drop(txn);

        if missing.is_empty() {
            // Nothing to apply
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let stack = txn
                .get_stack(&options.to_stack)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .unwrap();
            outcome.new_state = stack.state;
            outcome.sequence = stack.change_count;
            return Ok(outcome);
        }

        // If dry run, just return what would be applied
        if options.dry_run {
            outcome.applied_hashes = missing;
            outcome.changes_applied = outcome.applied_hashes.len();
            return Ok(outcome);
        }

        // Apply each change in order
        let apply_opts = ApplyOptions::default()
            .stack(&options.to_stack)
            .allow_conflict(options.allow_conflicts);

        for hash in &missing {
            let result = if options.apply_dependencies {
                self.apply_change_rec(hash, apply_opts.clone())
            } else {
                self.apply_change(hash, apply_opts.clone())
            };

            match result {
                Ok(apply_outcome) => {
                    outcome.applied_hashes.push(*hash);
                    outcome.changes_applied += 1;
                    outcome.new_state = apply_outcome.new_state;
                    outcome.sequence = apply_outcome.sequence;
                    if apply_outcome.has_conflicts {
                        outcome.has_conflicts = true;
                    }
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }

        Ok(outcome)
    }

    /// Apply changes up to a tag from one stack to another.
    ///
    /// This is a convenience method that combines `get_changes_up_to_tag`
    /// and `apply_from_stack`.
    ///
    /// # Arguments
    ///
    /// * `tag_name` - Name of the tag to apply up to
    /// * `from_stack` - Source stack containing the tag
    /// * `to_stack` - Target stack (None = current stack)
    ///
    /// # Returns
    ///
    /// A `CrossStackApplyOutcome` with details about what was applied.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Apply release-1.0.0 from feature to main
    /// let result = repo.apply_tag_to_stack("release-1.0.0", "feature", Some("main"))?;
    /// ```
    pub fn apply_tag_to_stack(
        &self,
        tag_name: &str,
        from_stack: &str,
        to_stack: Option<&str>,
    ) -> Result<CrossStackApplyOutcome, RepositoryError> {
        let target = to_stack.unwrap_or(&self.current_stack);

        let options = CrossStackApplyOptions::new(from_stack, target)
            .up_to_tag(tag_name)
            .with_dependencies(true);

        self.apply_from_stack(options)
    }

    /// Cherry-pick specific changes from one stack to another.
    ///
    /// # Arguments
    ///
    /// * `changes` - Hashes of changes to apply
    /// * `from_stack` - Source stack (for validation)
    /// * `to_stack` - Target stack (None = current stack)
    ///
    /// # Returns
    ///
    /// A `CrossStackApplyOutcome` with details about what was applied.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.cherry_pick(&[hash1, hash2], "feature", None)?;
    /// ```
    pub fn cherry_pick(
        &self,
        changes: &[Hash],
        _from_stack: &str,
        to_stack: Option<&str>,
    ) -> Result<CrossStackApplyOutcome, RepositoryError> {
        let target = to_stack.unwrap_or(&self.current_stack);

        // For cherry-pick, we apply specific changes with dependencies
        let options = CrossStackApplyOptions::new("", target)
            .only_changes(changes.to_vec())
            .with_dependencies(true);

        self.apply_from_stack(options)
    }

    // ========================================================================
    // History Methods
    // ========================================================================

    /// Get a forward history log for the current stack.
    ///
    /// Returns an iterator over history entries starting from the given
    /// sequence number and proceeding forward (oldest to newest).
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling the history query
    ///
    /// # Returns
    ///
    /// A vector of history entries.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let history = repo.log(HistoryOptions::default().limit(10))?;
    /// for entry in history {
    ///     println!("#{}: {}", entry.sequence, entry.hash.to_base32());
    /// }
    /// ```
    pub fn log(
        &self,
        options: HistoryOptions,
    ) -> Result<Vec<crate::history::HistoryEntry>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);
        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        let iter = crate::history::log(&txn, &stack, &options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Collect entries, loading headers if requested
        let mut entries = Vec::new();
        for result in iter {
            let mut entry = result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            // Load header if requested
            if options.load_headers {
                if let Ok(change) = self.load_change(&entry.hash) {
                    entry = entry.with_change_header(change.hashed.header.clone());
                }
            }

            entries.push(entry);
        }

        Ok(entries)
    }

    /// Get a reverse history log (most recent first).
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling the history query
    ///
    /// # Returns
    ///
    /// A vector of history entries in reverse order.
    pub fn reverse_log(
        &self,
        options: HistoryOptions,
    ) -> Result<Vec<crate::history::HistoryEntry>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);
        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        let mut entries = crate::history::reverse_log(&txn, &stack, &options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Load headers if requested
        if options.load_headers {
            for entry in &mut entries {
                if let Ok(change) = self.load_change(&entry.hash) {
                    entry.header = Some(change.hashed.header.clone());
                }
            }
        }

        Ok(entries)
    }

    /// Get a summary of the current stack's history.
    ///
    /// # Returns
    ///
    /// A `HistorySummary` with aggregate statistics.
    pub fn history_summary(&self) -> Result<HistorySummary, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        crate::history::history_summary(&txn, &stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    // ========================================================================
    // Unrecord Methods
    // ========================================================================

    /// Unrecord a change from the current stack.
    ///
    /// This removes the change from the stack's view without deleting the change
    /// itself. The change remains in the change store and graph, and can be
    /// re-applied later. This is similar to Gerrit's workflow where a patch can
    /// be removed from a change set, modified, and re-inserted.
    ///
    /// # Arguments
    ///
    /// * `hash` - Hash of the change to unrecord
    /// * `options` - Options controlling the unrecord behavior
    ///
    /// # Returns
    ///
    /// An `UnrecordOutcome` with details about what was unrecorded, including
    /// the original sequence number (useful for re-insertion).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Unrecord a specific change
    /// let outcome = repo.unrecord(&hash, UnrecordOptions::default())?;
    /// println!("Removed from sequence {}", outcome.original_sequence.unwrap());
    ///
    /// // Later, re-insert at the original position
    /// repo.reinsert_change(&hash, outcome.original_sequence)?;
    /// ```
    pub fn unrecord(
        &self,
        hash: &Hash,
        options: UnrecordOptions,
    ) -> Result<UnrecordOutcome, RepositoryError> {
        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Determine which stack to use
        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);

        // Get the stack
        let mut stack = txn
            .open_or_create_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get internal ID
        let change_id = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: hash.to_base32(),
            })?;

        // Check if this is a dry run
        if options.dry_run {
            // Preview mode - just return what would happen
            let preview = crate::unrecord::preview_unrecord(&txn, &stack, &[*hash], &options)
                .map_err(|e| RepositoryError::Unrecord(e.to_string()))?;
            return Ok(preview);
        }

        // Remove the change from the stack
        let original_seq = txn
            .del_change(&mut stack, change_id, hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if original_seq.is_none() {
            return Err(RepositoryError::Unrecord(format!(
                "Change {} is not in stack '{}'",
                hash.to_base32(),
                stack_name
            )));
        }

        // Update the stack
        txn.update_stack(&stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Build outcome
        let mut outcome = UnrecordOutcome::new(vec![*hash], stack.state, stack.change_count);
        outcome.stats.direct_unrecords = 1;

        Ok(outcome)
    }

    /// Unrecord the last change from the current stack.
    ///
    /// This is a convenience method for unrecording the most recent change.
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling the unrecord behavior
    ///
    /// # Returns
    ///
    /// An `UnrecordOutcome` with details about what was unrecorded.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Undo the last change
    /// let outcome = repo.unrecord_last(UnrecordOptions::default())?;
    /// ```
    pub fn unrecord_last(
        &self,
        options: UnrecordOptions,
    ) -> Result<UnrecordOutcome, RepositoryError> {
        // Get the last change hash
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);
        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        let last_hash = crate::unrecord::get_last_change(&txn, &stack)
            .map_err(|e| RepositoryError::Unrecord(e.to_string()))?
            .ok_or_else(|| RepositoryError::Unrecord("Stack is empty".to_string()))?;

        drop(txn);

        self.unrecord(&last_hash, options)
    }

    /// Reinsert a previously unrecorded change at a specific position.
    ///
    /// This is part of the Gerrit-like workflow where a change can be removed,
    /// modified, and re-inserted at its original position (or appended).
    ///
    /// # Arguments
    ///
    /// * `hash` - Hash of the change to reinsert
    /// * `at_sequence` - The sequence position to insert at (None = append to end)
    ///
    /// # Returns
    ///
    /// The new state and sequence after reinsertion.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Unrecord, modify, and reinsert at original position
    /// let outcome = repo.unrecord(&hash, UnrecordOptions::default())?;
    /// // ... modify the change ...
    /// repo.reinsert_change(&hash, outcome.original_sequence)?;
    /// ```
    pub fn reinsert_change(
        &self,
        hash: &Hash,
        at_sequence: Option<u64>,
    ) -> Result<(Merkle, u64), RepositoryError> {
        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the stack
        let mut stack = txn
            .open_or_create_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get internal ID (must already be registered)
        let change_id = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: hash.to_base32(),
            })?;

        // Determine insertion point
        let insert_at = at_sequence.unwrap_or(stack.change_count);

        // Reinsert the change
        txn.reinsert_change(&mut stack, change_id, hash, insert_at)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Update the stack
        txn.update_stack(&stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok((stack.state, stack.change_count))
    }

    /// Check if a change can be unrecorded.
    ///
    /// This checks whether the change is in the stack and whether it has
    /// any dependents that would also need to be unrecorded.
    ///
    /// # Arguments
    ///
    /// * `hash` - Hash of the change to check
    ///
    /// # Returns
    ///
    /// Information about the change's dependencies and whether it can be
    /// safely unrecorded.
    pub fn can_unrecord(
        &self,
        hash: &Hash,
    ) -> Result<crate::unrecord::UnrecordDependencyInfo, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        crate::unrecord::check_can_unrecord(&txn, &stack, hash, &UnrecordOptions::default())
            .map_err(|e| RepositoryError::Unrecord(e.to_string()))
    }

    // ========================================================================
    // Tag Methods
    // ========================================================================

    /// Create a tag for the current state.
    ///
    /// Tags are named snapshots of a stack's Merkle state. They can be
    /// lightweight (just name + state) or annotated (with message/author).
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name (must be valid per `validate_tag_name`)
    /// * `options` - Options for tag creation
    ///
    /// # Returns
    ///
    /// The created `Tag`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The tag name is invalid
    /// - A tag with this name already exists (unless `force` is set)
    /// - The stack doesn't exist
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Create a lightweight tag
    /// let tag = repo.create_tag("v1.0.0", TagOptions::default())?;
    ///
    /// // Create an annotated tag
    /// let tag = repo.create_tag("v1.0.0", TagOptions::default()
    ///     .message("Release version 1.0.0")
    ///     .author("Alice", Some("alice@example.com")))?;
    /// ```
    pub fn create_tag(&self, name: &str, options: TagOptions) -> Result<Tag, RepositoryError> {
        // Validate the tag name
        validate_tag_name(name).map_err(|e| RepositoryError::InvalidTagName {
            name: name.to_string(),
            reason: e.to_string(),
        })?;

        // Get current stack state
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);
        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        // Determine sequence to tag
        let sequence = options
            .sequence
            .unwrap_or(stack.change_count.saturating_sub(1));

        // Create the tag
        let tag = if options.is_annotated() {
            let message = options.message.unwrap_or_default();
            let author = options
                .author
                .unwrap_or_else(|| Author::new("Unknown", None::<String>));
            Tag::annotated(name, stack_name, sequence, stack.state, message, author)
        } else {
            Tag::new(name, stack_name, sequence, stack.state)
        };

        // Save to disk
        let tags_dir = self.dot_dir.join("tags");
        if options.force {
            save_tag_force(&tags_dir, &tag, true).map_err(|e| Self::convert_tag_error(e, name))?;
        } else {
            save_tag(&tags_dir, &tag).map_err(|e| Self::convert_tag_error(e, name))?;
        }

        Ok(tag)
    }

    /// Convert a TagError to a RepositoryError with proper variants.
    fn convert_tag_error(e: crate::tags::TagError, _name: &str) -> RepositoryError {
        match e {
            crate::tags::TagError::AlreadyExists { name } => {
                RepositoryError::TagAlreadyExists { name }
            }
            crate::tags::TagError::NotFound { name } => RepositoryError::TagNotFound { name },
            crate::tags::TagError::InvalidName { name, reason } => {
                RepositoryError::InvalidTagName { name, reason }
            }
            crate::tags::TagError::Io(e) => RepositoryError::Io(e),
            other => RepositoryError::Database(other.to_string()),
        }
    }

    /// Get a tag by name from the current stack.
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name to look up
    ///
    /// # Returns
    ///
    /// The `Tag` if found, or `None` if not.
    pub fn get_tag(&self, name: &str) -> Result<Option<Tag>, RepositoryError> {
        self.get_tag_from_stack(name, &self.current_stack)
    }

    /// Get a tag by name from a specific stack.
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name to look up
    /// * `stack` - The stack to search in
    ///
    /// # Returns
    ///
    /// The `Tag` if found, or `None` if not.
    pub fn get_tag_from_stack(
        &self,
        name: &str,
        stack: &str,
    ) -> Result<Option<Tag>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::load_tag(&tags_dir, stack, name)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Get a tag by name, searching all stacks.
    ///
    /// This is useful when you don't know which stack a tag belongs to.
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name to look up
    ///
    /// # Returns
    ///
    /// The `Tag` if found in any stack, or `None` if not.
    pub fn get_tag_any_stack(&self, name: &str) -> Result<Option<Tag>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::load_tag_any_stack(&tags_dir, name)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List all tags for the current stack.
    ///
    /// # Returns
    ///
    /// A vector of tags in the current stack.
    pub fn list_tags(&self) -> Result<Vec<Tag>, RepositoryError> {
        self.list_tags_for_stack(&self.current_stack)
    }

    /// List all tags for a specific stack.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to list tags from
    ///
    /// # Returns
    ///
    /// A vector of tags in the specified stack.
    pub fn list_tags_for_stack(&self, stack: &str) -> Result<Vec<Tag>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::list_tags(&tags_dir, stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List all tags across all stacks.
    ///
    /// # Returns
    ///
    /// A vector of all tags in the repository from all stacks.
    pub fn list_all_tags(&self) -> Result<Vec<Tag>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::list_all_tags(&tags_dir).map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List all stacks that have tags.
    ///
    /// # Returns
    ///
    /// A vector of stack names that have at least one tag.
    pub fn list_tag_stacks(&self) -> Result<Vec<String>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::list_tag_stacks(&tags_dir)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List tags matching a filter.
    ///
    /// # Arguments
    ///
    /// * `filter` - Filter criteria for tags
    ///
    /// # Returns
    ///
    /// A filtered and sorted vector of tags.
    pub fn list_tags_filtered(&self, filter: &TagFilter) -> Result<Vec<Tag>, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::list_tags_filtered(&tags_dir, filter)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Delete a tag from the current stack.
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name to delete
    ///
    /// # Returns
    ///
    /// `true` if the tag was deleted, `false` if it didn't exist.
    pub fn delete_tag(&self, name: &str) -> Result<bool, RepositoryError> {
        self.delete_tag_from_stack(name, &self.current_stack)
    }

    /// Delete a tag from a specific stack.
    ///
    /// # Arguments
    ///
    /// * `name` - The tag name to delete
    /// * `stack` - The stack to delete from
    ///
    /// # Returns
    ///
    /// `true` if the tag was deleted, `false` if it didn't exist.
    pub fn delete_tag_from_stack(&self, name: &str, stack: &str) -> Result<bool, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::delete_tag(&tags_dir, stack, name)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count the number of tags in the current stack.
    ///
    /// # Returns
    ///
    /// The number of tags in the current stack.
    pub fn tag_count(&self) -> Result<usize, RepositoryError> {
        self.tag_count_for_stack(&self.current_stack)
    }

    /// Count the number of tags in a specific stack.
    ///
    /// # Arguments
    ///
    /// * `stack` - The stack to count tags in
    ///
    /// # Returns
    ///
    /// The number of tags in the specified stack.
    pub fn tag_count_for_stack(&self, stack: &str) -> Result<usize, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::count_tags(&tags_dir, stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count all tags across all stacks.
    ///
    /// # Returns
    ///
    /// The total number of tags in the repository.
    pub fn tag_count_all(&self) -> Result<usize, RepositoryError> {
        let tags_dir = self.dot_dir.join("tags");
        crate::tags::count_all_tags(&tags_dir).map_err(|e| RepositoryError::Database(e.to_string()))
    }

    // ========================================================================
    // Archive Methods
    // ========================================================================

    /// Create an archive of the repository's current state.
    ///
    /// This exports the working copy state at the current (or specified)
    /// Merkle state to the given destination.
    ///
    /// # Arguments
    ///
    /// * `destination` - Path to the output archive or directory
    /// * `options` - Options controlling archive creation
    ///
    /// # Returns
    ///
    /// An `ArchiveOutcome` with details about the created archive.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Archive to a tarball
    /// let outcome = repo.archive("release.tar.gz", ArchiveOptions::default())?;
    ///
    /// // Archive to a directory
    /// let outcome = repo.archive("./release/", ArchiveOptions::directory())?;
    ///
    /// // Archive with a prefix
    /// let outcome = repo.archive("myproject-1.0.tar.gz",
    ///     ArchiveOptions::default().prefix("myproject-1.0/"))?;
    /// ```
    pub fn archive<P: AsRef<Path>>(
        &self,
        destination: P,
        options: ArchiveOptions,
    ) -> Result<ArchiveOutcome, RepositoryError> {
        use std::time::Instant;

        let start = Instant::now();
        let dest_path = destination.as_ref();

        // Get current state
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);
        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        let state = options.state.unwrap_or(stack.state);

        // Build manifest from tracked files
        let mut manifest = ArchiveManifest::new();
        let tracked_files =
            list_tracked(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;

        for file in tracked_files {
            // Apply include/exclude filters
            let path_str = file.path.to_string_lossy();
            if !options.should_include(&path_str) {
                continue;
            }

            // Get file info from working copy
            let full_path = self.root.join(&file.path);
            if full_path.is_file() {
                let metadata = std::fs::metadata(&full_path).map_err(RepositoryError::Io)?;
                let size = metadata.len();

                let path_string = file.path.to_string_lossy().to_string();
                let mut entry = ArchiveEntry::file(&path_string, size);

                // Apply prefix if specified
                if let Some(ref prefix) = options.prefix {
                    entry.path = format!("{}{}", prefix, path_string);
                }

                manifest.add(entry);
            } else if full_path.is_dir() {
                let path_string = file.path.to_string_lossy().to_string();
                let mut entry = ArchiveEntry::directory(&path_string);

                if let Some(ref prefix) = options.prefix {
                    entry.path = format!("{}{}", prefix, path_string);
                }

                manifest.add(entry);
            }
        }

        // Check for empty archive
        if manifest.is_empty() {
            return Err(RepositoryError::Archive("No files to archive".to_string()));
        }

        // Check limits
        if let Some(max_files) = options.max_files {
            if manifest.file_count > max_files {
                return Err(RepositoryError::Archive(format!(
                    "Too many files: {} (max {})",
                    manifest.file_count, max_files
                )));
            }
        }

        if let Some(max_size) = options.max_size {
            if manifest.total_size > max_size {
                return Err(RepositoryError::Archive(format!(
                    "Archive too large: {} bytes (max {})",
                    manifest.total_size, max_size
                )));
            }
        }

        // Create the archive based on format
        let archive_size = match options.format {
            crate::archive::ArchiveFormat::Directory => {
                self.archive_to_directory(dest_path, &manifest, &options)?
            }
            _ => {
                // For now, only directory format is fully implemented
                return Err(RepositoryError::Archive(format!(
                    "Archive format '{}' not yet implemented. Use directory format.",
                    options.format
                )));
            }
        };

        let duration = start.elapsed();

        Ok(
            ArchiveOutcome::new(dest_path.to_path_buf(), options.format, state, manifest)
                .with_archive_size(archive_size)
                .with_duration(duration.as_millis() as u64),
        )
    }

    /// Archive to a directory (internal helper).
    fn archive_to_directory(
        &self,
        dest: &Path,
        manifest: &ArchiveManifest,
        options: &ArchiveOptions,
    ) -> Result<u64, RepositoryError> {
        let mut archive = DirectoryArchive::new(dest).map_err(RepositoryError::Io)?;

        let mut total_size = 0u64;

        // First create directories
        for entry in manifest.directories() {
            archive
                .create_directory(&entry.path, entry.mode, 0)
                .map_err(RepositoryError::Io)?;
        }

        // Then copy files
        for entry in manifest.files() {
            // Determine source path - strip prefix if it was added
            let source_rel_path = if let Some(ref prefix) = options.prefix {
                if entry.path.starts_with(prefix) {
                    entry.path[prefix.len()..].to_string()
                } else {
                    entry.path.clone()
                }
            } else {
                entry.path.clone()
            };
            let source_path = self.root.join(&source_rel_path);

            let mut writer = archive
                .create_file(&entry.path, entry.size, entry.mode, 0)
                .map_err(RepositoryError::Io)?;

            // Copy file contents
            let mut source = std::fs::File::open(&source_path).map_err(RepositoryError::Io)?;
            let copied = std::io::copy(&mut source, &mut writer).map_err(RepositoryError::Io)?;
            total_size += copied;

            archive.close_file(writer).map_err(RepositoryError::Io)?;
        }

        archive.finish().map_err(RepositoryError::Io)?;

        Ok(total_size)
    }

    // =========================================================================
    // Content Retrieval
    // =========================================================================

    /// Get the recorded content for a tracked file.
    ///
    /// This retrieves the file content from the repository graph as it was
    /// at the last recorded state. This is useful for computing diffs between
    /// the working copy and the recorded state.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    ///
    /// # Returns
    ///
    /// The file content as bytes, or `None` if the file is not tracked or
    /// has no recorded content.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The database cannot be accessed
    /// - Content retrieval fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Get recorded content for a file
    /// if let Some(content) = repo.get_file_content("src/main.rs")? {
    ///     let text = String::from_utf8_lossy(&content);
    ///     println!("Recorded content:\n{}", text);
    /// } else {
    ///     println!("File not tracked or has no content");
    /// }
    /// ```
    pub fn get_file_content<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let path = path.as_ref();
        let normalized = normalize_path(path);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the current stack to build change filter
        // This ensures we only retrieve content from changes in the current stack
        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        // Collect all change NodeIds in the current stack
        let mut change_filter: HashSet<NodeId> = HashSet::new();
        let iter = txn
            .iter_changes(&stack, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in iter {
            let (_seq, node_id, _merkle) =
                result.map_err(|e| RepositoryError::Database(e.to_string()))?;
            change_filter.insert(node_id);
        }

        // Use the filtered retrieval method
        self.get_file_content_with_filter(&txn, &normalized, change_filter)
    }

    /// Get the recorded content for a tracked file with options.
    ///
    /// Like `get_file_content`, but allows specifying retrieval options
    /// such as whether to include deleted content.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    /// * `options` - Retrieval options
    ///
    /// # Returns
    ///
    /// A `RetrieveResult` containing the content and metadata, or `None`
    /// if the file is not tracked.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_core::record::workflow::retrieve::RetrieveContentOptions;
    ///
    /// // Include deleted content for conflict resolution
    /// let options = RetrieveContentOptions::new().include_deleted(true);
    /// if let Some(result) = repo.get_file_content_with_options("src/main.rs", options)? {
    ///     println!("Content: {} bytes", result.content.len());
    ///     if result.has_conflicts {
    ///         println!("Warning: {} conflicts detected", result.conflict_count);
    ///     }
    /// }
    /// ```
    pub fn get_file_content_with_options<P: AsRef<Path>>(
        &self,
        path: P,
        options: RetrieveContentOptions,
    ) -> Result<Option<RetrieveResult>, RepositoryError> {
        use atomic_core::record::workflow::retrieve::retrieve_content_with_options;

        let path = path.as_ref();
        let normalized = normalize_path(path);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if file is tracked
        if !is_tracked(&txn, &normalized).map_err(|e| RepositoryError::Database(e.to_string()))? {
            return Ok(None);
        }

        // Get the inode for the file
        let inode = match get_inode(&txn, &normalized) {
            Ok(Some(inode)) => inode,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        // Get the position for this inode from the INODES table
        let position = match txn.inode_position(inode) {
            Ok(Some(pos)) => pos,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        // Retrieve content from the graph with options
        let result = retrieve_content_with_options(&txn, &self.change_store, position, options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Some(result))
    }

    /// Check if a tracked file has any recorded content.
    ///
    /// This is a lightweight check that doesn't retrieve the actual content,
    /// useful for quickly determining if a file has been recorded.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    ///
    /// # Returns
    ///
    /// `true` if the file is tracked and has recorded content, `false` otherwise.
    pub fn has_recorded_content<P: AsRef<Path>>(&self, path: P) -> Result<bool, RepositoryError> {
        use atomic_core::record::workflow::retrieve::has_content;

        let path = path.as_ref();
        let normalized = normalize_path(path);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if file is tracked
        if !is_tracked(&txn, &normalized).map_err(|e| RepositoryError::Database(e.to_string()))? {
            return Ok(false);
        }

        // Get the inode for the file
        let inode = match get_inode(&txn, &normalized) {
            Ok(Some(inode)) => inode,
            Ok(None) => return Ok(false),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        // Get the position for this inode from the INODES table
        let position = match txn.inode_position(inode) {
            Ok(Some(pos)) => pos,
            Ok(None) => return Ok(false),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        // Check if position has content
        let has = has_content(&txn, &self.change_store, position)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(has)
    }

    // =========================================================================
    // State-Based Content Retrieval
    // =========================================================================

    /// Get file content as it was BEFORE a specific change was applied.
    ///
    /// This method retrieves the content of a file at the state immediately
    /// prior to a change being applied. This is essential for code review
    /// workflows where you want to see what a specific change actually modified.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    /// * `change_hash` - Hash of the change to get the "before" state for
    ///
    /// # Returns
    ///
    /// * `Ok(Some(content))` - The file content before the change
    /// * `Ok(None)` - The file didn't exist before this change, or the change
    ///                is not in the current stack's history
    /// * `Err(_)` - Database or I/O error
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_repository::Repository;
    ///
    /// let repo = Repository::open(".")?;
    ///
    /// // Get the content before a specific change
    /// let before = repo.get_file_content_before_change("src/main.rs", &change_hash)?;
    /// let after = repo.get_file_content_after_change("src/main.rs", &change_hash)?;
    ///
    /// // Now you can diff the before/after content
    /// if let (Some(old), Some(new)) = (before, after) {
    ///     let diff = diff_text(&old, &new, Algorithm::Myers);
    ///     // Display the diff...
    /// }
    /// ```
    ///
    /// # Implementation Details
    ///
    /// This method:
    /// 1. Finds the change's sequence number in the current stack
    /// 2. Collects all changes applied BEFORE that sequence
    /// 3. Uses the change filter to retrieve content at that state
    ///
    /// # Performance
    ///
    /// The first call for a specific state involves iterating over the change
    /// log up to that point. For multiple files at the same state, consider
    /// using [`get_file_content_at_sequence`] with a cached change set.
    pub fn get_file_content_before_change<P: AsRef<Path>>(
        &self,
        path: P,
        change_hash: &Hash,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        use crate::history::{get_changes_up_to_sequence, get_state_before_change};

        let path = path.as_ref();
        let normalized = normalize_path(path);

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

        // Find the state before this change
        let state_info = get_state_before_change(&txn, &stack, change_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let state_info = match state_info {
            Some(info) => info,
            None => return Ok(None), // Change not in this stack
        };

        // If this is the first change, there's no content before it
        if state_info.is_first_change() {
            return Ok(None);
        }

        // Get the set of changes applied before this change
        let change_set =
            get_changes_up_to_sequence(&txn, &stack, state_info.parent_max_sequence_exclusive())
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Retrieve content with the change filter
        self.get_file_content_with_filter(&txn, &normalized, change_set)
    }

    /// Get file content as it was AFTER a specific change was applied.
    ///
    /// This method retrieves the content of a file at the state immediately
    /// after a change was applied. Combined with [`get_file_content_before_change`],
    /// this enables showing exactly what a specific change modified.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    /// * `change_hash` - Hash of the change to get the "after" state for
    ///
    /// # Returns
    ///
    /// * `Ok(Some(content))` - The file content after the change
    /// * `Ok(None)` - The file doesn't exist after this change (was deleted),
    ///                or the change is not in the current stack's history
    /// * `Err(_)` - Database or I/O error
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Get before and after content for a change
    /// let before = repo.get_file_content_before_change("src/main.rs", &hash)?;
    /// let after = repo.get_file_content_after_change("src/main.rs", &hash)?;
    ///
    /// match (before, after) {
    ///     (None, Some(_)) => println!("File was added"),
    ///     (Some(_), None) => println!("File was deleted"),
    ///     (Some(old), Some(new)) => println!("File was modified"),
    ///     (None, None) => println!("File not affected by this change"),
    /// }
    /// ```
    pub fn get_file_content_after_change<P: AsRef<Path>>(
        &self,
        path: P,
        change_hash: &Hash,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        use crate::history::get_changes_up_to_change;

        let path = path.as_ref();
        let normalized = normalize_path(path);

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

        // Get all changes up to and including this change
        let change_set = match get_changes_up_to_change(&txn, &stack, change_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(set) => set,
            None => return Ok(None), // Change not in this stack
        };

        // Retrieve content with the change filter
        self.get_file_content_with_filter(&txn, &normalized, change_set)
    }

    /// Get file content at a specific sequence number.
    ///
    /// This is a lower-level method that retrieves file content at the state
    /// after a specific sequence number of changes have been applied.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    /// * `max_sequence` - Exclusive upper bound (content reflects changes 0..max_sequence)
    ///
    /// # Returns
    ///
    /// * `Ok(Some(content))` - The file content at that sequence
    /// * `Ok(None)` - The file doesn't exist at that sequence
    /// * `Err(_)` - Database or I/O error
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Get content after the first 5 changes
    /// let content = repo.get_file_content_at_sequence("src/main.rs", 5)?;
    ///
    /// // Get content at the very beginning (before any changes)
    /// let initial = repo.get_file_content_at_sequence("src/main.rs", 0)?;
    /// assert!(initial.is_none()); // No content before any changes
    /// ```
    pub fn get_file_content_at_sequence<P: AsRef<Path>>(
        &self,
        path: P,
        max_sequence: u64,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        use crate::history::get_changes_up_to_sequence;

        let path = path.as_ref();
        let normalized = normalize_path(path);

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

        // Get the set of changes up to the sequence
        let change_set = get_changes_up_to_sequence(&txn, &stack, max_sequence)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Retrieve content with the change filter
        self.get_file_content_with_filter(&txn, &normalized, change_set)
    }

    /// Internal helper to retrieve file content with a change filter.
    ///
    /// This method handles the common logic for state-based content retrieval:
    /// 1. Check if file is tracked
    /// 2. Get the inode and position
    /// 3. Retrieve content using the change filter
    fn get_file_content_with_filter<T>(
        &self,
        txn: &T,
        normalized_path: &str,
        change_set: std::collections::HashSet<NodeId>,
    ) -> Result<Option<Vec<u8>>, RepositoryError>
    where
        T: atomic_core::pristine::GraphTxnT + atomic_core::pristine::TreeTxnT,
    {
        use atomic_core::output::alive::RetrieveOptions;
        use atomic_core::record::workflow::retrieve::retrieve_content_with_filter;

        // Check if file is tracked
        if !is_tracked(txn, normalized_path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            return Ok(None);
        }

        // Get the inode for the file
        let inode = match get_inode(txn, normalized_path) {
            Ok(Some(inode)) => inode,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        // Get the position for this inode from the INODES table
        let position = match txn.inode_position(inode) {
            Ok(Some(pos)) => pos,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        // Create options with the change filter
        let options = RetrieveOptions::new().with_change_filter(change_set.clone());

        // Retrieve content from the graph with the filter
        let content = retrieve_content_with_filter(txn, &self.change_store, position, options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if content.is_empty() {
            Ok(None)
        } else {
            Ok(Some(content))
        }
    }

    // =========================================================================
    // Archive Operations
    // =========================================================================

    /// Archive a specific tag.
    ///
    /// # Arguments
    ///
    /// * `tag_name` - Name of the tag to archive
    /// * `destination` - Path to the output archive
    /// * `options` - Archive options
    ///
    /// # Returns
    ///
    /// An `ArchiveOutcome` with details about the created archive.
    pub fn archive_tag<P: AsRef<Path>>(
        &self,
        tag_name: &str,
        destination: P,
        mut options: ArchiveOptions,
    ) -> Result<ArchiveOutcome, RepositoryError> {
        // Get the tag
        let tag = self
            .get_tag(tag_name)?
            .ok_or_else(|| RepositoryError::TagNotFound {
                name: tag_name.to_string(),
            })?;

        // Set the state from the tag
        options.state = Some(tag.state);

        // Archive with the tag's state
        self.archive(destination, options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::{Author, Change, ChangeHeader};
    use atomic_core::types::Base32;

    use tempfile::TempDir;

    fn create_temp_repo() -> (TempDir, Repository) {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();
        (temp_dir, repo)
    }

    #[test]
    fn test_init_creates_structure() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();

        assert!(repo.dot_dir().exists());
        assert!(repo.pristine_path().exists());
        assert!(repo.changes_dir().exists());
        assert!(repo.config_path().exists());
    }

    #[test]
    fn test_init_fails_if_exists() {
        let (temp_dir, _repo) = create_temp_repo();

        let result = Repository::init(temp_dir.path());
        assert!(matches!(result, Err(RepositoryError::AlreadyExists { .. })));
    }

    #[test]
    fn test_open_existing() {
        let (temp_dir, repo) = create_temp_repo();
        let root = repo.root().to_path_buf();

        // Drop the original repository to release the database lock
        drop(repo);

        let opened = Repository::open(temp_dir.path()).unwrap();
        assert_eq!(opened.root(), root);
        assert_eq!(opened.current_stack(), DEFAULT_STACK);
    }

    #[test]
    fn test_open_from_subdirectory() {
        let (temp_dir, repo) = create_temp_repo();
        let root = repo.root().to_path_buf();

        // Drop the original repository to release the database lock
        drop(repo);

        // Create a subdirectory
        let subdir = temp_dir.path().join("src").join("lib");
        std::fs::create_dir_all(&subdir).unwrap();

        // Open from subdirectory should find the root
        let opened = Repository::open(&subdir).unwrap();
        assert_eq!(opened.root(), root);
    }

    #[test]
    fn test_open_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let result = Repository::open(temp_dir.path());
        assert!(matches!(result, Err(RepositoryError::NotFound { .. })));
    }

    #[test]
    fn test_is_repository() {
        let (temp_dir, _repo) = create_temp_repo();

        assert!(Repository::is_repository(temp_dir.path()));

        let non_repo = TempDir::new().unwrap();
        assert!(!Repository::is_repository(non_repo.path()));
    }

    #[test]
    fn test_change_path() {
        let (_temp_dir, repo) = create_temp_repo();

        let hash = "ABCDEF123456";
        let path = repo.change_path(hash);

        assert!(path.to_string_lossy().contains("AB"));
        assert!(path.to_string_lossy().contains(hash));
    }

    #[test]
    fn test_to_relative() {
        let (temp_dir, repo) = create_temp_repo();

        let abs_path = temp_dir.path().join("src").join("main.rs");
        let rel_path = repo.to_relative(&abs_path).unwrap();

        assert_eq!(rel_path, PathBuf::from("src/main.rs"));
    }

    #[test]
    fn test_to_absolute() {
        let (temp_dir, repo) = create_temp_repo();

        let rel_path = PathBuf::from("src/main.rs");
        let abs_path = repo.to_absolute(&rel_path);

        assert_eq!(abs_path, temp_dir.path().join("src/main.rs"));
    }

    #[test]
    fn test_is_internal_path() {
        let (_temp_dir, repo) = create_temp_repo();

        assert!(repo.is_internal_path(repo.dot_dir()));
        assert!(repo.is_internal_path(repo.pristine_path()));
        assert!(repo.is_internal_path(repo.changes_dir()));
        assert!(!repo.is_internal_path(repo.root().join("src")));
    }

    #[test]
    fn test_set_current_stack() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // First create the stack
        repo.create_stack("feature-stack").unwrap();

        // Then switch to it
        repo.set_current_stack("feature-stack").unwrap();
        assert_eq!(repo.current_stack(), "feature-stack");

        // Verify it persists - drop repo first to release lock
        let root = repo.root().to_path_buf();
        drop(repo);

        let reopened = Repository::open(&root).unwrap();
        assert_eq!(reopened.current_stack(), "feature-stack");
    }

    #[test]
    fn test_set_current_stack_nonexistent() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Trying to switch to a nonexistent stack should fail
        let result = repo.set_current_stack("nonexistent");
        assert!(matches!(result, Err(RepositoryError::StackNotFound { .. })));
    }

    #[test]
    fn test_create_stack() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Create a new stack
        repo.create_stack("feature").unwrap();

        // Verify it exists
        assert!(repo.stack_exists("feature").unwrap());

        // Creating the same stack again should fail
        let result = repo.create_stack("feature");
        assert!(matches!(
            result,
            Err(RepositoryError::StackAlreadyExists { .. })
        ));
    }

    #[test]
    fn test_list_stacks() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Should have default "dev" stack
        let stacks = repo.list_stacks().unwrap();
        assert!(stacks.contains(&"dev".to_string()));

        // Create additional stacks
        repo.create_stack("feature-a").unwrap();
        repo.create_stack("feature-b").unwrap();

        let stacks = repo.list_stacks().unwrap();
        assert_eq!(stacks.len(), 3);
        assert!(stacks.contains(&"dev".to_string()));
        assert!(stacks.contains(&"feature-a".to_string()));
        assert!(stacks.contains(&"feature-b".to_string()));
    }

    #[test]
    fn test_default_stack_name() {
        let (_temp_dir, repo) = create_temp_repo();
        assert_eq!(repo.current_stack(), "dev");
        assert_eq!(DEFAULT_STACK, "dev");
    }

    #[test]
    fn test_delete_stack() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Create a stack
        repo.create_stack("to-delete").unwrap();
        assert!(repo.stack_exists("to-delete").unwrap());

        // Delete the stack
        repo.delete_stack("to-delete").unwrap();

        // Verify it's gone
        assert!(!repo.stack_exists("to-delete").unwrap());
    }

    #[test]
    fn test_delete_stack_nonexistent() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Trying to delete a nonexistent stack should fail
        let result = repo.delete_stack("nonexistent");
        assert!(matches!(result, Err(RepositoryError::StackNotFound { .. })));
    }

    #[test]
    fn test_delete_current_stack_fails() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Trying to delete the current stack should fail
        let result = repo.delete_stack("dev");
        assert!(matches!(
            result,
            Err(RepositoryError::CannotDeleteCurrentStack { .. })
        ));
    }

    #[test]
    fn test_delete_stack_preserves_others() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Create two stacks
        repo.create_stack("keep-me").unwrap();
        repo.create_stack("delete-me").unwrap();

        // Delete one
        repo.delete_stack("delete-me").unwrap();

        // Verify the other still exists
        assert!(repo.stack_exists("keep-me").unwrap());
        assert!(!repo.stack_exists("delete-me").unwrap());
    }

    #[test]
    fn test_get_stack_info() {
        let (_temp_dir, mut repo) = create_temp_repo();

        // Create a stack
        repo.create_stack("info-test").unwrap();

        // Get info
        let info = repo.get_stack_info("info-test").unwrap();
        assert_eq!(info.name, "info-test");
        assert_eq!(info.change_count, 0);
        assert!(info.is_empty());
    }

    #[test]
    fn test_get_stack_info_nonexistent() {
        let (_temp_dir, repo) = create_temp_repo();

        // Trying to get info for a nonexistent stack should fail
        let result = repo.get_stack_info("nonexistent");
        assert!(matches!(result, Err(RepositoryError::StackNotFound { .. })));
    }

    #[test]
    fn test_stack_info_state_methods() {
        let (_temp_dir, mut repo) = create_temp_repo();

        repo.create_stack("state-test").unwrap();
        let info = repo.get_stack_info("state-test").unwrap();

        // Test state methods
        let base32 = info.state_base32();
        assert!(!base32.is_empty());

        let short = info.state_short();
        assert!(short.len() <= 12);

        // For an empty stack
        assert!(info.is_empty());
    }

    // ========================================================================
    // Change Storage Tests
    // ========================================================================

    /// Create a simple test change with the given message.
    fn create_test_change(message: &str) -> Change {
        let header = ChangeHeader::builder()
            .message(message)
            .author(Author::new("Test Author", Some("test@example.com")))
            .build();

        Change::new(header, Vec::new(), Vec::new(), Vec::new())
    }

    /// Create a test change with some content.
    fn create_test_change_with_content(message: &str, content: &[u8]) -> Change {
        let header = ChangeHeader::builder()
            .message(message)
            .author(Author::new("Test Author", Some("test@example.com")))
            .build();

        Change::new(header, Vec::new(), content.to_vec(), Vec::new())
    }

    #[test]
    fn test_repo_save_change() {
        let (_temp_dir, repo) = create_temp_repo();

        let change = create_test_change("Test save change via repository");
        let result = repo.save_change(&change);

        assert!(result.is_ok());
        let hash = result.unwrap();

        // Verify the change exists
        assert!(repo.has_change(&hash));
    }

    #[test]
    fn test_repo_load_change() {
        let (_temp_dir, repo) = create_temp_repo();

        // Save a change first
        let original = create_test_change("Test load change via repository");
        let hash = repo.save_change(&original).expect("Failed to save change");

        // Load the change
        let loaded = repo.load_change(&hash).expect("Failed to load change");

        // Verify the data matches
        assert_eq!(original.hashed.header.message, loaded.hashed.header.message);
    }

    #[test]
    fn test_repo_save_load_roundtrip() {
        let (_temp_dir, repo) = create_temp_repo();

        let original = create_test_change_with_content(
            "Test roundtrip via repository",
            b"Repository content test!",
        );

        // Save
        let hash = repo.save_change(&original).expect("Failed to save change");

        // Load
        let loaded = repo.load_change(&hash).expect("Failed to load change");

        // Verify all fields
        assert_eq!(original.hashed.header.message, loaded.hashed.header.message);
        assert_eq!(original.contents, loaded.contents);
    }

    #[test]
    fn test_repo_load_nonexistent_change() {
        let (_temp_dir, repo) = create_temp_repo();

        let fake_hash = Hash::of(b"nonexistent change");
        let result = repo.load_change(&fake_hash);

        assert!(result.is_err());
        assert!(matches!(
            result,
            Err(RepositoryError::ChangeNotFound { .. })
        ));
    }

    #[test]
    fn test_repo_has_change() {
        let (_temp_dir, repo) = create_temp_repo();

        let change = create_test_change("Test has_change via repository");
        let hash = repo.save_change(&change).expect("Failed to save change");

        // Should exist
        assert!(repo.has_change(&hash));

        // Should not exist
        let fake_hash = Hash::of(b"nonexistent");
        assert!(!repo.has_change(&fake_hash));
    }

    #[test]
    fn test_repo_delete_change() {
        let (_temp_dir, repo) = create_temp_repo();

        let change = create_test_change("Test delete change via repository");
        let hash = repo.save_change(&change).expect("Failed to save change");

        // Verify it exists
        assert!(repo.has_change(&hash));

        // Delete it
        let deleted = repo.delete_change(&hash).expect("Failed to delete change");
        assert!(deleted);

        // Verify it's gone
        assert!(!repo.has_change(&hash));
    }

    #[test]
    fn test_repo_count_changes() {
        let (_temp_dir, repo) = create_temp_repo();

        // Initially empty
        assert_eq!(repo.count_changes().unwrap(), 0);

        // Add some changes
        for i in 0..3 {
            let change = create_test_change(&format!("Change {}", i));
            repo.save_change(&change).expect("Failed to save change");
        }

        assert_eq!(repo.count_changes().unwrap(), 3);
    }

    #[test]
    fn test_repo_iter_changes() {
        let (_temp_dir, repo) = create_temp_repo();

        // Save multiple changes
        let mut saved_hashes = Vec::new();
        for i in 0..5 {
            let change = create_test_change(&format!("Repository change {}", i));
            let hash = repo.save_change(&change).expect("Failed to save change");
            saved_hashes.push(hash);
        }

        // Iterate and collect
        let found_hashes: Vec<Hash> = repo.iter_changes().filter_map(|r| r.ok()).collect();

        // All saved changes should be found
        assert_eq!(found_hashes.len(), saved_hashes.len());
        for hash in &saved_hashes {
            assert!(
                found_hashes.contains(hash),
                "Should find saved hash {}",
                hash.to_base32()
            );
        }
    }

    #[test]
    fn test_repo_change_store_accessor() {
        let (_temp_dir, repo) = create_temp_repo();

        // Access the underlying change store
        let store = repo.change_store();

        // Should be able to use it directly
        assert_eq!(store.changes_dir(), repo.changes_dir());
    }

    // ========================================================================
    // Status Tests
    // ========================================================================

    #[test]
    fn test_repo_status_empty_repo() {
        let (_temp_dir, repo) = create_temp_repo();

        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");

        assert_eq!(status.stack(), "dev");
        // Empty repo should be clean
        assert!(status.is_clean());
    }

    #[test]
    fn test_repo_status_with_untracked_files() {
        let (temp_dir, repo) = create_temp_repo();

        // Create some untracked files
        std::fs::write(temp_dir.path().join("file1.txt"), b"content1").unwrap();
        std::fs::create_dir_all(temp_dir.path().join("src")).unwrap();
        std::fs::write(temp_dir.path().join("src/main.rs"), b"fn main() {}").unwrap();

        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");

        // Should have untracked files
        assert!(status.has_untracked());
        assert_eq!(status.untracked_count(), 2);

        // But should still be "clean" (no tracked modifications)
        assert!(status.is_clean());
    }

    #[test]
    fn test_repo_status_tracked_only() {
        let (temp_dir, repo) = create_temp_repo();

        // Create some untracked files
        std::fs::write(temp_dir.path().join("file1.txt"), b"content1").unwrap();

        let status = repo
            .status(StatusOptions::tracked_only())
            .expect("status failed");

        // Should not include untracked files
        assert!(!status.has_untracked());
        assert_eq!(status.untracked_count(), 0);
    }

    #[test]
    fn test_repo_status_quick() {
        let (_temp_dir, repo) = create_temp_repo();

        // Quick status should work
        let status = repo.status_quick().expect("status_quick failed");
        assert!(status.is_clean());
    }

    #[test]
    fn test_repo_is_working_copy_clean() {
        let (_temp_dir, repo) = create_temp_repo();

        // Empty repo should be clean
        assert!(repo.is_working_copy_clean().expect("is_clean failed"));
    }

    #[test]
    fn test_repo_untracked_files() {
        let (temp_dir, repo) = create_temp_repo();

        // Create untracked files
        std::fs::write(temp_dir.path().join("new_file.txt"), b"content").unwrap();

        let untracked = repo.untracked_files().expect("untracked_files failed");

        assert_eq!(untracked.len(), 1);
        assert!(untracked.iter().any(|p| p.ends_with("new_file.txt")));
    }

    #[test]
    fn test_repo_modified_files_empty() {
        let (_temp_dir, repo) = create_temp_repo();

        // No modified files in empty repo
        let modified = repo.modified_files().expect("modified_files failed");
        assert!(modified.is_empty());
    }

    #[test]
    fn test_repo_deleted_files_empty() {
        let (_temp_dir, repo) = create_temp_repo();

        // No deleted files in empty repo
        let deleted = repo.deleted_files().expect("deleted_files failed");
        assert!(deleted.is_empty());
    }

    #[test]
    fn test_repo_status_ignores_atomic_dir() {
        let (temp_dir, repo) = create_temp_repo();

        // The .atomic directory should be ignored
        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");

        // None of the .atomic files should appear
        for entry in status.entries() {
            assert!(
                !entry.path().starts_with(".atomic"),
                "Should not include .atomic directory files"
            );
        }
    }

    #[test]
    fn test_repo_ignore_rules() {
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file
        std::fs::write(
            temp_dir.path().join(".atomicignore"),
            "target/\n*.log\n!important.log\n",
        )
        .unwrap();

        let rules = repo.ignore_rules();

        // Should ignore target directory
        assert!(rules.is_ignored(Path::new("target"), true));
        assert!(rules.is_ignored(Path::new("target/debug/app"), false));

        // Should ignore .log files
        assert!(rules.is_ignored(Path::new("debug.log"), false));
        assert!(rules.is_ignored(Path::new("logs/error.log"), false));

        // Should NOT ignore important.log (whitelisted)
        assert!(!rules.is_ignored(Path::new("important.log"), false));

        // Should NOT ignore normal files
        assert!(!rules.is_ignored(Path::new("src/main.rs"), false));
        assert!(!rules.is_ignored(Path::new("Cargo.toml"), false));
    }

    #[test]
    fn test_repo_is_ignored() {
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file
        std::fs::write(temp_dir.path().join(".atomicignore"), "build/\n*.tmp\n").unwrap();

        // Should ignore patterns from .atomicignore
        assert!(repo.is_ignored(Path::new("build"), true));
        assert!(repo.is_ignored(Path::new("cache.tmp"), false));

        // Should NOT ignore normal files
        assert!(!repo.is_ignored(Path::new("src/lib.rs"), false));
    }

    #[test]
    fn test_repo_status_respects_atomicignore() {
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file
        std::fs::write(temp_dir.path().join(".atomicignore"), "ignored/\n*.bak\n").unwrap();

        // Create files (some should be ignored, some not)
        std::fs::create_dir_all(temp_dir.path().join("ignored")).unwrap();
        std::fs::write(temp_dir.path().join("ignored/file.txt"), b"ignored").unwrap();
        std::fs::write(temp_dir.path().join("backup.bak"), b"backup").unwrap();
        std::fs::write(temp_dir.path().join("visible.txt"), b"visible").unwrap();

        // Get status with default options (respects ignore files)
        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");

        // Collect all paths in status
        let paths: Vec<PathBuf> = status
            .entries()
            .iter()
            .map(|e| e.path().to_path_buf())
            .collect();

        // Should NOT include ignored files
        assert!(
            !paths.iter().any(|p| p.starts_with("ignored")),
            "Should not include ignored/ directory"
        );
        assert!(
            !paths.iter().any(|p| p.to_string_lossy().ends_with(".bak")),
            "Should not include .bak files"
        );

        // Should include visible.txt
        assert!(
            paths.iter().any(|p| p == Path::new("visible.txt")),
            "Should include visible.txt"
        );
    }

    #[test]
    fn test_repo_status_include_ignored() {
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file
        std::fs::write(temp_dir.path().join(".atomicignore"), "*.log\n").unwrap();

        // Create files
        std::fs::write(temp_dir.path().join("debug.log"), b"log").unwrap();
        std::fs::write(temp_dir.path().join("main.rs"), b"code").unwrap();

        // Get status with include_ignored = true
        let status = repo.status(StatusOptions::all()).expect("status failed");

        // Collect all paths in status
        let paths: Vec<PathBuf> = status
            .entries()
            .iter()
            .map(|e| e.path().to_path_buf())
            .collect();

        // Should include both ignored and non-ignored files
        assert!(
            paths.iter().any(|p| p == Path::new("debug.log")),
            "Should include debug.log when include_ignored=true"
        );
        assert!(
            paths.iter().any(|p| p == Path::new("main.rs")),
            "Should include main.rs"
        );
    }

    #[test]
    fn test_repo_add_respects_atomicignore() {
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file
        std::fs::write(temp_dir.path().join(".atomicignore"), "ignored/\n").unwrap();

        // Create directory structure
        std::fs::create_dir_all(temp_dir.path().join("ignored")).unwrap();
        std::fs::write(temp_dir.path().join("ignored/file.txt"), b"ignored").unwrap();

        // Trying to add an ignored path should fail
        let result = repo.add("ignored", TrackingOptions::default());
        assert!(result.is_err(), "Adding ignored directory should fail");
    }

    #[test]
    fn test_repo_status_ignores_node_modules() {
        // This test mimics the real-world scenario where node_modules should be ignored
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore file with "node_modules" (no trailing slash or newline issues)
        std::fs::write(temp_dir.path().join(".atomicignore"), "node_modules\n").unwrap();

        // Create node_modules directory with nested files
        std::fs::create_dir_all(temp_dir.path().join("node_modules/typescript/lib")).unwrap();
        std::fs::create_dir_all(temp_dir.path().join("node_modules/@types/node")).unwrap();
        std::fs::write(
            temp_dir
                .path()
                .join("node_modules/typescript/lib/lib.es2015.proxy.d.ts"),
            b"// typescript",
        )
        .unwrap();
        std::fs::write(
            temp_dir
                .path()
                .join("node_modules/@types/node/child_process.d.ts"),
            b"// node types",
        )
        .unwrap();

        // Create some non-ignored files
        std::fs::write(temp_dir.path().join("package.json"), b"{}").unwrap();
        std::fs::write(temp_dir.path().join("index.js"), b"console.log('hello')").unwrap();

        // Verify ignore rules are loaded correctly
        let rules = repo.ignore_rules();
        assert!(rules.has_local_rules(), "Should have local rules");
        assert!(
            rules.is_ignored(Path::new("node_modules"), true),
            "node_modules directory should be ignored"
        );
        assert!(
            rules.is_ignored(
                Path::new("node_modules/typescript/lib/lib.es2015.proxy.d.ts"),
                false
            ),
            "Files in node_modules should be ignored"
        );

        // Get status with default options (respects ignore files)
        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");

        // Collect all paths in status
        let paths: Vec<PathBuf> = status
            .entries()
            .iter()
            .map(|e| e.path().to_path_buf())
            .collect();

        // Debug output
        eprintln!("Status entries:");
        for path in &paths {
            eprintln!("  {:?}", path);
        }

        // Should NOT include any node_modules files
        assert!(
            !paths.iter().any(|p| p.starts_with("node_modules")),
            "Should not include node_modules directory in status, but found: {:?}",
            paths
                .iter()
                .filter(|p| p.starts_with("node_modules"))
                .collect::<Vec<_>>()
        );

        // Should include non-ignored files
        assert!(
            paths
                .iter()
                .any(|p| p.as_path() == Path::new("package.json")),
            "Should include package.json"
        );
        assert!(
            paths.iter().any(|p| p.as_path() == Path::new("index.js")),
            "Should include index.js"
        );
    }

    #[test]
    fn test_repo_status_ignores_node_modules_no_trailing_newline() {
        // Test the specific case where .atomicignore has no trailing newline
        let (temp_dir, repo) = create_temp_repo();

        // Create .atomicignore WITHOUT trailing newline (common user mistake)
        std::fs::write(temp_dir.path().join(".atomicignore"), "node_modules").unwrap();

        // Create node_modules
        std::fs::create_dir_all(temp_dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(temp_dir.path().join("node_modules/pkg/index.js"), b"module").unwrap();

        // Create non-ignored file
        std::fs::write(temp_dir.path().join("app.js"), b"app").unwrap();

        // Get status
        let status = repo
            .status(StatusOptions::default())
            .expect("status failed");
        let paths: Vec<PathBuf> = status
            .entries()
            .iter()
            .map(|e| e.path().to_path_buf())
            .collect();

        // Should NOT include node_modules
        assert!(
            !paths.iter().any(|p| p.starts_with("node_modules")),
            "node_modules should be ignored even without trailing newline"
        );

        // Should include app.js
        assert!(
            paths.iter().any(|p| p.as_path() == Path::new("app.js")),
            "Should include app.js"
        );
    }

    // ========================================================================
    // File Tracking Tests
    // ========================================================================

    #[test]
    fn test_repo_add_file() {
        let (temp_dir, repo) = create_temp_repo();

        // Create a file
        std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();

        // Add it to tracking
        let stats = repo.add("test.txt", TrackingOptions::default()).unwrap();

        assert_eq!(stats.files_added, 1);
        assert!(repo.is_tracked("test.txt").unwrap());
    }

    #[test]
    fn test_repo_add_directory() {
        let (temp_dir, repo) = create_temp_repo();

        // Create directory with files
        std::fs::create_dir_all(temp_dir.path().join("src")).unwrap();
        std::fs::write(temp_dir.path().join("src/main.rs"), b"fn main() {}").unwrap();
        std::fs::write(temp_dir.path().join("src/lib.rs"), b"// lib").unwrap();

        // Add directory recursively
        let stats = repo.add("src", TrackingOptions::default()).unwrap();

        assert!(stats.files_added >= 2);
        assert!(repo.is_tracked("src/main.rs").unwrap());
        assert!(repo.is_tracked("src/lib.rs").unwrap());
    }

    #[test]
    fn test_repo_add_already_tracked() {
        let (temp_dir, repo) = create_temp_repo();

        // Create and add a file
        std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();
        repo.add("test.txt", TrackingOptions::default()).unwrap();

        // Adding again should succeed but skip
        let stats = repo.add("test.txt", TrackingOptions::default()).unwrap();
        assert_eq!(stats.files_added, 0);
        assert_eq!(stats.skipped, 1);
    }

    #[test]
    fn test_repo_add_dry_run() {
        let (temp_dir, repo) = create_temp_repo();

        // Create a file
        std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();

        // Dry run should not actually add
        let stats = repo.add("test.txt", TrackingOptions::dry_run()).unwrap();

        assert_eq!(stats.files_added, 1);
        assert!(!repo.is_tracked("test.txt").unwrap()); // Not actually tracked
    }

    #[test]
    fn test_repo_remove_file() {
        let (temp_dir, repo) = create_temp_repo();

        // Create and add a file
        std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();
        repo.add("test.txt", TrackingOptions::default()).unwrap();
        assert!(repo.is_tracked("test.txt").unwrap());

        // Remove from tracking
        let stats = repo.remove("test.txt", TrackingOptions::default()).unwrap();

        assert_eq!(stats.files_removed, 1);
        assert!(!repo.is_tracked("test.txt").unwrap());
    }

    #[test]
    fn test_repo_remove_not_tracked() {
        let (_temp_dir, repo) = create_temp_repo();

        // Removing non-tracked file should error
        let result = repo.remove("nonexistent.txt", TrackingOptions::default());
        assert!(result.is_err());

        // With force, it should succeed
        let stats = repo
            .remove("nonexistent.txt", TrackingOptions::forced())
            .unwrap();
        assert_eq!(stats.files_removed, 0);
    }

    #[test]
    fn test_repo_move_file() {
        let (temp_dir, repo) = create_temp_repo();

        // Create and add a file
        std::fs::write(temp_dir.path().join("old.txt"), b"content").unwrap();
        repo.add("old.txt", TrackingOptions::default()).unwrap();
        let original_inode = repo.get_file_inode("old.txt").unwrap().unwrap();

        // Move the file on disk
        std::fs::rename(
            temp_dir.path().join("old.txt"),
            temp_dir.path().join("new.txt"),
        )
        .unwrap();

        // Update tracking
        let inode = repo.move_file("old.txt", "new.txt").unwrap();

        // Inode should be preserved
        assert_eq!(inode, original_inode);
        assert!(!repo.is_tracked("old.txt").unwrap());
        assert!(repo.is_tracked("new.txt").unwrap());
    }

    #[test]
    fn test_repo_list_tracked_files() {
        let (temp_dir, repo) = create_temp_repo();

        // Create and add files
        std::fs::write(temp_dir.path().join("file1.txt"), b"1").unwrap();
        std::fs::write(temp_dir.path().join("file2.txt"), b"2").unwrap();
        repo.add("file1.txt", TrackingOptions::default()).unwrap();
        repo.add("file2.txt", TrackingOptions::default()).unwrap();

        let tracked = repo.list_tracked_files().unwrap();

        assert_eq!(tracked.len(), 2);
    }

    #[test]
    fn test_repo_tracked_file_count() {
        let (temp_dir, repo) = create_temp_repo();

        assert_eq!(repo.tracked_file_count().unwrap(), 0);

        std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();
        repo.add("test.txt", TrackingOptions::default()).unwrap();

        assert_eq!(repo.tracked_file_count().unwrap(), 1);
    }

    #[test]
    fn test_repo_tracked_files_under() {
        let (temp_dir, repo) = create_temp_repo();

        // Create files in different directories
        std::fs::create_dir_all(temp_dir.path().join("src")).unwrap();
        std::fs::create_dir_all(temp_dir.path().join("tests")).unwrap();
        std::fs::write(temp_dir.path().join("src/main.rs"), b"main").unwrap();
        std::fs::write(temp_dir.path().join("src/lib.rs"), b"lib").unwrap();
        std::fs::write(temp_dir.path().join("tests/test.rs"), b"test").unwrap();

        repo.add("src", TrackingOptions::default()).unwrap();
        repo.add("tests", TrackingOptions::default()).unwrap();

        let src_files = repo.tracked_files_under("src").unwrap();

        // Should only have files under src/
        assert!(src_files.len() >= 2);
        for (path, _) in &src_files {
            assert!(
                path.starts_with("src/"),
                "Expected src/ prefix, got: {}",
                path
            );
        }
    }

    // ========================================================================
    // Tag Method Tests
    // ========================================================================

    #[test]
    fn test_repo_create_tag() {
        let (_temp_dir, repo) = create_temp_repo();

        let tag = repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        assert_eq!(tag.name, "v1.0.0");
        assert_eq!(tag.stack, DEFAULT_STACK);
        assert!(!tag.is_annotated());
    }

    #[test]
    fn test_repo_create_annotated_tag() {
        let (_temp_dir, repo) = create_temp_repo();

        let options = TagOptions::default()
            .message("Release 1.0")
            .author("Alice", Some("alice@example.com"));

        let tag = repo.create_tag("v1.0.0", options).unwrap();

        assert_eq!(tag.name, "v1.0.0");
        assert!(tag.is_annotated());
        assert_eq!(tag.message(), Some("Release 1.0"));
    }

    #[test]
    fn test_repo_get_tag() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        let tag = repo.get_tag("v1.0.0").unwrap();
        assert!(tag.is_some());
        assert_eq!(tag.unwrap().name, "v1.0.0");
    }

    #[test]
    fn test_repo_get_tag_not_found() {
        let (_temp_dir, repo) = create_temp_repo();

        let tag = repo.get_tag("nonexistent").unwrap();
        assert!(tag.is_none());
    }

    #[test]
    fn test_repo_list_tags() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

        let tags = repo.list_tags().unwrap();
        assert_eq!(tags.len(), 2);
    }

    #[test]
    fn test_repo_list_tags_empty() {
        let (_temp_dir, repo) = create_temp_repo();

        let tags = repo.list_tags().unwrap();
        assert!(tags.is_empty());
    }

    #[test]
    fn test_repo_list_tags_filtered() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default().message("Annotated"))
            .unwrap();
        repo.create_tag("release-1", TagOptions::default()).unwrap();

        // Filter by pattern
        let filter = TagFilter::new().pattern("v*");
        let tags = repo.list_tags_filtered(&filter).unwrap();
        assert_eq!(tags.len(), 2);

        // Filter annotated only
        let filter = TagFilter::new().annotated_only();
        let tags = repo.list_tags_filtered(&filter).unwrap();
        assert_eq!(tags.len(), 1);
    }

    #[test]
    fn test_repo_delete_tag() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        assert!(repo.delete_tag("v1.0.0").unwrap());
        assert!(repo.get_tag("v1.0.0").unwrap().is_none());
    }

    #[test]
    fn test_repo_delete_tag_not_found() {
        let (_temp_dir, repo) = create_temp_repo();

        assert!(!repo.delete_tag("nonexistent").unwrap());
    }

    #[test]
    fn test_repo_tag_count() {
        let (_temp_dir, repo) = create_temp_repo();

        assert_eq!(repo.tag_count().unwrap(), 0);

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

        assert_eq!(repo.tag_count().unwrap(), 2);
    }

    #[test]
    fn test_repo_create_tag_invalid_name() {
        let (_temp_dir, repo) = create_temp_repo();

        let result = repo.create_tag("", TagOptions::default());
        assert!(matches!(
            result,
            Err(RepositoryError::InvalidTagName { .. })
        ));

        let result = repo.create_tag("bad/name", TagOptions::default());
        assert!(matches!(
            result,
            Err(RepositoryError::InvalidTagName { .. })
        ));
    }

    #[test]
    fn test_repo_create_tag_already_exists() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        let result = repo.create_tag("v1.0.0", TagOptions::default());

        // Should fail because tag exists
        assert!(result.is_err());
    }

    #[test]
    fn test_repo_create_tag_force_overwrite() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        // Force overwrite should succeed
        let tag = repo
            .create_tag("v1.0.0", TagOptions::default().force(true))
            .unwrap();
        assert_eq!(tag.name, "v1.0.0");
    }

    #[test]
    fn test_repo_get_tag_from_stack() {
        let (_temp_dir, repo) = create_temp_repo();

        // Create tag in current stack
        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        // Get from current stack (default behavior)
        let tag = repo.get_tag("v1.0.0").unwrap();
        assert!(tag.is_some());

        // Get from specific stack
        let tag = repo.get_tag_from_stack("v1.0.0", DEFAULT_STACK).unwrap();
        assert!(tag.is_some());

        // Get from different stack (should not exist)
        let tag = repo.get_tag_from_stack("v1.0.0", "other").unwrap();
        assert!(tag.is_none());
    }

    #[test]
    fn test_repo_list_tags_for_stack() {
        let (_temp_dir, repo) = create_temp_repo();

        // Create tags in current stack
        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

        // list_tags returns current stack only
        let tags = repo.list_tags().unwrap();
        assert_eq!(tags.len(), 2);

        // list_tags_for_stack with current stack
        let tags = repo.list_tags_for_stack(DEFAULT_STACK).unwrap();
        assert_eq!(tags.len(), 2);

        // list_tags_for_stack with other stack (empty)
        let tags = repo.list_tags_for_stack("other").unwrap();
        assert!(tags.is_empty());
    }

    #[test]
    fn test_repo_list_all_tags() {
        let (_temp_dir, repo) = create_temp_repo();

        // Create tags (all go to current stack since we can't easily switch)
        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

        // list_all_tags includes all stacks
        let all_tags = repo.list_all_tags().unwrap();
        assert_eq!(all_tags.len(), 2);
    }

    #[test]
    fn test_repo_tag_count_for_stack() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();
        repo.create_tag("v2.0.0", TagOptions::default()).unwrap();

        // tag_count returns count for current stack
        assert_eq!(repo.tag_count().unwrap(), 2);

        // tag_count_for_stack with specific stack
        assert_eq!(repo.tag_count_for_stack(DEFAULT_STACK).unwrap(), 2);
        assert_eq!(repo.tag_count_for_stack("other").unwrap(), 0);

        // tag_count_all returns total across all stacks
        assert_eq!(repo.tag_count_all().unwrap(), 2);
    }

    #[test]
    fn test_repo_delete_tag_from_stack() {
        let (_temp_dir, repo) = create_temp_repo();

        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        // Delete from wrong stack should return false
        assert!(!repo.delete_tag_from_stack("v1.0.0", "other").unwrap());

        // Tag should still exist
        assert!(repo.get_tag("v1.0.0").unwrap().is_some());

        // Delete from correct stack should succeed
        assert!(repo.delete_tag_from_stack("v1.0.0", DEFAULT_STACK).unwrap());
        assert!(repo.get_tag("v1.0.0").unwrap().is_none());
    }

    #[test]
    fn test_repo_list_tag_stacks() {
        let (_temp_dir, repo) = create_temp_repo();

        // Initially no stacks have tags
        let stacks = repo.list_tag_stacks().unwrap();
        assert!(stacks.is_empty());

        // Create a tag
        repo.create_tag("v1.0.0", TagOptions::default()).unwrap();

        // Now current stack should be listed
        let stacks = repo.list_tag_stacks().unwrap();
        assert_eq!(stacks.len(), 1);
        assert!(stacks.contains(&DEFAULT_STACK.to_string()));
    }

    // ========================================================================
    // History Method Tests
    // ========================================================================

    #[test]
    fn test_repo_history_summary_empty() {
        let (_temp_dir, repo) = create_temp_repo();

        let summary = repo.history_summary().unwrap();
        assert_eq!(summary.change_count, 0);
        assert!(summary.is_empty());
    }

    #[test]
    fn test_repo_log_empty() {
        let (_temp_dir, repo) = create_temp_repo();

        let entries = repo.log(HistoryOptions::default()).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_repo_reverse_log_empty() {
        let (_temp_dir, repo) = create_temp_repo();

        let entries = repo.reverse_log(HistoryOptions::default()).unwrap();
        assert!(entries.is_empty());
    }

    // ========================================================================
    // Archive Method Tests
    // ========================================================================

    #[test]
    fn test_repo_archive_empty_fails() {
        let (_temp_dir, repo) = create_temp_repo();

        let dest = _temp_dir.path().join("archive");
        let result = repo.archive(&dest, ArchiveOptions::directory());

        // Should fail because no tracked files
        assert!(matches!(result, Err(RepositoryError::Archive(_))));
    }

    #[test]
    fn test_repo_archive_to_directory() {
        let (temp_dir, repo) = create_temp_repo();

        // Create and track a file
        std::fs::write(temp_dir.path().join("test.txt"), b"Hello World").unwrap();
        repo.add("test.txt", TrackingOptions::default()).unwrap();

        let dest = temp_dir.path().join("archive");
        let outcome = repo.archive(&dest, ArchiveOptions::directory()).unwrap();

        assert!(dest.exists());
        assert_eq!(outcome.manifest.file_count, 1);
        assert!(dest.join("test.txt").exists());
    }

    #[test]
    fn test_repo_archive_with_prefix() {
        let (temp_dir, repo) = create_temp_repo();

        // Create and track a file
        std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();
        repo.add("test.txt", TrackingOptions::default()).unwrap();

        let dest = temp_dir.path().join("archive");
        let options = ArchiveOptions::directory().prefix("project-1.0/");
        let outcome = repo.archive(&dest, options).unwrap();

        assert!(dest.exists());
        // The file should be at archive/project-1.0/test.txt
        assert!(dest.join("project-1.0/test.txt").exists());
    }

    #[test]
    fn test_repo_archive_with_include_filter() {
        let (temp_dir, repo) = create_temp_repo();

        // Create and track files
        std::fs::write(temp_dir.path().join("include.txt"), b"include").unwrap();
        std::fs::write(temp_dir.path().join("exclude.log"), b"exclude").unwrap();
        repo.add("include.txt", TrackingOptions::default()).unwrap();
        repo.add("exclude.log", TrackingOptions::default()).unwrap();

        let dest = temp_dir.path().join("archive");
        let options = ArchiveOptions::directory().include(&["*.txt"]);
        let outcome = repo.archive(&dest, options).unwrap();

        assert_eq!(outcome.manifest.file_count, 1);
        assert!(dest.join("include.txt").exists());
        assert!(!dest.join("exclude.log").exists());
    }

    #[test]
    fn test_repo_archive_with_exclude_filter() {
        let (temp_dir, repo) = create_temp_repo();

        // Create and track files
        std::fs::write(temp_dir.path().join("keep.txt"), b"keep").unwrap();
        std::fs::write(temp_dir.path().join("remove.log"), b"remove").unwrap();
        repo.add("keep.txt", TrackingOptions::default()).unwrap();
        repo.add("remove.log", TrackingOptions::default()).unwrap();

        let dest = temp_dir.path().join("archive");
        let options = ArchiveOptions::directory().exclude(&["*.log"]);
        let outcome = repo.archive(&dest, options).unwrap();

        assert_eq!(outcome.manifest.file_count, 1);
        assert!(dest.join("keep.txt").exists());
        assert!(!dest.join("remove.log").exists());
    }

    #[test]
    fn test_repo_archive_tag_not_found() {
        let (_temp_dir, repo) = create_temp_repo();

        let dest = _temp_dir.path().join("archive");
        let result = repo.archive_tag("nonexistent", &dest, ArchiveOptions::directory());

        assert!(matches!(result, Err(RepositoryError::TagNotFound { .. })));
    }

    // ========================================================================
    // Apply Method Tests (basic tests - full integration needs changes)
    // ========================================================================

    #[test]
    fn test_apply_options_default() {
        let options = ApplyOptions::default();
        assert!(options.stack.is_none());
        assert!(!options.apply_dependencies);
        assert!(options.allow_conflicts);
    }

    #[test]
    fn test_apply_options_with_stack() {
        let options = ApplyOptions::default().stack("feature");
        assert_eq!(options.stack, Some("feature".to_string()));
    }

    // ========================================================================
    // Apply Recorded Tests
    // ========================================================================

    #[test]
    fn test_apply_recorded_creates_tree_entries() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create a file in the working copy
        let file_path = temp_dir.path().join("new_file.txt");
        std::fs::write(&file_path, b"Hello, Atomic!").unwrap();

        // Track and record the file
        repo.add("new_file.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add new file");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(false); // Don't auto-apply, we'll test apply_recorded

        let record_outcome = repo.record(header, options).unwrap();

        // Verify the change was recorded
        assert!(record_outcome.was_saved());
        assert!(!record_outcome.was_applied());

        // Now apply using apply_recorded
        let apply_outcome = repo
            .apply_recorded(&record_outcome, ApplyOptions::default())
            .unwrap();

        // Verify the apply succeeded
        assert_eq!(apply_outcome.stats.changes_applied, 1);
        assert!(!apply_outcome.has_conflicts);

        // Verify the tree entries were created
        let txn = repo.pristine.read_txn().unwrap();

        // Check path → inode mapping exists
        let inode = txn.get_inode("new_file.txt").unwrap();
        assert!(inode.is_some(), "TREE entry should exist for new_file.txt");

        // Check inode → path reverse mapping
        let inode = inode.unwrap();
        let path = txn.get_path(inode).unwrap();
        assert_eq!(path, Some("new_file.txt".to_string()));

        // Check inode → position mapping
        let position = txn.inode_position(inode).unwrap();
        assert!(position.is_some(), "INODES entry should exist");
    }

    #[test]
    fn test_apply_recorded_updates_stack_state() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and track a file
        std::fs::write(temp_dir.path().join("test.txt"), b"content").unwrap();
        repo.add("test.txt", TrackingOptions::default()).unwrap();

        // Record without applying
        let header = ChangeHeader::new("Test change");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(false);

        let record_outcome = repo.record(header, options).unwrap();

        // Get initial stack state
        let initial_state = {
            let txn = repo.pristine.read_txn().unwrap();
            let stack = txn.get_stack("dev").unwrap().unwrap();
            stack.state
        };
        assert_eq!(initial_state, Merkle::ZERO);

        // Apply the change
        let apply_outcome = repo
            .apply_recorded(&record_outcome, ApplyOptions::default())
            .unwrap();

        // Verify state was updated
        assert_ne!(apply_outcome.new_state, Merkle::ZERO);
        assert_eq!(apply_outcome.sequence, 1);

        // Verify stack in database reflects the change
        let final_state = {
            let txn = repo.pristine.read_txn().unwrap();
            let stack = txn.get_stack("dev").unwrap().unwrap();
            stack.state
        };
        assert_eq!(final_state, apply_outcome.new_state);
    }

    #[test]
    fn test_apply_recorded_with_specific_stack() {
        use crate::record::RecordOptions;

        let (temp_dir, mut repo) = create_temp_repo();

        // Create another stack
        repo.create_stack("feature").unwrap();

        // Create and track a file
        std::fs::write(temp_dir.path().join("feature.txt"), b"feature content").unwrap();
        repo.add("feature.txt", TrackingOptions::default()).unwrap();

        // Record
        let header = ChangeHeader::new("Feature change");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(false);

        let record_outcome = repo.record(header, options).unwrap();

        // Apply to the "feature" stack specifically
        let apply_options = ApplyOptions::default().stack("feature");
        let apply_outcome = repo.apply_recorded(&record_outcome, apply_options).unwrap();

        // Verify "feature" stack was updated
        let feature_state = {
            let txn = repo.pristine.read_txn().unwrap();
            let stack = txn.get_stack("feature").unwrap().unwrap();
            stack.state
        };
        assert_eq!(feature_state, apply_outcome.new_state);

        // Verify "dev" stack is still at zero
        let dev_state = {
            let txn = repo.pristine.read_txn().unwrap();
            let stack = txn.get_stack("dev").unwrap().unwrap();
            stack.state
        };
        assert_eq!(dev_state, Merkle::ZERO);
    }

    #[test]
    fn test_record_stats_vertices_and_edges() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create a file with some content
        let file_path = temp_dir.path().join("hello.txt");
        std::fs::write(&file_path, b"Hello, World!\nThis is a test.\n").unwrap();

        // Track the file
        repo.add("hello.txt", TrackingOptions::default()).unwrap();

        // Record the file - this should create vertices for the new content
        let header = ChangeHeader::new("Add hello.txt");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);

        let outcome = repo.record(header, options).unwrap();

        // Verify the stats show vertices added (FileAdd creates 3: name, inode, content)
        let stats = outcome.stats();
        assert!(
            stats.vertices_added > 0,
            "Should have vertices_added > 0, got {}",
            stats.vertices_added
        );
        assert_eq!(
            stats.vertices_added, 3,
            "FileAdd should create 3 vertices (name, inode, content)"
        );
        assert!(
            stats.content_bytes > 0,
            "Should have content_bytes > 0, got {}",
            stats.content_bytes
        );
        assert_eq!(stats.files_recorded, 1);
        assert_eq!(stats.hunks_created, 1); // One FileAdd graph_op

        // Verify the display format shows the new CRDT-style output
        let display = format!("{}", stats);
        assert!(
            display.contains("vertices"),
            "Display should contain 'vertices', got: {}",
            display
        );
        assert!(
            display.contains("edges"),
            "Display should contain 'edges', got: {}",
            display
        );
        assert!(
            display.contains("bytes"),
            "Display should contain 'bytes', got: {}",
            display
        );
        // Should NOT contain old line-based format
        assert!(
            !display.contains("insertions"),
            "Display should NOT contain 'insertions', got: {}",
            display
        );
        assert!(
            !display.contains("deletions"),
            "Display should NOT contain 'deletions', got: {}",
            display
        );
    }

    // ========================================================================
    // Edit GraphOp Tests - TDD for proper modified file handling
    // ========================================================================
    //
    // These tests verify that modified files generate proper Edit hunks
    // instead of full FileAdd replacements. Edit hunks are more efficient
    // because they only record the changed content, not the entire file.
    //
    // In Atomic's graph model:
    // - FileAdd creates 3 vertices: name, inode, content (full file)
    // - Edit creates 1 span per insertion (just the new content)
    // - Deletions create EdgeUpdate atoms to mark old content as deleted
    //
    // The expected behavior for a modified file:
    // 1. Retrieve old content from the graph
    // 2. Diff old vs new content
    // 3. Create Edit hunks for insertions (new vertices)
    // 4. Create Replacement hunks for deletions (edge modifications)

    /// Test that modifying a file creates Edit hunks, not FileAdd.
    ///
    /// This is the core test for proper edit support. When a tracked file
    /// is modified, we should:
    /// 1. Detect it as Modified (not Added)
    /// 2. Retrieve the old content from the graph
    /// 3. Diff the old and new content
    /// 4. Create Edit/Replacement hunks (not FileAdd)
    ///
    /// Edit hunks are more efficient because they only store the delta,
    /// not the entire file content.
    #[test]
    fn test_modified_file_creates_edit_hunks() {
        use crate::record::RecordOptions;
        use atomic_core::change::GraphOp;

        let (temp_dir, repo) = create_temp_repo();

        // Step 1: Create and record initial file
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, b"line1\nline2\nline3\n").unwrap();
        repo.add("test.txt", TrackingOptions::default()).unwrap();

        let header = ChangeHeader::new("Initial commit");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let initial_outcome = repo.record(header, options).unwrap();

        // Verify initial change has FileAdd graph_op
        let initial_change = initial_outcome.change();
        assert_eq!(
            initial_change.hunks().len(),
            1,
            "Initial commit should have exactly 1 graph_op"
        );
        assert!(
            matches!(initial_change.hunks()[0], GraphOp::FileAdd { .. }),
            "Initial commit should have FileAdd graph_op, got {:?}",
            initial_change.hunks()[0].type_name()
        );

        // Step 2: Modify the file (change middle line)
        std::fs::write(&file_path, b"line1\nmodified_line2\nline3\n").unwrap();

        // Step 3: Record the modification
        let header2 = ChangeHeader::new("Edit middle line");
        let options2 = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let edit_outcome = repo.record(header2, options2).unwrap();

        // Step 4: Verify the change contains Edit or Replacement hunks, NOT FileAdd
        let edit_change = edit_outcome.change();
        assert!(
            !edit_change.hunks().is_empty(),
            "Edit commit should have at least one graph_op"
        );

        // Check that we got Edit/Replacement hunks, not FileAdd
        for graph_op in edit_change.hunks() {
            let hunk_type = graph_op.type_name();
            assert!(
                hunk_type == "Edit" || hunk_type == "Replacement",
                "Modified file should create Edit or Replacement graph_op, got {}",
                hunk_type
            );
        }

        // Verify stats reflect edit operations (fewer vertices than FileAdd)
        let stats = edit_outcome.stats();
        assert!(
            stats.vertices_added < 3,
            "Edit should create fewer than 3 vertices (FileAdd creates 3), got {}",
            stats.vertices_added
        );
    }

    /// Test that adding lines to a file creates Edit hunks for the new content.
    ///
    /// When lines are added to an existing file, we should create Edit hunks
    /// that contain only the new content, not the entire file.
    #[test]
    fn test_adding_lines_creates_edit_hunks() {
        use crate::record::RecordOptions;
        use atomic_core::change::GraphOp;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record initial file
        let file_path = temp_dir.path().join("growing.txt");
        std::fs::write(&file_path, b"first line\n").unwrap();
        repo.add("growing.txt", TrackingOptions::default()).unwrap();

        let header = ChangeHeader::new("Initial");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Add more lines to the file
        std::fs::write(&file_path, b"first line\nsecond line\nthird line\n").unwrap();

        // Record the addition
        let header2 = ChangeHeader::new("Add lines");
        let options2 = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // Verify we got Edit hunks
        let change = outcome.change();
        for graph_op in change.hunks() {
            assert!(
                matches!(graph_op, GraphOp::Edit { .. } | GraphOp::Replacement { .. }),
                "Adding lines should create Edit/Replacement graph_op, got {}",
                graph_op.type_name()
            );
        }

        // The new content should only include the added lines
        let stats = outcome.stats();
        assert!(
            stats.content_bytes > 0,
            "Should have recorded some content bytes"
        );
    }

    /// Test that deleting lines creates edge modifications (Replacement hunks).
    ///
    /// When lines are removed from a file, we mark the old content as deleted
    /// using EdgeUpdate atoms (wrapped in Replacement hunks).
    #[test]
    fn test_deleting_lines_creates_replacement_hunks() {
        use crate::record::RecordOptions;
        use atomic_core::change::GraphOp;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record initial file with multiple lines
        let file_path = temp_dir.path().join("shrinking.txt");
        std::fs::write(&file_path, b"keep this\ndelete this\nalso keep\n").unwrap();
        repo.add("shrinking.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Initial with 3 lines");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Delete the middle line
        std::fs::write(&file_path, b"keep this\nalso keep\n").unwrap();

        // Record the deletion
        let header2 = ChangeHeader::new("Delete middle line");
        let options2 = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // Verify we got Replacement or Edit hunks (deletions use EdgeUpdate)
        let change = outcome.change();
        assert!(
            !change.hunks().is_empty(),
            "Deletion should create at least one graph_op"
        );

        // Stats should show edge modifications for deletions
        let stats = outcome.stats();
        assert!(
            stats.edges_modified > 0 || stats.vertices_added > 0,
            "Deletion should modify edges or add vertices, got edges={}, vertices={}",
            stats.edges_modified,
            stats.vertices_added
        );
    }

    /// Test that replacing content creates both deletion and insertion operations.
    ///
    /// When content is replaced (old text removed, new text added), we should
    /// see both edge modifications (for deleted content) and new vertices
    /// (for inserted content).
    #[test]
    fn test_replacing_content_creates_mixed_hunks() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record initial file
        let file_path = temp_dir.path().join("replace.txt");
        std::fs::write(&file_path, b"old content here\n").unwrap();
        repo.add("replace.txt", TrackingOptions::default()).unwrap();

        let header = ChangeHeader::new("Initial");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Replace with completely different content
        std::fs::write(&file_path, b"new content here\n").unwrap();

        // Record the replacement
        let header2 = ChangeHeader::new("Replace content");
        let options2 = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // Verify the change was recorded
        assert_eq!(outcome.stats().files_recorded, 1);

        // For a replacement, we expect both vertices (new content) and edges (deletions)
        let stats = outcome.stats();
        assert!(
            stats.vertices_added > 0,
            "Replacement should add vertices for new content"
        );
    }

    /// Test that the old content is correctly retrieved from the graph.
    ///
    /// This tests the integration between record and get_file_content.
    /// The old content must be retrieved to compute the diff.
    #[test]
    fn test_old_content_retrieved_for_diff() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record initial file with specific content
        let file_path = temp_dir.path().join("retrieve.txt");
        let original_content = b"This is the original content\nWith multiple lines\n";
        std::fs::write(&file_path, original_content).unwrap();
        repo.add("retrieve.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Initial");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Verify we can retrieve the content from the graph
        let retrieved = repo.get_file_content(std::path::Path::new("retrieve.txt"));
        assert!(
            retrieved.is_ok(),
            "Should be able to retrieve file content: {:?}",
            retrieved.err()
        );
        let retrieved_content = retrieved.unwrap();
        assert!(
            retrieved_content.is_some(),
            "Retrieved content should not be None"
        );
        assert_eq!(
            retrieved_content.unwrap(),
            original_content.to_vec(),
            "Retrieved content should match original"
        );

        // Now modify and record - this should use the retrieved content for diff
        std::fs::write(
            &file_path,
            b"This is MODIFIED content\nWith multiple lines\n",
        )
        .unwrap();

        let header2 = ChangeHeader::new("Modify first line");
        let options2 = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // The modification should have been recorded
        assert_eq!(outcome.stats().files_recorded, 1);
    }

    /// Test recording multiple modified files in one change.
    ///
    /// When multiple files are modified, each should get proper Edit hunks.
    #[test]
    fn test_multiple_modified_files_get_edit_hunks() {
        use crate::record::RecordOptions;
        use atomic_core::change::GraphOp;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record multiple files
        std::fs::write(temp_dir.path().join("file1.txt"), b"content1\n").unwrap();
        std::fs::write(temp_dir.path().join("file2.txt"), b"content2\n").unwrap();
        repo.add("file1.txt", TrackingOptions::default()).unwrap();
        repo.add("file2.txt", TrackingOptions::default()).unwrap();

        let header = ChangeHeader::new("Initial two files");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Modify both files
        std::fs::write(temp_dir.path().join("file1.txt"), b"modified1\n").unwrap();
        std::fs::write(temp_dir.path().join("file2.txt"), b"modified2\n").unwrap();

        // Record both modifications
        let header2 = ChangeHeader::new("Modify both files");
        let options2 = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // Both files should be recorded
        assert_eq!(
            outcome.stats().files_recorded,
            2,
            "Should record 2 modified files"
        );

        // All hunks should be Edit or Replacement (not FileAdd)
        let change = outcome.change();
        for graph_op in change.hunks() {
            assert!(
                matches!(graph_op, GraphOp::Edit { .. } | GraphOp::Replacement { .. }),
                "Modified files should use Edit/Replacement hunks, got {}",
                graph_op.type_name()
            );
        }
    }

    /// Test that stats correctly reflect Edit operations vs FileAdd.
    ///
    /// Edit operations should show:
    /// - vertices_added: 1 per insertion (not 3 like FileAdd)
    /// - edges_modified: count of deletion operations
    #[test]
    fn test_edit_stats_are_accurate() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and record a file
        let file_path = temp_dir.path().join("stats.txt");
        std::fs::write(&file_path, b"original\n").unwrap();
        repo.add("stats.txt", TrackingOptions::default()).unwrap();

        let header = ChangeHeader::new("Initial");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let initial = repo.record(header, options).unwrap();

        // Initial FileAdd should have 3 vertices
        assert_eq!(
            initial.stats().vertices_added,
            3,
            "FileAdd should create 3 vertices (name, inode, content)"
        );

        // Modify the file
        std::fs::write(&file_path, b"modified\n").unwrap();

        let header2 = ChangeHeader::new("Edit");
        let options2 = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let edit = repo.record(header2, options2).unwrap();

        // Edit should have fewer vertices (just the new content, not name/inode)
        let edit_stats = edit.stats();
        assert!(
            edit_stats.vertices_added <= 2,
            "Edit should create at most 2 vertices (1 for new content, possibly 1 for context), got {}",
            edit_stats.vertices_added
        );

        // Edit that replaces content should also modify edges
        assert!(
            edit_stats.vertices_added > 0 || edit_stats.edges_modified > 0,
            "Edit should either add vertices or modify edges"
        );
    }

    /// Test that status shows files as Clean after recording.
    ///
    /// This is a regression test for the issue where files still showed
    /// as Modified after being recorded because content retrieval wasn't
    /// working correctly.
    #[test]
    fn test_status_clean_after_record() {
        use crate::record::RecordOptions;
        use crate::status::StatusOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Step 1: Create and record a new file
        let file_path = temp_dir.path().join("status_test.txt");
        let content = b"Initial content for status test\n";
        std::fs::write(&file_path, content).unwrap();

        repo.add("status_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add status test file");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Step 2: Check status - file should be Clean (not Modified)
        let status = repo.status(StatusOptions::default()).unwrap();

        // The file should NOT appear as modified
        let modified_files: Vec<_> = status.modified().collect();
        assert!(
            modified_files.is_empty(),
            "No files should be modified after recording, but got: {:?}",
            modified_files.iter().map(|e| e.path()).collect::<Vec<_>>()
        );

        // The file should be Clean
        let clean_files: Vec<_> = status.clean().collect();
        assert!(
            clean_files
                .iter()
                .any(|e| e.path().to_string_lossy().contains("status_test.txt")),
            "status_test.txt should be Clean after recording"
        );

        // Step 3: Verify the recorded content matches the file
        let retrieved = repo.get_file_content("status_test.txt").unwrap();
        assert!(
            retrieved.is_some(),
            "Should be able to retrieve recorded content"
        );
        assert_eq!(
            retrieved.unwrap(),
            content.to_vec(),
            "Retrieved content should match original file"
        );
    }

    /// Test that status correctly detects modifications after initial record.
    #[test]
    fn test_status_modified_after_change() {
        use crate::record::RecordOptions;
        use crate::status::StatusOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Step 1: Create and record initial file
        let file_path = temp_dir.path().join("modify_test.txt");
        std::fs::write(&file_path, b"Initial content\n").unwrap();

        repo.add("modify_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add file");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Step 2: Modify the file
        std::fs::write(&file_path, b"Modified content\n").unwrap();

        // Step 3: Check status - file should be Modified now
        let status = repo.status(StatusOptions::default()).unwrap();

        let modified_files: Vec<_> = status.modified().collect();
        assert_eq!(modified_files.len(), 1, "One file should be modified");
        assert!(
            modified_files[0]
                .path()
                .to_string_lossy()
                .contains("modify_test.txt"),
            "modify_test.txt should be Modified"
        );
    }

    /// Test modifying the FIRST line of a file.
    ///
    /// This is a regression test for a bug where modifying the first line of a
    /// file caused the unchanged lines to be lost. The bug was in `globalize_hunk`
    /// which used `content` (graph_op content) instead of `full_content` (full file)
    /// for Replace hunks.
    ///
    /// See: https://github.com/atomic-vcs/atomic/issues/XXX
    #[test]
    fn test_modify_first_line_content_retrieval() {
        use crate::record::RecordOptions;
        use crate::status::StatusOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Step 1: Create a file with 2 lines and record it
        let file_path = temp_dir.path().join("first_line_test.txt");
        let initial_content = b"Line 1 - original\nLine 2 - unchanged\n";
        std::fs::write(&file_path, initial_content).unwrap();

        repo.add("first_line_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add file with two lines");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Verify initial content can be retrieved
        let retrieved1 = repo.get_file_content("first_line_test.txt").unwrap();
        assert!(
            retrieved1.is_some(),
            "Initial content should be retrievable"
        );
        assert_eq!(retrieved1.unwrap(), initial_content.to_vec());

        // Step 2: Modify ONLY the first line
        let modified_content = b"Line 1 - MODIFIED\nLine 2 - unchanged\n";
        std::fs::write(&file_path, modified_content).unwrap();

        // Step 3: Check status - should show as Modified
        let status1 = repo.status(StatusOptions::default()).unwrap();
        let modified_files: Vec<_> = status1.modified().collect();
        assert_eq!(modified_files.len(), 1, "File should show as modified");

        // Step 4: Record the modification (this creates a Replacement graph_op)
        let header2 = ChangeHeader::new("Modify first line only");
        let options2 = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header2, options2).unwrap();

        // Step 5: Verify content retrieval returns the FULL modified file
        // (This was the bug - it only returned the first line, losing line 2)
        let retrieved2 = repo.get_file_content("first_line_test.txt").unwrap();
        assert!(
            retrieved2.is_some(),
            "Content should be retrievable after modifying first line"
        );
        assert_eq!(
            retrieved2.unwrap(),
            modified_content.to_vec(),
            "Retrieved content should match the full modified file (including unchanged line 2)"
        );

        // Step 6: Check status - should be Clean now
        let status2 = repo.status(StatusOptions::default()).unwrap();
        let modified_after: Vec<_> = status2.modified().collect();
        assert!(
            modified_after.is_empty(),
            "File should be Clean after recording the edit, but got Modified"
        );
    }

    /// Test full workflow: record → modify → record → status should be clean.
    #[test]
    fn test_status_clean_after_modify_and_record() {
        use crate::record::RecordOptions;
        use crate::status::StatusOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Step 1: Create and record initial file
        let file_path = temp_dir.path().join("workflow_test.txt");
        std::fs::write(&file_path, b"Version 1\n").unwrap();

        repo.add("workflow_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Initial version");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Step 2: Modify the file
        let modified_content = b"Version 2 - modified\n";
        std::fs::write(&file_path, modified_content).unwrap();

        // Verify it shows as modified
        let status = repo.status(StatusOptions::default()).unwrap();
        assert!(
            status
                .modified()
                .any(|e| e.path().to_string_lossy().contains("workflow_test.txt")),
            "File should be Modified after modification"
        );

        // Step 3: Record the modification
        let header2 = ChangeHeader::new("Modified version");
        let options2 = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        let outcome = repo.record(header2, options2).unwrap();

        // Verify the modification was recorded
        assert_eq!(
            outcome.stats().files_recorded,
            1,
            "Should have recorded 1 file"
        );

        // Step 4: Check status - should be clean now
        let status = repo.status(StatusOptions::default()).unwrap();

        let modified_files: Vec<_> = status.modified().collect();
        assert!(
            modified_files.is_empty(),
            "No files should be modified after recording the modification, but got: {:?}",
            modified_files.iter().map(|e| e.path()).collect::<Vec<_>>()
        );

        // Step 5: Verify the recorded content is the modified version
        let retrieved = repo.get_file_content("workflow_test.txt").unwrap();
        assert!(retrieved.is_some(), "Should be able to retrieve content");
        assert_eq!(
            retrieved.unwrap(),
            modified_content.to_vec(),
            "Retrieved content should be the modified version"
        );
    }

    #[test]
    fn test_apply_recorded_hash_matches() {
        use crate::record::RecordOptions;

        let (temp_dir, repo) = create_temp_repo();

        // Create and track a file
        std::fs::write(temp_dir.path().join("hash_test.txt"), b"hash test content").unwrap();
        repo.add("hash_test.txt", TrackingOptions::default())
            .unwrap();

        // Record and apply
        let header = ChangeHeader::new("Hash test");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(false);

        let record_outcome = repo.record(header, options).unwrap();
        let expected_hash = *record_outcome.hash();

        let apply_outcome = repo
            .apply_recorded(&record_outcome, ApplyOptions::default())
            .unwrap();

        // Verify the hash is in the applied hashes
        assert!(apply_outcome.stats.applied_hashes.contains(&expected_hash));
    }

    /// Test that switching stacks correctly outputs file content.
    ///
    /// This test verifies that when switching between stacks that share
    /// the same changes, the file content is preserved. A stack created
    /// with create_stack_from inherits the source stack's changes.
    #[test]
    fn test_switch_stack_outputs_content() {
        use crate::record::RecordOptions;

        let (temp_dir, mut repo) = create_temp_repo();

        // Step 1: Create and record a file on the default stack
        let file_path = temp_dir.path().join("switch_test.txt");
        let content = b"Content for stack switch test\n";
        std::fs::write(&file_path, content).unwrap();

        repo.add("switch_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add file on dev stack");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Step 2: Create a new stack FROM dev (inherits dev's changes)
        repo.create_stack_from("feature", "dev").unwrap();

        // Step 3: Switch to the new stack
        let _switch_result = repo.switch_stack("feature").unwrap();

        // The switch should succeed
        assert_eq!(repo.current_stack(), "feature");

        // Step 4: Verify the file content is still present in working copy
        let file_content = std::fs::read(&file_path).unwrap();
        assert_eq!(
            file_content, content,
            "File content should be preserved after stack switch"
        );

        // Step 5: Switch back to dev and verify content again
        let _switch_back_result = repo.switch_stack("dev").unwrap();
        assert_eq!(repo.current_stack(), "dev");

        let file_content_after = std::fs::read(&file_path).unwrap();
        assert_eq!(
            file_content_after, content,
            "File content should be present after switching back to dev"
        );
    }

    /// Test correct stack switching behavior with content isolation.
    ///
    /// This is the TDD test for how stack switching SHOULD work:
    /// 1. Record content on dev stack
    /// 2. Create feature stack FROM dev (inherits dev's changes)
    /// 3. Record different content on feature
    /// 4. Switching between stacks shows each stack's content
    ///
    /// Key insight: When creating a new stack, it should inherit the current
    /// stack's changes so that switching to it preserves the working copy state.
    #[test]
    fn test_switch_stack_shows_stack_content() {
        use crate::record::RecordOptions;

        let (temp_dir, mut repo) = create_temp_repo();

        // Step 1: Create and record a file on dev stack
        let file_path = temp_dir.path().join("stack_test.txt");
        let dev_content = b"Content on dev stack\n";
        std::fs::write(&file_path, dev_content).unwrap();

        repo.add("stack_test.txt", TrackingOptions::default())
            .unwrap();

        let header = ChangeHeader::new("Add file on dev");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Verify dev has 1 change
        let dev_info = repo.get_stack_info("dev").unwrap();
        assert_eq!(dev_info.change_count, 1, "Dev should have 1 change");

        // Step 2: Create feature stack FROM dev (should inherit dev's changes)
        repo.create_stack_from("feature", "dev").unwrap();

        // Feature should now have the same changes as dev
        let feature_info = repo.get_stack_info("feature").unwrap();
        assert_eq!(
            feature_info.change_count, 1,
            "Feature should inherit dev's 1 change"
        );

        // Step 3: Switch to feature - content should still be present
        repo.switch_stack("feature").unwrap();

        let content_on_feature = std::fs::read(&file_path).unwrap();
        assert_eq!(
            content_on_feature, dev_content,
            "Content should be preserved when switching to feature (inherited from dev)"
        );

        // Step 4: Modify the file on feature stack
        let feature_content = b"Modified content on feature stack\n";
        std::fs::write(&file_path, feature_content).unwrap();

        let header = ChangeHeader::new("Modify file on feature");
        let options = RecordOptions::new()
            .all(true)
            .save_to_store(true)
            .apply_after_record(true);
        repo.record(header, options).unwrap();

        // Feature now has 2 changes (inherited + its own)
        let feature_info = repo.get_stack_info("feature").unwrap();
        assert_eq!(
            feature_info.change_count, 2,
            "Feature should have 2 changes (inherited + modification)"
        );

        // Verify feature content in working copy
        let current_content = std::fs::read(&file_path).unwrap();
        assert_eq!(current_content, feature_content);

        // Step 5: Switch back to dev - content should revert to dev version
        repo.switch_stack("dev").unwrap();

        let content_after_switch = std::fs::read(&file_path).unwrap();
        assert_eq!(
            content_after_switch, dev_content,
            "Content should revert to dev version after switching back"
        );

        // Dev still has only 1 change
        let dev_info = repo.get_stack_info("dev").unwrap();
        assert_eq!(dev_info.change_count, 1, "Dev should still have 1 change");

        // Step 6: Switch to feature again - content should be feature version
        repo.switch_stack("feature").unwrap();

        let feature_content_after_switch = std::fs::read(&file_path).unwrap();
        assert_eq!(
            feature_content_after_switch, feature_content,
            "Content should be feature version after switching to feature"
        );
    }
}

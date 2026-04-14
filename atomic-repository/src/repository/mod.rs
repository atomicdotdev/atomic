//! Repository management for Atomic VCS
//!
//! This module provides the main `Repository` abstraction that coordinates
//! all VCS operations including initialization, opening existing repositories,
//! recording changes, and working copy management.
//!
//! # Views
//!
//! Atomic uses **Views** to organize changes. This is a fundamental conceptual
//! difference from Git:
//!
//! | Concept | Git Branches | Atomic Views |
//! |---------|--------------|--------------|
//! | Nature | Fork of history | Perspective on the graph |
//! | Data | Duplicates commits | References same changes |
//! | Merge | Combines divergent histories | Inserts missing changes |
//! | Identity | Pointer to a commit | Ordered sequence + Merkle state |
//!
//! Views are **perspectives** on the graph - they represent which changes have
//! been inserted and in what order. Multiple views can coexist, each showing a
//! different perspective on the same underlying data.
//!
//! # Change Storage
//!
//! Changes are stored in a content-addressed manner under `.atomic/changes/`.
//! The repository provides convenient methods for saving and loading changes
//! that integrate with the underlying [`ChangeStore`].
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
use std::sync::Arc;

use atomic_core::change::{Author, Change, ChangeHeader, GraphOp};
use atomic_core::output::repo::{materialize_view, MaterializeOptions, MaterializeResult};
use atomic_core::output::FileSystem;
use atomic_core::output::WorkingCopy;
use atomic_core::pristine::{GraphTxnT, MutTxnT, Pristine, TreeTxnT, ViewScope, ViewTxnT};
use atomic_core::record::workflow::retrieve::{RetrieveContentOptions, RetrieveResult};
use atomic_core::types::{Base32, Hash, Inode, Merkle, NodeId, Position};

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

// ── Sub-modules (new) ───────────────────────────────────────────────────

mod filter;
mod materialize;
mod switch;
mod views;

// Re-export public items so external callers and sibling sub-modules that
// use `use super::*;` continue to resolve them at `crate::repository::…`.
pub use filter::{collect_view_change_ids, collect_visible_change_ids};
pub use views::ViewInfo;

// Re-import workspace helpers from `switch` so they are available to
// `mod.rs` (used in `init`) and to sibling sub-modules via `use super::*;`.
use switch::{ensure_workspace_dir, workspace_path};

// ── Sub-modules (existing) ──────────────────────────────────────────────

mod archive;
mod changes;
mod content;
mod history;
mod insert;
mod record;
mod remotes;
mod status;
mod tags;
mod tracking;
mod vault;
mod vault_defaults;
mod vault_embeddings;
mod vault_goal;
mod vault_identity;
mod vault_intent;
mod vault_kg_enrich;
mod vault_names;
mod vault_triples;
pub use vault_embeddings::{hash_embed, EmbedConfig, TextChunk};
pub use vault_goal::{
    GoalInfo, GoalStartOptions, GoalStartResult, GoalStopOptions, GoalStopResult,
};
pub use vault_identity::VaultIdentity;
pub use vault_intent::{IntentCreateOptions, IntentCreateResult, IntentInfo, IntentUpdateOptions};
pub use vault_kg_enrich::KgEnrichStats;
pub use vault_names::{derive_intent_prefix, generate_goal_name};

#[cfg(test)]
mod tests;

// ── Constants ───────────────────────────────────────────────────────────

/// The name of the Atomic directory
pub const DOT_DIR: &str = ".atomic";

/// The default view name
pub const DEFAULT_VIEW: &str = "dev";

/// Backward-compatible alias for [`DEFAULT_VIEW`].
pub const DEFAULT_STACK: &str = DEFAULT_VIEW;

/// Subdirectory inside `.atomic/` that holds per-view workspace state.
///
/// Each view gets a directory at `.atomic/workspaces/<view_name>/` where
/// ignored/artifact files are shelved on `switch_view` and restored when
/// switching back.  This is the mechanism by which views achieve full
/// working copy isolation — not just tracked files (managed by the graph)
/// but also build artifacts like `node_modules/`, `dist/`, `.next/`, etc.
const WORKSPACES_DIR: &str = "workspaces";

// ── Repository struct ───────────────────────────────────────────────────

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
    /// Current view name
    current_view: String,
    /// The pristine database handle.
    ///
    /// Wrapped in `Arc` so that multiple `Repository` instances _can_
    /// share the same underlying redb `Database` and avoid stale-snapshot
    /// bugs. In practice, sharing only happens when constructing via
    /// `open_with_pristine`; `open` / `open_readonly` create a fresh
    /// `Pristine` for each `Repository`.
    pristine: Arc<Pristine>,
    /// The change store for persisting changes
    change_store: ChangeStore,
}

impl std::fmt::Debug for Repository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repository")
            .field("root", &self.root)
            .field("dot_dir", &self.dot_dir)
            .field("current_view", &self.current_view)
            .field("pristine", &"<Pristine>")
            .field("change_store", &self.change_store)
            .finish()
    }
}

// ── Construction, accessors, internal helpers ────────────────────────────

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
        std::fs::create_dir_all(dot_dir.join(WORKSPACES_DIR))?;

        // Create initial config
        let config_path = dot_dir.join("config.toml");
        let initial_config = format!(
            r#"# Atomic repository configuration

[view]
default = "{}"
"#,
            DEFAULT_VIEW
        );
        std::fs::write(&config_path, initial_config)?;

        // Create working copy ID file
        let wc_id_path = dot_dir.join("working_copy_id");
        std::fs::write(&wc_id_path, "")?;

        // Initialize the pristine database (redb creates the file)
        let pristine = Arc::new(
            Pristine::open(dot_dir.join("pristine.redb"))
                .map_err(|e| RepositoryError::Database(e.to_string()))?,
        );

        // Create the default view and its workspace directory
        {
            let mut txn = pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.open_or_create_view(DEFAULT_VIEW)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }
        ensure_workspace_dir(&dot_dir, DEFAULT_VIEW)?;

        // Initialize the change store
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Self {
            root,
            dot_dir,
            current_view: DEFAULT_VIEW.to_string(),
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
        let pristine = Arc::new(
            Pristine::open(dot_dir.join("pristine.redb"))
                .map_err(|e| RepositoryError::Database(e.to_string()))?,
        );

        // Read current view from config or use default
        let current_view =
            Self::read_current_view(&dot_dir).unwrap_or_else(|_| DEFAULT_STACK.to_string());

        // Open the change store
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Self {
            root,
            dot_dir,
            current_view,
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
        let pristine = Arc::new(
            Pristine::open_readonly(dot_dir.join("pristine.redb"))
                .map_err(|e| RepositoryError::Database(e.to_string()))?,
        );

        // Read current view from config or use default
        let current_view =
            Self::read_current_view(&dot_dir).unwrap_or_else(|_| DEFAULT_STACK.to_string());

        // Open the change store
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Self {
            root,
            dot_dir,
            current_view,
            pristine,
            change_store,
        })
    }

    /// Open an existing repository using a pre-opened `Pristine`.
    ///
    /// This constructor is used by the storage server to share a single
    /// `Pristine` (redb `Database`) handle across concurrent requests to
    /// the same project.  Sharing the handle ensures every write transaction
    /// sees the committed state of prior transactions — opening a fresh
    /// `Database` per request can see stale snapshots.
    ///
    /// # Requirements
    ///
    /// The provided `pristine` **must** have been opened from
    /// `<path>/.atomic/pristine.redb` (i.e. the same repository that
    /// `path` resolves to). Passing a `Pristine` from a different
    /// repository will silently couple one repo's configuration and
    /// change store with another repo's database, leading to corruption.
    pub fn open_with_pristine<P: AsRef<Path>>(
        path: P,
        pristine: Arc<Pristine>,
    ) -> Result<Self, RepositoryError> {
        let root = Self::find_root(path.as_ref())?;
        let dot_dir = root.join(DOT_DIR);

        let current_view =
            Self::read_current_view(&dot_dir).unwrap_or_else(|_| DEFAULT_STACK.to_string());

        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Self {
            root,
            dot_dir,
            current_view,
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

    // ── Path accessors ──────────────────────────────────────────────────

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

    /// Get the current view name.
    #[inline]
    pub fn current_view(&self) -> &str {
        &self.current_view
    }

    /// Get the config file path.
    #[inline]
    pub fn config_path(&self) -> PathBuf {
        self.dot_dir.join("config.toml")
    }

    // ── View pointer (internal state) ───────────────────────────────────

    /// Set the current view (internal, does not update working copy).
    ///
    /// This updates both the in-memory state and persists the change to disk,
    /// but does NOT update the working copy. Use `switch_view` instead for
    /// the full switch operation that also updates the working copy.
    ///
    /// # Arguments
    ///
    /// * `view` - The name of the view to switch to
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The view does not exist in the pristine database
    /// - The view file cannot be written
    pub fn set_current_view(&mut self, view: &str) -> Result<(), RepositoryError> {
        // Verify the view exists in the pristine database
        {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            if txn
                .get_view(view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .is_none()
            {
                return Err(RepositoryError::ViewNotFound {
                    name: view.to_string(),
                });
            }
        }

        self.current_view = view.to_string();
        self.write_current_view(view)?;
        Ok(())
    }

    // ── Internal helpers ────────────────────────────────────────────────

    /// Get a reference to the pristine database.
    #[inline]
    pub fn pristine(&self) -> &Pristine {
        &self.pristine
    }

    /// Read the current view from the config file.
    fn read_current_view(dot_dir: &Path) -> Result<String, RepositoryError> {
        let current_path = dot_dir.join("current_view");
        if current_path.exists() {
            let content = std::fs::read_to_string(&current_path)?;
            Ok(content.trim().to_string())
        } else {
            // Fall back to legacy path for backward compatibility
            let legacy_path = dot_dir.join("current_stack");
            if legacy_path.exists() {
                let content = std::fs::read_to_string(&legacy_path)?;
                Ok(content.trim().to_string())
            } else {
                Ok(DEFAULT_STACK.to_string())
            }
        }
    }

    /// Write the current view to disk.
    fn write_current_view(&self, view: &str) -> Result<(), RepositoryError> {
        let current_path = self.dot_dir.join("current_view");
        std::fs::write(&current_path, view)?;
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

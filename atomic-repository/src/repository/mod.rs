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

/// The name of the Atomic directory
pub const DOT_DIR: &str = ".atomic";

/// The default view name
pub const DEFAULT_STACK: &str = "dev";

/// Subdirectory inside `.atomic/` that holds per-view workspace state.
///
/// Each view gets a directory at `.atomic/workspaces/<view_name>/` where
/// ignored/artifact files are shelved on `switch_view` and restored when
/// switching back.  This is the mechanism by which views achieve full
/// working copy isolation — not just tracked files (managed by the graph)
/// but also build artifacts like `node_modules/`, `dist/`, `.next/`, etc.
const WORKSPACES_DIR: &str = "workspaces";

/// Return the workspace directory path for a given view.
///
/// The path is `.atomic/workspaces/<view_name>/`.  View names may
/// contain `/` (e.g. `agent/ses_abc123`), which becomes a nested
/// directory structure.
fn workspace_path(dot_dir: &Path, view_name: &str) -> PathBuf {
    dot_dir.join(WORKSPACES_DIR).join(view_name)
}

/// Ensure the workspace directory for a view exists.
///
/// Creates `.atomic/workspaces/<view_name>/` and any intermediate
/// directories.  This is called from `init`, `create_view`, and
/// `create_view_from`.
fn ensure_workspace_dir(dot_dir: &Path, view_name: &str) -> Result<(), RepositoryError> {
    let ws = workspace_path(dot_dir, view_name);
    std::fs::create_dir_all(&ws)?;
    Ok(())
}

/// Remove empty ancestor directories after file removal.
///
/// Given an iterator of relative paths that were just deleted, this
/// collects every parent directory, sorts them deepest-first, and
/// attempts `std::fs::remove_dir` on each.  Because `remove_dir` only
/// succeeds on *empty* directories, this is always safe — a directory
/// that still contains files (tracked, untracked, or otherwise) will
/// simply fail silently.
///
/// Extracting this into a standalone helper keeps `switch_view` at the
/// orchestration level and makes the cleanup logic reusable for other
/// operations (e.g. `atomic clean`).
fn cleanup_empty_ancestors<'a>(root: &Path, removed_paths: impl Iterator<Item = &'a str>) {
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    for path in removed_paths {
        let p = PathBuf::from(path);
        let mut ancestor = p.parent();
        while let Some(dir) = ancestor {
            if dir == Path::new("") || dir == Path::new(".") {
                break;
            }
            dirs.insert(dir.to_path_buf());
            ancestor = dir.parent();
        }
    }
    // Sort deepest-first so children are removed before parents.
    let mut sorted: Vec<PathBuf> = dirs.into_iter().collect();
    sorted.sort_by_key(|a| std::cmp::Reverse(a.components().count()));
    for dir in sorted {
        let abs = root.join(&dir);
        if abs.is_dir() {
            // Only succeeds if the directory is empty — safe by construction.
            let _ = std::fs::remove_dir(&abs);
        }
    }
}

/// Collect all change `NodeId`s inserted into a view into a `HashSet`.
///
/// This is the canonical helper for building a **change filter** — the set
/// of changes that define a view's content.  It is used by:
///
/// - `materialize` (to filter which files are materialised)
/// - `visible_file_paths` (to compute the file set for `switch_view`)
/// - `status` (to decide which tracked files are "ours")
/// - `get_file_content*` variants (to scope graph retrieval)
///
/// Centralising this pattern eliminates duplication and ensures every
/// call site uses the same iteration + error handling.
///
/// # Complexity
///
/// O(C) where C is the number of changes on the view — a single linear
/// scan of `VIEW_CHANGES`.
pub fn collect_view_change_ids<T: ViewTxnT>(
    txn: &T,
    view: &atomic_core::pristine::ViewState,
) -> Result<HashSet<NodeId>, RepositoryError> {
    let mut ids = HashSet::new();
    let iter = txn
        .iter_changes(view, 0)
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
    for result in iter {
        let (_seq, node_id, _merkle) =
            result.map_err(|e| RepositoryError::Database(e.to_string()))?;
        ids.insert(node_id);
    }
    Ok(ids)
}

/// Collect all change `NodeId`s visible from a view, including parent views.
///
/// For **draft** views this walks the full overlay chain (other draft
/// ancestors) and then the shared ancestor chain, collecting every change
/// that contributes to the view's effective perspective.  This mirrors the
/// filter built inside `get_file_content_via_overlay` and must be used
/// wherever `materialize` needs to decide which vertices are alive.
///
/// For **shared** views this is identical to `collect_view_change_ids`.
///
/// # Why this is needed
///
/// The `change_filter` passed to `materialize_view` controls which graph
/// vertices are considered "alive".  If the filter only contains the draft
/// view's own changes, vertices introduced by the shared `dev` view (the
/// base content) fail the filter and are excluded — producing empty or
/// incomplete file output.
pub fn collect_visible_change_ids<T: ViewTxnT>(
    txn: &T,
    view: &atomic_core::pristine::ViewState,
) -> Result<HashSet<NodeId>, RepositoryError> {
    // Start with the current view's own changes.
    let mut ids = collect_view_change_ids(txn, view)?;

    if view.kind.is_draft() {
        // Include changes from every draft ancestor in the overlay chain.
        let chain = txn
            .resolve_view_chain(view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        for &ancestor_id in &chain {
            if ancestor_id == view.id {
                continue; // already included above
            }
            if let Some(ancestor) = txn
                .get_view_by_id(ancestor_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                let ancestor_ids = collect_view_change_ids(txn, &ancestor)?;
                ids.extend(ancestor_ids);
            }
        }

        // Walk the parent chain past all draft ancestors to find the nearest
        // shared view and include its changes (the global graph base).
        let mut cursor = view.parent;
        while let Some(pid) = cursor {
            let parent = txn
                .get_view_by_id(pid)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            match parent {
                Some(p) if p.kind.is_shared() => {
                    let shared_ids = collect_view_change_ids(txn, &p)?;
                    ids.extend(shared_ids);
                    break;
                }
                Some(p) => cursor = p.parent,
                None => break,
            }
        }
    }

    Ok(ids)
}

/// Information about a view.
///
/// This struct provides metadata about a view including its current
/// Merkle state and the number of changes inserted into it.
#[derive(Debug, Clone)]
pub struct ViewInfo {
    /// The name of the view
    pub name: String,
    /// The current Merkle state (hash of all inserted changes)
    pub state: Merkle,
    /// The number of changes inserted into this view
    pub change_count: u64,
    /// View scope (Draft or Shared)
    pub scope: ViewScope,
    /// Parent view name, if any
    pub parent_name: Option<String>,
}

impl ViewInfo {
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

    /// Check if the view is empty (has no changes).
    pub fn is_empty(&self) -> bool {
        self.change_count == 0
    }

    /// Get a human-readable label for the view scope.
    pub fn kind_label(&self) -> &str {
        match self.scope {
            ViewScope::Shared => "shared",
            ViewScope::Draft => "draft",
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
        let pristine = Arc::new(
            Pristine::open(dot_dir.join("pristine.redb"))
                .map_err(|e| RepositoryError::Database(e.to_string()))?,
        );

        // Create the default view and its workspace directory
        {
            let mut txn = pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.open_or_create_view(DEFAULT_STACK)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }
        ensure_workspace_dir(&dot_dir, DEFAULT_STACK)?;

        // Initialize the change store
        let change_store = ChangeStore::new(dot_dir.join("changes"), DEFAULT_CACHE_CAPACITY)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(Self {
            root,
            dot_dir,
            current_view: DEFAULT_STACK.to_string(),
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

    /// Switch to a different view and update the working copy.
    ///
    /// This is the primary method for switching views. It:
    /// 1. Validates the view exists
    /// 2. Updates the current view pointer
    /// 3. Materializes the working copy to match the new view's state
    ///
    /// # Arguments
    ///
    /// * `view` - The name of the view to switch to
    ///
    /// # Returns
    ///
    /// Statistics about the materialize operation (files written, etc.)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The view does not exist
    /// - The working copy cannot be updated
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut repo = Repository::open(".")?;
    ///
    /// // Switch to feature view and update working copy
    /// let result = repo.switch_view("feature")?;
    /// println!("Updated {} files", result.files_written);
    /// ```
    pub fn switch_view(&mut self, view: &str) -> Result<MaterializeResult, RepositoryError> {
        let old_view_name = self.current_view.clone();

        // Compute files visible on the OLD view.
        let old_files = self.visible_file_paths(&old_view_name)?;

        // Set the current view (validates it exists)
        self.set_current_view(view)?;

        // Compute files visible on the NEW view.
        let new_files = self.visible_file_paths(view)?;

        let working_copy = FileSystem::from_root(&self.root);

        // ── Phase 1: Shelve ignored files into the OLD view's workspace ──
        //
        // Ignored files (node_modules/, dist/, .next/, etc.) are build
        // artifacts that belong to the view that created them.  We move
        // them into `.atomic/workspaces/<old_view>/` so they can be
        // restored when the user switches back.
        //
        // This uses `rename()` which is O(1) on the same filesystem —
        // no data is copied, just inode pointers are updated.
        //
        // The rule:
        //   - Tracked files      → managed by the graph (phases 2-4)
        //   - Untracked, ignored → shelved/restored per-view (phases 1 & 5)
        //   - Untracked, novel   → user's undecided work, left alone
        let old_ws = workspace_path(&self.dot_dir, &old_view_name);
        ensure_workspace_dir(&self.dot_dir, &old_view_name)?;
        let ignored_paths = self.collect_ignored_paths_on_disk();
        if !ignored_paths.is_empty() {
            // Clear old workspace content, then move current ignored files in.
            // We clear first because the workspace may contain stale state
            // from a previous shelve.
            for path in &ignored_paths {
                let ws_dest = old_ws.join(path);
                // Remove stale entry in workspace if it exists
                if ws_dest.is_dir() {
                    let _ = std::fs::remove_dir_all(&ws_dest);
                } else if ws_dest.exists() {
                    let _ = std::fs::remove_file(&ws_dest);
                }
                // Ensure parent dirs exist in workspace
                if let Some(parent) = ws_dest.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                // Move from working copy → workspace (O(1) rename)
                let src = self.root.join(path);
                if src.exists() {
                    let _ = std::fs::rename(&src, &ws_dest);
                }
            }
        }

        // ── Phase 2: Remove tracked files that belong to the old view ──
        //
        // Files visible on the old view but NOT on the new view are
        // removed from disk.
        let mut removed_paths: Vec<String> = Vec::new();
        for path in old_files.difference(&new_files) {
            let abs_path = self.root.join(path);
            if abs_path.exists()
                && !abs_path.is_dir()
                && working_copy.remove_path(path, false).is_ok()
            {
                removed_paths.push(path.clone());
            }
        }

        // ── Phase 3: Clean up empty ancestor directories ────────────────
        let all_removed = removed_paths
            .iter()
            .map(|s| s.as_str())
            .chain(ignored_paths.iter().map(|s| s.as_str()));
        cleanup_empty_ancestors(&self.root, all_removed);

        // ── Phase 4: Materialize the new view's tracked files from graph ─
        let result = self.materialize()?;

        // ── Phase 5: Restore ignored files from the NEW view's workspace ─
        //
        // Move artifacts from `.atomic/workspaces/<new_view>/` back into
        // the working copy.  Again O(1) renames, no data copying.
        let new_ws = workspace_path(&self.dot_dir, view);
        if new_ws.is_dir() {
            self.restore_workspace_to_working_copy(&new_ws);
        }

        Ok(result)
    }

    /// Restore entries from a workspace directory into the working copy.
    ///
    /// Walks the top-level entries in `ws_dir` and moves each into the
    /// project root via `rename()`.  Skips the `.atomic` directory if
    /// present.
    fn restore_workspace_to_working_copy(&self, ws_dir: &Path) {
        let entries = match std::fs::read_dir(ws_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Never move .atomic into the working copy
            if name_str == DOT_DIR {
                continue;
            }

            let src = entry.path();
            let dst = self.root.join(&*name_str);

            // If the destination already exists (e.g. a directory that
            // was created by materialize for tracked content),
            // merge by recursing into it rather than replacing it.
            if dst.is_dir() && src.is_dir() {
                self.merge_dir_into(&src, &dst);
                let _ = std::fs::remove_dir_all(&src);
            } else {
                // Ensure parent exists
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::rename(&src, &dst);
            }
        }
    }

    /// Recursively merge the contents of `src_dir` into `dst_dir`.
    ///
    /// Files in `src_dir` are moved into `dst_dir`.  If a subdirectory
    /// exists in both, the merge recurses.  This is used when restoring
    /// workspace artifacts into a directory that already contains tracked
    /// files (e.g. `src/` might have tracked `.ts` files from the graph
    /// AND ignored `.cache/` from the workspace).
    fn merge_dir_into(&self, src_dir: &Path, dst_dir: &Path) {
        let entries = match std::fs::read_dir(src_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let src = entry.path();
            let dst = dst_dir.join(&name);

            if dst.is_dir() && src.is_dir() {
                self.merge_dir_into(&src, &dst);
                let _ = std::fs::remove_dir_all(&src);
            } else {
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::rename(&src, &dst);
            }
        }
    }

    /// Walk the working copy and collect relative paths of files and
    /// directories that match `.atomicignore` rules.
    ///
    /// Only top-level ignored entries are returned — if `node_modules/`
    /// matches, we return `"node_modules"` rather than enumerating every
    /// file inside it (the caller will `remove_dir_all`).
    ///
    /// Paths that live inside `.atomic/` are never returned.
    fn collect_ignored_paths_on_disk(&self) -> Vec<String> {
        let rules = self.ignore_rules();
        let mut result = Vec::new();

        // Recursive walker that stops descending into ignored directories.
        fn walk(
            root: &Path,
            dir: &Path,
            rules: &crate::ignore::IgnoreRules,
            out: &mut Vec<String>,
        ) {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(_) => return,
            };
            for entry in entries.flatten() {
                let abs = entry.path();
                let rel = match abs.strip_prefix(root) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                // Never touch the .atomic directory itself.
                if rel.starts_with(DOT_DIR) {
                    continue;
                }

                let is_dir = abs.is_dir();

                if rules.is_ignored(rel, is_dir) {
                    // Collect the top-level ignored entry — don't recurse.
                    if let Some(s) = rel.to_str() {
                        out.push(s.to_string());
                    }
                } else if is_dir {
                    // Not ignored — recurse to find ignored children.
                    walk(root, &abs, rules, out);
                }
                // Non-ignored files are left alone.
            }
        }

        walk(&self.root, &self.root, &rules, &mut result);
        result
    }

    /// Compute the set of file paths whose creating change is on a view.
    ///
    /// Visibility is determined by the view's **change log**, not the
    /// overlay chain.  The overlay provides graph-level read access for
    /// record / diff operations, but file *materialization* (what shows
    /// up on disk after `switch_view`) is governed by which changes have
    /// been explicitly inserted into the view.
    ///
    /// A file is visible on a view when:
    /// 1. It appears in the global TREE table (has been `add`ed).
    /// 2. Its inode has a graph position in the INODES table (has been
    ///    `record`ed).
    /// 3. The change that introduced that position is present in the
    ///    view's change log (via `iter_changes`).
    ///
    /// Files that have been `add`ed but not yet `record`ed (no INODES
    /// entry) are NOT returned — they persist across switches as
    /// working-copy state.
    pub fn visible_file_paths(&self, view_name: &str) -> Result<HashSet<String>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = match txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(s) => s,
            None => return Ok(HashSet::new()),
        };

        let view_change_ids = collect_view_change_ids(&txn, &view)?;

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
                if view_change_ids.contains(&position.change) {
                    paths.insert(path);
                }
            }
        }

        Ok(paths)
    }

    /// Materialize the working copy to match the current view's state.
    ///
    /// This synchronizes the working copy files with the repository graph
    /// state for the current view. Files are created, updated, or deleted
    /// to match what's recorded in the view.
    ///
    /// Since all edges are stored in the global GRAPH table, this uses the
    /// raw transaction directly with a change filter to scope which vertices
    /// are alive for this view.
    ///
    /// # Returns
    ///
    /// Statistics about the materialize operation including:
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
    /// // Reset working copy to current view's state
    /// let result = repo.materialize()?;
    /// println!("Materialized {} files", result.files_written);
    ///
    /// if result.has_conflicts() {
    ///     println!("Warning: {} conflicts detected", result.conflict_count());
    /// }
    /// ```
    pub fn materialize(&self) -> Result<MaterializeResult, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the current view
        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        // Collect all change NodeIds visible from the current view for the
        // change_filter.  For draft views this includes the current view's
        // own changes PLUS all changes from the overlay chain (other draft
        // ancestors) and the nearest shared ancestor (e.g. dev).
        //
        // Without the parent-chain changes, vertices introduced by dev (the
        // base content) fail the filter and materialize_view skips them,
        // producing empty or duplicated file content on the draft view.
        let change_filter = collect_visible_change_ids(&txn, &view)?;

        let working_copy = FileSystem::from_root(&self.root);
        let options = MaterializeOptions::new().with_change_filter(change_filter);

        // Use the raw transaction for graph reads. All edges are now in the
        // global GRAPH table, and the change_filter controls which vertices
        // are considered alive for this view.
        let result = materialize_view(&txn, &self.change_store, &working_copy, options)
            .map_err(|e| RepositoryError::Output(format!("{}", e)))?;

        Ok(result)
    }

    /// Materialize the working copy for a specific prefix only.
    ///
    /// This is useful for partial updates when you only want to sync
    /// a subset of files.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Path prefix to materialize (e.g., "src/")
    ///
    /// # Returns
    ///
    /// Statistics about the materialize operation.
    pub fn materialize_prefix(&self, prefix: &str) -> Result<MaterializeResult, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the current view for the change filter
        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        let change_filter = collect_visible_change_ids(&txn, &view)?;

        let working_copy = FileSystem::from_root(&self.root);
        let options = MaterializeOptions::new()
            .prefix(prefix)
            .with_change_filter(change_filter);

        let result = materialize_view(&txn, &self.change_store, &working_copy, options)
            .map_err(|e| RepositoryError::Output(format!("{}", e)))?;

        Ok(result)
    }

    /// Create a new view.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the view to create
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The view already exists
    /// - The database operation fails
    pub fn create_view(&mut self, name: &str) -> Result<(), RepositoryError> {
        // Create the workspace directory for this view.
        ensure_workspace_dir(&self.dot_dir, name)?;

        // Create a **Draft** view parented on the nearest Shared
        // ancestor of the current view.  The change log starts EMPTY —
        // no changes are inherited automatically.
        //
        // The parent link gives the view read-access to the shared
        // graph content (via the overlay chain) so that `record` can
        // compute diffs against the existing state.  But no files are
        // *materialised* on disk until changes are explicitly inserted
        // into this view (which copies them into the view's change log).
        //
        // This means:
        //   `view new feature`              → empty workspace, no files
        //   `insert from-view dev feature`  → inherits dev's files
        //
        // Using the nearest Shared ancestor (instead of the current
        // view directly) prevents sibling Draft views from seeing
        // each other's edges through the overlay chain.
        let parent_name = self.nearest_shared_ancestor(&self.current_view.clone())?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some()
        {
            return Err(RepositoryError::ViewAlreadyExists {
                name: name.to_string(),
            });
        }

        let parent_view = txn
            .get_view(&parent_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: parent_name.clone(),
            })?;

        txn.create_view(name, ViewScope::Draft, Some(parent_view.id))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Create a new Shared view with no parent.
    ///
    /// Changes inserted into a Shared view go into the global `GRAPH` table
    /// and are permanently visible to all views. This is the correct scope
    /// to use for server-side push targets, where changes must be universally
    /// visible regardless of which run or request inserts them.
    ///
    /// Returns `ViewAlreadyExists` if the view already exists.
    pub fn create_shared_view(&mut self, name: &str) -> Result<(), RepositoryError> {
        ensure_workspace_dir(&self.dot_dir, name)?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some()
        {
            return Err(RepositoryError::ViewAlreadyExists {
                name: name.to_string(),
            });
        }

        txn.create_view(name, ViewScope::Shared, None)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Walk the parent chain from `view_name` and return the name of the
    /// first Shared view encountered.  If `view_name` is itself Shared,
    /// it is returned immediately.  This is used to determine the correct
    /// parent for newly created Draft views.
    pub fn nearest_shared_ancestor(&self, view_name: &str) -> Result<String, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        // Already Shared → use it directly.
        if view.kind.is_shared() {
            return Ok(view_name.to_string());
        }

        // Walk up the parent chain looking for a Shared ancestor.
        let mut cursor = view.parent;
        while let Some(parent_id) = cursor {
            if let Some(parent) = txn
                .get_view_by_id(parent_id)
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
        // normal use — dev is always Shared), use the current view.
        Ok(view_name.to_string())
    }

    /// Create a new view that inherits changes from another view.
    ///
    /// This creates a new view and copies all changes from the source view
    /// to the new view. The new view will have the same content state as
    /// the source view at the time of creation.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the new view to create
    /// * `from_view` - The name of the view to inherit changes from
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The new view already exists
    /// - The source view does not exist
    /// - The database operation fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Create a feature view that starts with dev's changes
    /// repo.create_view_from("feature", "dev")?;
    /// ```
    pub fn create_view_from(&mut self, name: &str, from_view: &str) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if the new view already exists
        if txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some()
        {
            return Err(RepositoryError::ViewAlreadyExists {
                name: name.to_string(),
            });
        }

        // Get the source view
        let source_view = txn
            .get_view(from_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: from_view.to_string(),
            })?;

        let source_id = source_view.id;

        // Collect all changes from the source view
        let changes: Vec<(NodeId, Hash)> = {
            let iter = txn
                .iter_changes(&source_view, 0)
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

        // Create the new view as a **Draft** view parented on the
        // source view.  Draft views write edges to GRAPH like all
        // views, but use a change filter for isolation. The parent
        // link means the view chain includes the source's content.
        // Create workspace directory for the new view.
        ensure_workspace_dir(&self.dot_dir, name)?;

        let mut new_view = txn
            .create_view(name, ViewScope::Draft, Some(source_id))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Copy all changes from the source to the new view's log.
        // This does NOT re-insert hunks — the edges already exist in
        // GRAPH. The new view sees them via the change filter.
        for (node_id, hash) in changes {
            txn.put_change(&mut new_view, node_id, &hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Update the view state
        txn.update_view(&new_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// List all views in the repository.
    ///
    /// # Returns
    ///
    /// A vector of view names.
    pub fn list_views(&self) -> Result<Vec<String>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.list_views()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Check if a view exists.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the view to check
    pub fn view_exists(&self, name: &str) -> Result<bool, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .is_some())
    }

    /// Delete a view from the repository.
    ///
    /// This removes the view and all its associated metadata, but does not
    /// delete the changes themselves. Changes remain in the graph and may be
    /// referenced by other views.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the view to delete
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The view does not exist
    /// - The view is the current view (cannot delete current view)
    /// - The database operation fails
    pub fn delete_view(&mut self, name: &str) -> Result<(), RepositoryError> {
        // Cannot delete the current view
        if name == self.current_view {
            return Err(RepositoryError::CannotDeleteCurrentView {
                name: name.to_string(),
            });
        }

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the view to delete
        let view = txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;

        // Delete the view.
        //
        // `del_view` enforces:
        // - Shared views cannot be deleted (returns CannotDeleteSharedView)
        // - Views with children cannot be deleted (returns ViewHasChildren)
        // Remove workspace directory for this view before deleting
        // the view from the database.  This cleans up any shelved
        // artifacts (node_modules, dist, etc.) that were stored when
        // the user last switched away from this view.
        let ws = workspace_path(&self.dot_dir, name);
        if ws.is_dir() {
            let _ = std::fs::remove_dir_all(&ws);
        }

        txn.del_view(&view).map_err(|e| match &e {
            atomic_core::pristine::PristineError::CannotDeleteSharedView { name } => {
                RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot delete shared view '{}': shared views are permanent. \
                         Use 'view new' to create a draft view instead.",
                        name
                    ),
                }
            }
            atomic_core::pristine::PristineError::ViewHasChildren { name, children } => {
                RepositoryError::InvalidOperation {
                    message: format!(
                        "cannot delete view '{}': has child views ({}). \
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

    /// Get information about a view.
    ///
    /// Returns the view's metadata including its Merkle state and change count.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the view to query
    ///
    /// # Returns
    ///
    /// A `ViewInfo` struct with the view's metadata, or an error if the view
    /// doesn't exist.
    pub fn get_view_info(&self, name: &str) -> Result<ViewInfo, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;

        // Resolve parent name if the view has a parent
        let parent_name = if let Some(parent_id) = view.parent {
            txn.get_view_by_id(parent_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .map(|p| p.name)
        } else {
            None
        };

        Ok(ViewInfo {
            name: view.name.clone(),
            state: view.state,
            change_count: view.change_count,
            scope: view.kind,
            parent_name,
        })
    }

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

#[cfg(test)]
mod tests;

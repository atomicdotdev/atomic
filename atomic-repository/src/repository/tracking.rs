use super::*;

// Preview and mutation select the same paths, using the caller's transaction.
fn removal_candidates<T: TreeTxnT>(
    txn: &T,
    path: &Path,
    options: &TrackingOptions,
) -> Result<Vec<(String, Inode, TrackedPathState)>, RepositoryError> {
    let normalized = normalize_path(path);
    let files = if options.recursive {
        tracked_under_prefix(txn, &normalized)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
    } else {
        get_inode(txn, &normalized)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .map(|inode| vec![(normalized, inode)])
            .unwrap_or_default()
    };
    if files.is_empty() && !options.force {
        return Err(RepositoryError::FileNotTracked {
            path: path.to_path_buf(),
        });
    }
    files
        .into_iter()
        .map(|(candidate, inode)| {
            tracked_path_state(txn, inode)
                .map(|state| (candidate, inode, state))
                .map_err(|e| RepositoryError::Database(e.to_string()))
        })
        .collect()
}

// View-scoped pending-untrack intent (the `atomic remove --keep` marker).
// The list is tiny and always handled read-modify-write inside the caller's
// transaction. This is intent state, not a projection: nothing derives it
// and native-index repair must never clear it.
fn mark_pending_untrack<T>(txn: &mut T, view_id: u64, path: &str) -> Result<(), RepositoryError>
where
    T: atomic_core::pristine::ViewTxnT + atomic_core::pristine::MutTxnT,
{
    let mut paths = txn
        .pending_untracks(view_id)
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
    if !paths.iter().any(|existing| existing == path) {
        paths.push(path.to_string());
        paths.sort();
    }
    txn.put_pending_untracks(view_id, &paths)
        .map_err(|e| RepositoryError::Database(e.to_string()))
}

/// Cancels pending-untrack intent for anything at or under `roots` on the
/// current view — the `git add`-cancels-a-staged-removal behavior: re-tracking
/// is deliberate and supersedes a mark that has not been recorded yet. Markers
/// live on still-tracked paths, so `add` reaches this even when the path itself
/// is already tracked and would otherwise be skipped.
fn cancel_pending_untracks<T>(
    txn: &mut T,
    current_view: &str,
    roots: &[String],
) -> Result<(), RepositoryError>
where
    T: atomic_core::pristine::ViewTxnT + atomic_core::pristine::MutTxnT,
{
    let Some(view) = txn
        .get_view(current_view)
        .map_err(|e| RepositoryError::Database(e.to_string()))?
    else {
        return Ok(());
    };
    let existing = txn
        .pending_untracks(view.id)
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
    if existing.is_empty() {
        return Ok(());
    }
    let kept: Vec<String> = existing
        .iter()
        .filter(|pending| {
            !roots
                .iter()
                .any(|root| *pending == root || pending.starts_with(&format!("{root}/")))
        })
        .cloned()
        .collect();
    if kept.len() != existing.len() {
        txn.put_pending_untracks(view.id, &kept)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
    }
    Ok(())
}

impl Repository {
    /// Drop pending-untrack markers for `view_name`, keeping any marker not in
    /// `paths`. Called by record after apply (consuming what it observed) —
    /// see the marker snapshot in `record_with_lifecycle`.
    pub(crate) fn clear_pending_untracks(
        &self,
        view_name: &str,
        paths: &[String],
    ) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let Some(view) = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            return Ok(());
        };
        let current = txn
            .pending_untracks(view.id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let kept: Vec<String> = current
            .iter()
            .filter(|pending| !paths.iter().any(|dropped| dropped == *pending))
            .cloned()
            .collect();
        if kept.len() != current.len() {
            txn.put_pending_untracks(view.id, &kept)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    // File Tracking Methods

    /// Add a file or directory to tracking.
    ///
    /// This registers the file with the repository so it will be included
    /// in future changes. Adding a file does NOT create a change - you need
    /// to call `record()` for that.
    ///
    /// # Arguments
    ///
    /// * `working_copy` - Identity of the physical working copy being inspected
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
    /// repo.add(working_copy, "src/main.rs", TrackingOptions::default())?;
    ///
    /// // Add a directory recursively
    /// repo.add(working_copy, "src/", TrackingOptions::default())?;
    ///
    /// // Add without recursion
    /// repo.add(working_copy, "src/", TrackingOptions::non_recursive())?;
    /// ```
    pub fn add<P: AsRef<Path>>(
        &self,
        working_copy: WorkingCopyId,
        path: P,
        options: TrackingOptions,
    ) -> Result<TrackingStats, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let path = path.as_ref();
        let mut stats = TrackingStats::new();

        // Load ignore rules
        let rules = self.load_ignore_rules();

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
                stats.skip(file_path, "already tracked");
                continue;
            }

            // Add to tree (only files, not directories)
            add_to_tree(&mut txn, &normalized, false)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            stats.files_added += 1;
        }

        // Re-tracking supersedes any pending `--keep` mark at or under the
        // requested path, so a marked path that gets `add`ed again cancels the
        // mark (the git `add`-unstages-a-removal behavior) even when the path
        // was already tracked and skipped above.
        cancel_pending_untracks(&mut txn, &self.current_view, &[normalize_path(path)])?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(stats)
    }

    /// Add multiple files to tracking in a single write transaction.
    ///
    /// This is much faster than calling `add()` in a loop because it avoids
    /// opening a separate write transaction (and fsync) for each file.
    /// For git import of a commit adding 20 files, this reduces from 20
    /// fsyncs to 1.
    ///
    /// # Arguments
    ///
    /// * `working_copy` - Identity of the physical working copy being updated
    /// * `paths` - Paths to add (relative to repository root)
    ///
    /// # Returns
    ///
    /// Number of files actually added (skips already-tracked files).
    pub fn add_batch(
        &self,
        working_copy: WorkingCopyId,
        paths: &[&str],
    ) -> Result<usize, RepositoryError> {
        use crate::tracking::{TreeProjectionKind, TreeProjectionOperation, TreeProjectionPlan};
        self.validate_working_copy(working_copy)?;
        if paths.is_empty() {
            return Ok(0);
        }

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Performance (RFC §21 measured budgets, CB-13C AC-3): plan the
        // whole batch as ONE tree projection. `TreeProjectionPlan::plan`
        // derives directory occupancy from a FULL `iter_tree` scan, so
        // planning per path rescans the entire TREE table once per file —
        // O(n²) across a batch (measured: a 10k-file import spent minutes
        // here). Planning once keeps the scan linear in the final tree.
        let mut operations: Vec<TreeProjectionOperation> = Vec::with_capacity(paths.len());
        let mut planned: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut count = 0usize;
        for path in paths {
            let normalized = normalize_path(Path::new(path));
            if planned.contains(&normalized) {
                continue;
            }
            if is_tracked(&txn, &normalized)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                continue;
            }
            let inode = txn
                .alloc_inode()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            operations.push(TreeProjectionOperation::Add {
                inode,
                path: Some(normalized.clone()),
                position: None,
                kind: TreeProjectionKind::File,
            });
            planned.insert(normalized);
            count += 1;
        }

        TreeProjectionPlan::plan(&txn, operations)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .apply(&mut txn)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(count)
    }

    /// Remove multiple files from tracking in a single write transaction.
    ///
    /// This is much faster than calling `remove()` in a loop because it
    /// avoids a separate write transaction (and fsync) for each file.
    ///
    /// # Arguments
    ///
    /// * `working_copy` - Identity of the physical working copy being updated
    /// * `paths` - Paths to remove (relative to repository root)
    ///
    /// # Returns
    ///
    /// Number of files actually removed.
    pub fn remove_batch(
        &self,
        working_copy: WorkingCopyId,
        paths: &[&str],
    ) -> Result<usize, RepositoryError> {
        use crate::tracking::{TreeProjectionOperation, TreeProjectionPlan};
        self.validate_working_copy(working_copy)?;
        if paths.is_empty() {
            return Ok(0);
        }

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Performance: one tree projection for the whole batch (see
        // add_batch — per-path planning rescans the full TREE table).
        let mut operations: Vec<TreeProjectionOperation> = Vec::with_capacity(paths.len());
        let mut planned: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut count = 0usize;
        for path in paths {
            let normalized = normalize_path(Path::new(path));
            if planned.contains(&normalized) {
                continue;
            }
            planned.insert(normalized.clone());
            if let Some(inode) = crate::tracking::get_inode(&txn, &normalized)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                // Recorded bindings belong to their change (see `remove`); plan
                // only the staged ones so the batch never orphans a live claim.
                if tracked_path_state(&txn, inode)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    == TrackedPathState::Recorded
                {
                    continue;
                }
                operations.push(TreeProjectionOperation::Delete {
                    inode,
                    retire: true,
                });
                count += 1;
            }
        }

        TreeProjectionPlan::plan(&txn, operations)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .apply(&mut txn)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(count)
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
    /// * `working_copy` - Identity of the physical working copy being inspected
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
    /// repo.add_directory(
    ///     working_copy,
    ///     "src/empty_module/",
    ///     TrackingOptions::default(),
    /// )?;
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
        working_copy: WorkingCopyId,
        path: P,
        options: TrackingOptions,
    ) -> Result<TrackingStats, RepositoryError> {
        use crate::tracking::add_directory_to_tree;

        self.validate_working_copy(working_copy)?;
        let path = path.as_ref();
        let mut stats = TrackingStats::new();

        // Load ignore rules
        let rules = self.load_ignore_rules();

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
    /// Only a **staged** path — one `add` tracked but `record` never recorded —
    /// is untracked here, by deleting the `TREE`/`REV_TREE` rows `add` created.
    /// Nothing is deleted from disk; that is the caller's decision.
    ///
    /// A **recorded** path cannot be untracked this way. Its name is durable in
    /// `PATH_CLAIMS` and its `INODES`/`REV_INODES` binding is derived from the
    /// change that recorded it, so wiping the binding would leave the surviving
    /// claim unresolvable and every later projection (`status`, `record`,
    /// `switch`, materialize) would fail with `path claim '<path>' has no inode
    /// for graph position <pos>` until `atomic doctor repair-native-indexes`
    /// rebuilt the rows. Such paths are reported untouched in
    /// [`TrackingStats::pending_deletions`]; removing them from the working copy
    /// and recording the delete is what actually stops tracking them, which is
    /// what `atomic remove` does for them, mirroring `git rm`.
    ///
    /// # Arguments
    ///
    /// * `working_copy` - Identity of the physical working copy being updated
    /// * `path` - Path to remove from tracking
    /// * `options` - Options controlling the remove operation
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Untrack a single staged file
    /// let stats = repo.remove(working_copy, "old_file.txt", TrackingOptions::default())?;
    /// assert_eq!(stats.files_removed, 1);
    ///
    /// // A recorded path is deferred, not untracked
    /// let stats = repo.remove(working_copy, "Cargo.lock", TrackingOptions::default())?;
    /// assert_eq!(stats.files_removed, 0);
    /// assert!(stats.has_pending_deletions());
    /// ```
    pub fn remove<P: AsRef<Path>>(
        &self,
        working_copy: WorkingCopyId,
        path: P,
        options: TrackingOptions,
    ) -> Result<TrackingStats, RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let path = path.as_ref();
        let mut stats = TrackingStats::new();
        if options.dry_run {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            // Preview reports the same split the mutation applies, so a dry run
            // never promises an untrack that the recorder owns.
            for (file_path, _inode, state) in removal_candidates(&txn, path, &options)? {
                match state {
                    TrackedPathState::Staged => stats.files_removed += 1,
                    TrackedPathState::Recorded => {
                        if options.keep_recorded {
                            stats.pending_untracks.push(PathBuf::from(file_path));
                        } else {
                            stats.defer_deletion(PathBuf::from(file_path));
                        }
                    }
                }
            }
            return Ok(stats);
        }
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let to_remove = removal_candidates(&txn, path, &options)?;
        // Resolve the marked view once, and only when this request will mark
        // something: `--keep` stages the untrack as per-view intent.
        let mark_view = if options.keep_recorded
            && to_remove
                .iter()
                .any(|(_, _, state)| matches!(state, TrackedPathState::Recorded))
        {
            Some(
                txn.get_view(&self.current_view)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: self.current_view.clone(),
                    })?
                    .id,
            )
        } else {
            None
        };

        for (file_path, _inode, state) in to_remove {
            match state {
                TrackedPathState::Staged => {
                    untrack_staged_path(&mut txn, &file_path)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    stats.files_removed += 1;
                }
                TrackedPathState::Recorded => {
                    if let (true, Some(view_id)) = (options.keep_recorded, mark_view) {
                        mark_pending_untrack(&mut txn, view_id, &file_path)?;
                        stats.pending_untracks.push(PathBuf::from(file_path));
                    } else {
                        stats.defer_deletion(PathBuf::from(file_path));
                    }
                }
            }
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
    /// * `working_copy` - Identity of the physical working copy being updated
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
    /// repo.move_file(working_copy, "old_name.rs", "new_name.rs")?;
    /// ```
    pub fn move_file<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        working_copy: WorkingCopyId,
        from: P,
        to: Q,
    ) -> Result<Inode, RepositoryError> {
        self.validate_working_copy(working_copy)?;
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

    /// Remove a file from the FILE_INDEX.
    ///
    /// Call this when a file is deleted (e.g., during git import cleanup)
    /// so that `status` doesn't show it as a stale entry.
    pub fn del_file_index(
        &self,
        working_copy: WorkingCopyId,
        path: &str,
    ) -> Result<(), RepositoryError> {
        self.validate_working_copy(working_copy)?;
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let normalized = path.replace('\\', "/");
        txn.del_working_copy_file_index(working_copy, &normalized)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Remove multiple files from the FILE_INDEX in a single write transaction.
    ///
    /// This is much faster than calling `del_file_index()` in a loop because
    /// it avoids a separate write transaction (and fsync) for each file.
    ///
    /// # Arguments
    ///
    /// * `working_copy` - Identity of the physical working copy owning the index
    /// * `paths` - Paths to remove from the index
    pub fn del_file_index_batch(
        &self,
        working_copy: WorkingCopyId,
        paths: &[&str],
    ) -> Result<(), RepositoryError> {
        self.validate_working_copy(working_copy)?;
        if paths.is_empty() {
            return Ok(());
        }

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for path in paths {
            let normalized = path.replace('\\', "/");
            let _ = txn.del_working_copy_file_index(working_copy, &normalized);
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Store mtime + size + content hash for a batch of files in a single transaction.
    ///
    /// This populates the file index so that `status` can compare
    /// file metadata instead of reconstructing graph content for every
    /// file.  Call this after git import (or any bulk write that puts
    /// files on disk without going through `record`).
    ///
    /// # Arguments
    ///
    /// * `working_copy` - Identity of the physical working copy owning the index
    /// * `files` - Slice of `(path, mtime_secs, mtime_nanos, file_size, content_hash)` tuples.
    ///   Paths should be repo-relative with forward slashes.
    pub fn update_file_index(
        &self,
        working_copy: WorkingCopyId,
        files: &[(String, i64, u32, u64, Hash)],
    ) -> Result<(), RepositoryError> {
        self.validate_working_copy(working_copy)?;
        if files.is_empty() {
            return Ok(());
        }

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for (path, secs, nanos, size, hash) in files {
            txn.put_working_copy_file_index(working_copy, path, *secs, *nanos, *size, hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(())
    }

    /// Rebuild the FILE_INDEX from the current working copy.
    ///
    /// Walks every tracked file, stats it on disk, hashes its content,
    /// and stores (mtime, size, content_hash) in the FILE_INDEX.  After
    /// this call, `status` can use the fast stat-comparison path instead
    /// of reconstructing graph content for every file.
    ///
    /// This is essential after `git import` where `restore_from_git`
    /// resets all file mtimes, invalidating any FILE_INDEX entries
    /// written during batch processing.
    ///
    /// # Returns
    ///
    /// The number of files indexed.
    pub fn reindex_working_copy(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<usize, RepositoryError> {
        use std::time::SystemTime;

        self.validate_working_copy(working_copy)?;
        let tracked = self.list_tracked_files().unwrap_or_default();
        let repo_root = self.root.clone();

        let mut entries: Vec<(String, i64, u32, u64, Hash)> = Vec::with_capacity(tracked.len());

        for file in &tracked {
            let abs = repo_root.join(&file.path);
            if !abs.exists() || abs.is_dir() {
                continue;
            }

            let metadata = match std::fs::metadata(&abs) {
                Ok(m) => m,
                Err(_) => continue,
            };

            let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let duration = mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            let secs = duration.as_secs() as i64;
            let nanos = duration.subsec_nanos();
            let size = metadata.len();

            let content_hash = match std::fs::read(&abs) {
                Ok(bytes) => Hash::of(&bytes),
                Err(_) => continue,
            };

            let path_str = file.path.to_string_lossy().replace('\\', "/");
            entries.push((path_str, secs, nanos, size, content_hash));
        }

        let count = entries.len();

        // Write in batches of 5000 to avoid holding the write txn too long
        for chunk in entries.chunks(5000) {
            self.update_file_index(working_copy, chunk)?;
        }

        Ok(count)
    }

    /// List tracked files under a given path prefix.
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
}

#[cfg(test)]
mod batch_scaling_tests {

    use crate::repository::Repository;
    use tempfile::TempDir;

    /// RFC §21 measured-budget regression (CB-13C AC-3): `add_batch` plans
    /// the whole batch as ONE tree projection. Planning per path re-derives
    /// directory occupancy from a full `iter_tree` scan per file, which is
    /// O(n²) across a batch — the measured 10k-file import spent minutes
    /// here and a 100k-file corpus would take hours. The batched plan keeps
    /// the scan linear; 4000 paths land well under the bound while the
    /// per-path shape overruns it by an order of magnitude in debug.
    #[test]
    fn add_batch_plans_one_tree_projection_not_one_per_path() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();

        let paths: Vec<String> = (0..4000)
            .map(|i| format!("dir-{:03}/file-{:05}.txt", i / 500, i))
            .collect();
        let refs: Vec<&str> = paths.iter().map(|p| p.as_str()).collect();

        let started = std::time::Instant::now();
        let added = repo.add_batch(working_copy, &refs).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(added, 4000);
        // Every path must actually be tracked (the batched plan applied all
        // of the adds).
        let txn = repo.pristine().read_txn().unwrap();
        for path in &paths {
            assert!(
                crate::tracking::is_tracked(&txn, path).unwrap(),
                "'{path}' must be tracked after add_batch"
            );
        }
        assert!(
            elapsed.as_secs() < 10,
            "add_batch of 4000 paths took {elapsed:?}; the one-projection batch \
             must stay linear (a per-path plan scan overruns this bound)"
        );

        // remove_batch shares the shape: one projection for the whole batch.
        let started = std::time::Instant::now();
        let removed = repo.remove_batch(working_copy, &refs).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(removed, 4000);
        assert!(
            elapsed.as_secs() < 10,
            "remove_batch of 4000 paths took {elapsed:?}; must stay linear"
        );
    }
}

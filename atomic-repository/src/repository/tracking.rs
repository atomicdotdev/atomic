use super::*;

impl Repository {
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
    /// This removes the file from version control tracking. It does NOT
    /// delete the file from disk.
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
    /// // Remove a single file
    /// repo.remove(working_copy, "old_file.txt", TrackingOptions::default())?;
    ///
    /// // Remove a directory recursively
    /// repo.remove(working_copy, "old_dir/", TrackingOptions::default())?;
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
    use super::*;
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

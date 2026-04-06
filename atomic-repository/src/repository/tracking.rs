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

        // Build the set of change NodeIds visible on the current stack so we
        // can apply the same stack-aware filter that `status` uses.  A file
        // that lives in the global TREE table but whose creating change belongs
        // to a *different* stack is invisible here and should be re-addable.
        let current_stack_change_ids: std::collections::HashSet<atomic_core::types::NodeId> =
            if let Ok(Some(stack)) = txn.get_stack(&self.current_stack) {
                collect_stack_change_ids(&txn, &stack).unwrap_or_default()
            } else {
                std::collections::HashSet::new()
            };

        for file_path in files {
            // Normalize path with repo root to handle absolute paths correctly
            // (e.g., on macOS where /tmp -> /private/tmp)
            let normalized = normalize_path_with_root(&file_path, Some(&self.root));
            let abs_path = self.root.join(&file_path);

            // Skip directories - they are implicitly tracked through their contents
            if abs_path.is_dir() {
                continue;
            }

            // Check if already tracked — with stack-aware filtering.
            //
            // A file is considered "tracked on this stack" only if:
            //   (a) it exists in the global TREE table, AND
            //   (b) either it hasn't been recorded yet (no INODES position),
            //       OR its creating change belongs to the current stack.
            //
            // If (a) is true but (b) is false, the file was recorded on a
            // different stack (e.g. an agent stack).  `status` shows it as
            // untracked here, so `add` must also treat it as untracked and
            // allow re-adding it.
            if is_tracked(&txn, &normalized)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                // File is in TREE — now check if it belongs to this stack.
                let on_current_stack = if let Ok(Some(inode)) = txn.get_inode(&normalized) {
                    match txn.inode_position(inode) {
                        Ok(Some(position)) => {
                            // Recorded: check if the creating change is on this stack.
                            position.change.is_root()
                                || current_stack_change_ids.contains(&position.change)
                        }
                        Ok(None) => true, // Added but not yet recorded — belongs here.
                        Err(_) => true,   // Can't determine — be conservative, keep as tracked.
                    }
                } else {
                    true // Can't look up inode — be conservative.
                };

                if on_current_stack {
                    stats.skip(file_path, "already tracked");
                    continue;
                }
                // Falls through: file is in TREE but on a different stack.
                // Treat as untracked on this stack and allow re-adding.
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
}

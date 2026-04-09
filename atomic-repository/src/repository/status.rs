use super::*;

impl Repository {
    // Status Methods

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
        // Get the current view state
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view_state = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .map(|s| s.state);

        let mut status = RepositoryStatus::new(self.current_view.clone(), view_state);

        // ── View-aware filtering ───────────────────────────────────────
        // Collect every change NodeId that belongs to the current view,
        // PLUS all of their transitive dependencies (via the DEPS table).
        //
        // Why dependencies?  After a content revise, the view log has A'
        // (the revised change) but NOT A (the original).  A' depends on A
        // because its hunks reference A's graph vertices.  The INODES
        // position for the file still points to A's NodeId (which created
        // the inode vertex).  Without including dependencies, the status
        // view filter would see A's NodeId as "not on this view" and
        // hide the file — even though A' (which superseded A) IS on the
        // view.
        let current_view_change_ids: HashSet<NodeId> = if let Some(ref view) = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            let mut ids = collect_view_change_ids(&txn, view)?;

            // Expand with dependencies from change FILES (not the DEPS
            // table, which is for attestations).  After a content revise,
            // the view has A' but not A.  A' depends on A (its hunks
            // reference A's vertices).  Without including A's NodeId,
            // the status filter would hide files introduced by A.
            let direct_ids: Vec<NodeId> = ids.iter().copied().collect();
            for node_id in direct_ids {
                if let Ok(Some(hash)) = txn.get_external(node_id) {
                    if let Ok(change) = self.load_change(&hash) {
                        for dep_hash in change.dependencies() {
                            if let Ok(Some(dep_id)) = txn.get_internal(dep_hash) {
                                ids.insert(dep_id);
                            }
                        }
                    }
                }
            }

            ids
        } else {
            HashSet::new()
        };

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
        // We also normalize paths to handle any incorrectly stored absolute paths.
        //
        // View-aware filtering: a file that has been recorded (has an
        // INODES position) but whose creating change is NOT on the current
        // view is excluded from tracked_paths.  This prevents files
        // recorded on other views from appearing in status.  Files that
        // have been `add`ed but not yet recorded (no INODES position) are
        // kept — they are pending working-copy state.
        let mut tracked_paths: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();

        for result in tracked_files {
            let (path, inode) = result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            // ── View filter ────────────────────────────────────────────
            // If this file has been recorded (has INODES position), check
            // whether the creating change is on the current view.  If
            // not, skip it entirely — it belongs to another view.
            if let Ok(Some(position)) = txn.inode_position(inode) {
                if !position.change.is_root() && !current_view_change_ids.contains(&position.change)
                {
                    // File is recorded on a different view — invisible here
                    continue;
                }
            }
            // Files with no INODES position (added, not yet recorded) pass through.

            let path_buf = PathBuf::from(&path);

            // Normalize: if the path is absolute and starts with the repo root,
            // convert it to a relative path. This handles cases where paths were
            // incorrectly stored with absolute paths (e.g., on macOS where /tmp
            // resolves to /private/tmp).
            let stripped = if path_buf.is_absolute() {
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

            // Normalize to forward slashes for consistent comparison with
            // disk paths (also normalized to '/') on all platforms.
            let normalized_path = PathBuf::from(stripped.to_string_lossy().replace('\\', "/"));

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

            // Normalize to forward slashes for consistent comparison with
            // disk paths (also normalized to '/') on all platforms.
            let normalized_path =
                PathBuf::from(normalized_path.to_string_lossy().replace('\\', "/"));

            // Use the original path for database lookup since that's what's stored.
            // Apply the same view-awareness filter here: skip inodes whose
            // creating change is not on the current view.
            if let Ok(Some(inode)) = txn.get_inode(&original_path) {
                // View filter (mirrors the tracked_paths filter above)
                if let Ok(Some(position)) = txn.inode_position(inode) {
                    if !position.change.is_root()
                        && !current_view_change_ids.contains(&position.change)
                    {
                        continue;
                    }
                }
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
                    // Fast path: check filesystem mtime + size against cached values.
                    // If they match, the file hasn't been modified since the last record,
                    // and we can skip the expensive graph content reconstruction entirely.
                    // This reduces incremental status from O(files × graph_size) to O(files × stat).
                    let mut mtime_matched = false;

                    if has_graph_content {
                        if let Ok(metadata) = std::fs::metadata(&abs_path) {
                            use std::time::SystemTime;
                            let current_mtime =
                                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                            let current_size = metadata.len();

                            // Convert to (secs, nanos) for comparison
                            let duration = current_mtime
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap_or_default();
                            let current_secs = duration.as_secs() as i64;
                            let current_nanos = duration.subsec_nanos();

                            // Check the mtime cache
                            let path_str = path.to_string_lossy();
                            if let Ok(Some((cached_secs, cached_nanos, cached_size))) =
                                txn.get_file_mtime(&path_str)
                            {
                                if current_secs == cached_secs
                                    && current_nanos == cached_nanos
                                    && current_size == cached_size
                                {
                                    // mtime + size match — file hasn't changed.
                                    // Keep as Clean, skip the expensive content comparison.
                                    mtime_matched = true;
                                }
                            }
                        }
                    }

                    if !mtime_matched {
                        // Slow path: hash the working copy file and compare with graph content.
                        match hash_file_contents(&abs_path) {
                            Ok(current_hash) => {
                                entry.set_current_hash(current_hash);

                                // If file has graph content, compare with recorded content
                                if has_graph_content {
                                    // Retrieve the recorded content from the graph and hash it.
                                    // Use get_file_content which builds a change filter
                                    // so that the view's content is correctly scoped.
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
                                            // No recorded content retrieved.
                                            // If the file has graph content but retrieval returned None,
                                            // this indicates a retrieval issue - mark as Modified to be safe.
                                            // This ensures git import doesn't miss changes when content
                                            // retrieval fails (e.g., due to change filter issues).
                                            if has_graph_content {
                                                let is_empty_file = std::fs::metadata(&abs_path)
                                                    .map(|m| m.len() == 0)
                                                    .unwrap_or(false);
                                                if !is_empty_file {
                                                    // File has graph content but retrieval failed - treat as modified
                                                    entry = FileStatusEntry::new(
                                                        path.clone(),
                                                        FileStatus::Modified,
                                                    );
                                                    if let Some(inode) = inode {
                                                        entry.set_inode(inode);
                                                    }
                                                    entry.set_current_hash(current_hash);
                                                    entry.set_details(
                                                        "Content retrieval failed".to_string(),
                                                    );
                                                }
                                            }
                                            // For files without graph content, keep as Clean
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
                                if has_graph_content {
                                    entry =
                                        FileStatusEntry::new(path.clone(), FileStatus::Modified);
                                    if let Some(inode) = inode {
                                        entry.set_inode(inode);
                                    }
                                    entry.set_details("Unable to read file contents".to_string());
                                }
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
}

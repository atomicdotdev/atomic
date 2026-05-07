use super::*;

impl Repository {
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

        // Populate the mtime cache for all written files so that subsequent
        // `status` calls can compare file metadata (stat) instead of
        // reconstructing graph content.  This makes status after materialize
        // or server-side push instantaneous.
        self.populate_file_index(&result);

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
    /// Populate the file index for all tracked files after a materialize.
    ///
    /// Stats each tracked file from disk and stores its mtime + size +
    /// content hash in the pristine database. Errors are silently ignored
    /// (best-effort).
    ///
    /// Why we ignore `result.file_results` and walk the tracked set: the
    /// `MaterializeResult.file_results` map is only populated when
    /// `merge_file_result(_, store_result=true)` is called, and the
    /// `materialize_view` call site in `atomic-core` passes `false`. As a
    /// result `result.file_results.keys()` is empty in production, and the
    /// previous implementation of this function silently no-op'd —
    /// FILE_INDEX was never refreshed by materialize, leaving stale
    /// per-view hashes after `view switch` and producing false `Modified`
    /// reports from `status`.
    ///
    /// Walking `list_tracked_files()` is correct because materialize has
    /// just brought the working copy into sync with the destination
    /// view's recorded state — every tracked file's on-disk content is
    /// the authoritative baseline FILE_INDEX should cache.
    fn populate_file_index(&self, _result: &MaterializeResult) {
        use std::time::SystemTime;

        let tracked = match self.list_tracked_files() {
            Ok(t) => t,
            Err(_) => return,
        };

        let mut entries: Vec<(String, i64, u32, u64, Hash)> = Vec::with_capacity(tracked.len());

        for file in &tracked {
            let abs_path = self.root.join(&file.path);
            let metadata = match std::fs::metadata(&abs_path) {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };

            let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let duration = mtime
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            let secs = duration.as_secs() as i64;
            let nanos = duration.subsec_nanos();
            let size = metadata.len();

            let content_hash = match std::fs::read(&abs_path) {
                Ok(bytes) => Hash::of(&bytes),
                Err(_) => continue,
            };

            let normalized = file.path.to_string_lossy().replace('\\', "/");
            entries.push((normalized, secs, nanos, size, content_hash));
        }

        if !entries.is_empty() {
            let _ = self.update_file_index(&entries);
        }
    }

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

        self.populate_file_index(&result);

        Ok(result)
    }
}

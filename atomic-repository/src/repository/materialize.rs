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
        self.populate_mtime_cache(&result);

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
    /// Populate the mtime cache for all files in a materialize result.
    ///
    /// Stats each written file from disk and stores its mtime + size in the
    /// pristine database.  Errors are silently ignored (best-effort).
    fn populate_mtime_cache(&self, result: &MaterializeResult) {
        use std::time::SystemTime;

        let mut entries: Vec<(String, i64, u32, u64)> = Vec::new();

        for path in result.file_results.keys() {
            let abs_path = self.root.join(path);
            if let Ok(metadata) = std::fs::metadata(&abs_path) {
                let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let duration = mtime
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default();
                let secs = duration.as_secs() as i64;
                let nanos = duration.subsec_nanos();
                let size = metadata.len();
                let normalized = path.replace('\\', "/");
                entries.push((normalized, secs, nanos, size));
            }
        }

        if !entries.is_empty() {
            let _ = self.update_file_mtimes(&entries);
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

        self.populate_mtime_cache(&result);

        Ok(result)
    }
}

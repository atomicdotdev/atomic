use super::*;

impl Repository {
    /// Get the recorded content for a tracked file.
    ///
    /// This method builds a **change filter** that defines the current view's
    /// content perspective, then retrieves file content through the raw
    /// transaction.  Since all edges live in the global GRAPH, the raw
    /// transaction sees everything — the change filter handles view isolation.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    ///
    /// # Returns
    ///
    /// The file content as bytes, or `None` if the file is not tracked or
    /// has no recorded content from this view's perspective.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Read file content as seen by the current view
    /// let content = repo.get_file_content("src/main.rs")?;
    /// ```
    pub fn get_file_content<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        use atomic_core::output::alive::RetrieveOptions;
        use atomic_core::record::workflow::retrieve::retrieve_content_with_filter;

        let path = path.as_ref();
        let normalized = normalize_path(path);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        // Check if file is tracked (tree tables are global)
        if !is_tracked(&txn, &normalized).map_err(|e| RepositoryError::Database(e.to_string()))? {
            return Ok(None);
        }

        // Get inode → position
        let inode = match get_inode(&txn, &normalized) {
            Ok(Some(inode)) => inode,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        let position = match txn.inode_position(inode) {
            Ok(Some(pos)) => pos,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        // Always build the change filter.
        //
        // There is no "fast path" for shared root views: draft views
        // also write their vertices into the global GRAPH (the ambient
        // graph model), so an unfiltered retrieval on a shared root
        // would see vertices from drafts that aren't in its VIEW_CHANGES.
        //
        // The filter is the source of truth for what each view sees —
        // it's computed cheaply at read time from VIEW_CHANGES plus the
        // parent chain.
        let change_filter = collect_visible_change_ids_with_deps(&txn, &view)?;
        let options = RetrieveOptions::new().with_change_filter(change_filter);

        // All edges are in GRAPH — raw transaction sees everything.
        // The change_filter handles view isolation.
        let content = retrieve_content_with_filter(&txn, &self.change_store, position, options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if content.is_empty() {
            Ok(None)
        } else {
            Ok(Some(content))
        }
    }

    /// Get file content, excluding a specific change.
    ///
    /// This is identical to [`Self::get_file_content`] but removes
    /// `exclude_hash` from the change filter. Use this to get the file
    /// content as it was **before** a specific change was applied — pass
    /// the change's hash as `exclude_hash` and you get the prior state.
    pub fn get_file_content_excluding<P: AsRef<Path>>(
        &self,
        path: P,
        exclude_hash: &Hash,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        use atomic_core::output::alive::RetrieveOptions;
        use atomic_core::record::workflow::retrieve::retrieve_content_with_filter;

        let path = path.as_ref();
        let normalized = normalize_path(path);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        if !is_tracked(&txn, &normalized).map_err(|e| RepositoryError::Database(e.to_string()))? {
            return Ok(None);
        }

        let inode = match get_inode(&txn, &normalized) {
            Ok(Some(inode)) => inode,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        let position = match txn.inode_position(inode) {
            Ok(Some(pos)) => pos,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        let mut change_filter = if view.kind.is_shared() && view.parent.is_none() {
            collect_view_change_ids(&txn, &view)?
        } else {
            collect_visible_change_ids_with_deps(&txn, &view)?
        };

        // Remove the excluded change from the filter
        if let Ok(Some(exclude_id)) = txn.get_internal(exclude_hash) {
            change_filter.remove(&exclude_id);
        }

        let options = RetrieveOptions::new().with_change_filter(change_filter);

        let content = retrieve_content_with_filter(&txn, &self.change_store, position, options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if content.is_empty() {
            Ok(None)
        } else {
            Ok(Some(content))
        }
    }

    /// Diff two views: returns (changes only in A, changes only in B, common changes).
    ///
    /// This is the change-level diff. For file-content diff, use
    /// `get_file_content` for each view and diff the results.
    ///
    /// # Arguments
    ///
    /// * `view_a` - First view name
    /// * `view_b` - Second view name
    ///
    /// # Returns
    ///
    /// A tuple of `(only_in_a, only_in_b, in_both)` — each a `Vec<Hash>`.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (only_feature, only_dev, common) = repo.diff_views("feature", "dev")?;
    /// println!("{} changes only in feature", only_feature.len());
    /// println!("{} changes only in dev", only_dev.len());
    /// println!("{} changes in common", common.len());
    /// ```
    #[allow(clippy::type_complexity)]
    pub fn diff_views(
        &self,
        view_a: &str,
        view_b: &str,
    ) -> Result<(Vec<Hash>, Vec<Hash>, Vec<Hash>), RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let a = txn
            .get_view(view_a)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_a.to_string(),
            })?;

        let b = txn
            .get_view(view_b)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_b.to_string(),
            })?;

        // Collect hashes from each view
        let a_changes: Vec<Hash> = {
            let iter = txn
                .iter_changes(&a, 0)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let mut hashes = Vec::new();
            for result in iter {
                let (_seq, node_id, _merkle) =
                    result.map_err(|e| RepositoryError::Database(e.to_string()))?;
                if let Some(hash) = txn
                    .get_external(node_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                {
                    hashes.push(hash);
                }
            }
            hashes
        };

        let b_changes: Vec<Hash> = {
            let iter = txn
                .iter_changes(&b, 0)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let mut hashes = Vec::new();
            for result in iter {
                let (_seq, node_id, _merkle) =
                    result.map_err(|e| RepositoryError::Database(e.to_string()))?;
                if let Some(hash) = txn
                    .get_external(node_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                {
                    hashes.push(hash);
                }
            }
            hashes
        };

        let a_set: HashSet<Hash> = a_changes.iter().copied().collect();
        let b_set: HashSet<Hash> = b_changes.iter().copied().collect();

        let only_a: Vec<Hash> = a_changes
            .iter()
            .filter(|h| !b_set.contains(h))
            .copied()
            .collect();
        let only_b: Vec<Hash> = b_changes
            .iter()
            .filter(|h| !a_set.contains(h))
            .copied()
            .collect();
        let common: Vec<Hash> = a_changes
            .iter()
            .filter(|h| b_set.contains(h))
            .copied()
            .collect();

        Ok((only_a, only_b, common))
    }

    /// Get the recorded content for a tracked file on a specific view.
    ///
    /// Like `get_file_content`, but reads from the specified view instead
    /// of the current view. This is a **read-only** operation — it does
    /// NOT call `set_current_view` or write anything to disk.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    /// * `view_name` - The view to read from
    ///
    /// # Returns
    ///
    /// The file content as bytes, or `None` if the file is not tracked
    /// on the specified view.
    pub fn get_file_content_on_view<P: AsRef<Path>>(
        &self,
        path: P,
        view_name: &str,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let path = path.as_ref();
        let normalized = normalize_path(path);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the specified view (read-only — no set_current_stack)
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        let change_filter = if view.kind.is_shared() && view.parent.is_none() {
            collect_view_change_ids(&txn, &view)?
        } else {
            collect_visible_change_ids_with_deps(&txn, &view)?
        };

        // Use the filtered retrieval method
        self.get_file_content_with_filter(&txn, &normalized, change_filter, true)
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

    // State-Based Content Retrieval

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
    ///   is not in the current view's history
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
    /// 1. Finds the change's sequence number in the current view
    /// 2. Collects all changes applied BEFORE that sequence
    /// 3. Uses the change filter to retrieve content at that state
    ///
    /// # Performance
    ///
    /// The first call for a specific state involves iterating over the change
    /// log up to that point. For multiple files at the same state, consider
    /// using [`Self::get_file_content_at_sequence`] with a cached change set.
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

        // Get the current view
        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        // Find the state before this change
        let state_info = get_state_before_change(&txn, &view, change_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let state_info = match state_info {
            Some(info) => info,
            None => return Ok(None), // Change not in this view
        };

        // If this is the first change, there's no content before it
        if state_info.is_first_change() {
            return Ok(None);
        }

        // Get the set of changes applied before this change
        let change_set =
            get_changes_up_to_sequence(&txn, &view, state_info.parent_max_sequence_exclusive())
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Retrieve content with the change filter.
        // Pass require_tracked=false: the file may have been deleted after
        // this point, but we want its content as it existed before the change.
        self.get_file_content_with_filter(&txn, &normalized, change_set, false)
    }

    /// Get file content as it was AFTER a specific change was applied.
    ///
    /// This method retrieves the content of a file at the state immediately
    /// after a change was applied. Combined with [`Self::get_file_content_before_change`],
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
    ///   or the change is not in the current view's history
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

        // Get the current view
        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        // Get all changes up to and including this change
        let change_set = match get_changes_up_to_change(&txn, &view, change_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(set) => set,
            None => return Ok(None), // Change not in this view
        };

        // Retrieve content with the change filter.
        // require_tracked=true: if the file was deleted before this point,
        // there is no "after" content to return.
        self.get_file_content_with_filter(&txn, &normalized, change_set, true)
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

        // Get the current view
        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        // Get the set of changes up to the sequence
        let change_set = get_changes_up_to_sequence(&txn, &view, max_sequence)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Retrieve content with the change filter
        self.get_file_content_with_filter(&txn, &normalized, change_set, true)
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
        require_tracked: bool,
    ) -> Result<Option<Vec<u8>>, RepositoryError>
    where
        T: atomic_core::pristine::GraphTxnT + atomic_core::pristine::TreeTxnT,
    {
        use atomic_core::output::alive::RetrieveOptions;
        use atomic_core::record::workflow::retrieve::retrieve_content_with_filter;

        // Check if file is tracked (skip for deleted files — they are no
        // longer in the TREE but their inode/content is still in the graph).
        if require_tracked
            && !is_tracked(txn, normalized_path)
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

    // Archive Operations

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

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

        // NOTE on CRDT-driven output (task #24):
        // The new `output_file_via_crdt` walker in atomic_core::output::crdt
        // is faster and avoids the byte-graph linear-walker bugs that
        // overcount bytes on multi-edge vertices.  Callers that want the
        // *materialized* (no-filter, single-view) content can call it
        // directly via `get_file_content_via_crdt`.
        //
        // We don't use it here because this entry point honors the
        // view's `change_filter`, and the CRDT walker reads
        // `branch.state` directly — the materialized state across all
        // applied changes.  For multi-view scenarios that would expose
        // branches from views the caller isn't on.
        //
        // Wiring the CRDT walker into the filter-aware path requires
        // either (a) per-(change, branch) state-change tracking or
        // (b) replaying BranchOps from filter-in changes — both deferred.

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
        let content =
            retrieve_content_with_filter_fast(&txn, &self.change_store, inode, position, options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if content.is_empty() {
            Ok(None)
        } else {
            Ok(Some(content))
        }
    }

    /// Get file content using the CRDT-driven walker (task #24).
    ///
    /// Walks the `Trunk → Branch` chain in file order and fetches each
    /// alive branch's bytes from its recorded `BRANCH_VERTEX` span.  This
    /// bypasses the byte-graph linear walker entirely.
    ///
    /// # When to use
    ///
    /// Use this when you want the *materialized* file content — the state
    /// after all applied changes — without filtering by view.  This is
    /// correct for single-view linear history and for tools that want a
    /// canonical snapshot.
    ///
    /// For view-scoped reads, use [`Self::get_file_content`] instead.
    /// That entry point honors the view's `change_filter` (at the cost of
    /// going through the byte-graph walker).
    ///
    /// Falls back to byte-graph output when the CRDT layer has no row
    /// for this file (legacy repos that predate CRDT population) or when
    /// any alive branch lacks a `BRANCH_VERTEX` mapping.
    pub fn get_file_content_via_crdt<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        use atomic_core::output::crdt::{output_file_via_crdt, CrdtOutputError};

        let path = path.as_ref();
        let normalized = normalize_path(path);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        match output_file_via_crdt(&txn, &self.change_store, &normalized) {
            Ok(content) if !content.is_empty() => Ok(Some(content)),
            Ok(_) => {
                // Empty result — file not in CRDT layer.  Fall back to the
                // view-scoped byte-graph walker.
                drop(txn);
                self.get_file_content(path)
            }
            Err(CrdtOutputError::OrphanBranch(_)) => {
                // Alive branch without BRANCH_VERTEX — pre-walker data.
                // Fall back to byte-graph walker.
                drop(txn);
                self.get_file_content(path)
            }
            Err(CrdtOutputError::Pristine(e)) => {
                Err(RepositoryError::Database(e.to_string()))
            }
            Err(CrdtOutputError::Store(e)) => {
                Err(RepositoryError::Database(e.to_string()))
            }
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

        let content =
            retrieve_content_with_filter_fast(&txn, &self.change_store, inode, position, options)
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
        T: atomic_core::pristine::GraphTxnT
            + atomic_core::pristine::TreeTxnT
            + atomic_core::pristine::InodeGraphOps,
    {
        use atomic_core::output::alive::RetrieveOptions;
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
        let content =
            retrieve_content_with_filter_fast(txn, &self.change_store, inode, position, options)
            .map_err(|e: atomic_core::record::RecordError| {
                RepositoryError::Database(e.to_string())
            })?;

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

fn retrieve_content_with_filter_fast<T, C>(
    txn: &T,
    changes: &C,
    inode: Inode,
    position: Position<NodeId>,
    options: atomic_core::output::alive::RetrieveOptions,
) -> atomic_core::record::RecordResult<Vec<u8>>
where
    T: atomic_core::pristine::GraphTxnT + atomic_core::pristine::InodeGraphOps,
    C: atomic_core::change::ChangeStore,
{
    let trace_retrieve = std::env::var_os("ATOMIC_TRACE_RETRIEVE").is_some();
    if let Some(content) = try_retrieve_linear_content_with_filter(txn, changes, inode, position, &options)? {
        if trace_retrieve {
            eprintln!(
                "[retrieve_content_with_filter_fast] inode fast path hit bytes={}",
                content.len()
            );
        }
        return Ok(content);
    }

    if trace_retrieve {
        eprintln!("[retrieve_content_with_filter_fast] falling back to retrieve_graph");
    }

    atomic_core::record::workflow::retrieve::retrieve_content_with_filter(txn, changes, position, options)
}

fn try_retrieve_linear_content_with_filter<T, C>(
    txn: &T,
    changes: &C,
    inode: Inode,
    position: Position<NodeId>,
    options: &atomic_core::output::alive::RetrieveOptions,
) -> atomic_core::record::RecordResult<Option<Vec<u8>>>
where
    T: atomic_core::pristine::GraphTxnT + atomic_core::pristine::InodeGraphOps,
    C: atomic_core::change::ChangeStore,
{
    use atomic_core::types::EdgeFlags;
    let trace_retrieve = std::env::var_os("ATOMIC_TRACE_RETRIEVE").is_some();

    if position == Position::ROOT {
        return Ok(Some(Vec::new()));
    }

    let inode_marker = position.inode_node();
    let mut current = inode_marker;
    let mut visited = std::collections::HashSet::new();
    let mut vertices = Vec::new();

    loop {
        if !visited.insert(current) {
            if trace_retrieve {
                eprintln!(
                    "[try_retrieve_linear_content_with_filter] cycle at {}",
                    current
                );
            }
            return Ok(None);
        }

        let mut adj = txn
            .init_inode_adj(inode, current, EdgeFlags::BLOCK, EdgeFlags::all())
            .map_err(|e| {
                atomic_core::record::RecordError::Io(std::io::Error::other(format!(
                    "Failed to init inode traversal: {}",
                    e
                )))
            })?;

        let mut next_vertex = None;

        while let Some(edge_result) = txn.next_inode_adj(&mut adj) {
            let edge = match edge_result {
                Ok(edge) => edge,
                Err(_) => {
                    if trace_retrieve {
                        eprintln!(
                            "[try_retrieve_linear_content_with_filter] inode adj read error at {}",
                            current
                        );
                    }
                    return Ok(None);
                }
            };

            let flags = edge.flag();
            if flags.contains(EdgeFlags::PARENT)
                || flags.contains(EdgeFlags::PSEUDO)
                || flags.contains(EdgeFlags::FOLDER)
            {
                continue;
            }

            if !options.passes_filter(edge.introduced_by()) {
                continue;
            }

            if flags.contains(EdgeFlags::DELETED) {
                if trace_retrieve {
                    eprintln!(
                        "[try_retrieve_linear_content_with_filter] deleted edge from {} to {}",
                        current,
                        edge.dest()
                    );
                }
                return Ok(None);
            }

            let Some(dest) = (match txn.find_block_in_inode(inode, edge.dest()) {
                Ok(dest) => dest,
                Err(_) => {
                    if trace_retrieve {
                        eprintln!(
                            "[try_retrieve_linear_content_with_filter] find_block_in_inode error for {}",
                            edge.dest()
                        );
                    }
                    return Ok(None);
                }
            }) else {
                if trace_retrieve {
                    eprintln!(
                        "[try_retrieve_linear_content_with_filter] no inode block for {}",
                        edge.dest()
                    );
                }
                return Ok(None);
            };

            if !options.passes_filter(dest.change) {
                continue;
            }

            if next_vertex.replace(dest).is_some() {
                if trace_retrieve {
                    eprintln!(
                        "[try_retrieve_linear_content_with_filter] multiple successors from {}",
                        current
                    );
                }
                return Ok(None);
            }
        }

        let Some(dest) = next_vertex else {
            break;
        };

        let is_inode_marker = dest.start == dest.end && dest.start == position.pos;
        if !is_inode_marker && !dest.change.is_root() && dest.start != dest.end {
            vertices.push(dest);
        }

        current = dest;
    }

    let mut content = Vec::new();
    let mut change_contents = std::collections::HashMap::<Hash, Vec<u8>>::new();
    for node in vertices {
        let Some(hash) = txn.get_external(node.change).ok().flatten() else {
            if trace_retrieve {
                eprintln!(
                    "[try_retrieve_linear_content_with_filter] missing hash for {}",
                    node
                );
            }
            return Ok(None);
        };

        if let std::collections::hash_map::Entry::Vacant(entry) = change_contents.entry(hash) {
            let Ok(change) = changes.get_change(&hash) else {
                if trace_retrieve {
                    eprintln!(
                        "[try_retrieve_linear_content_with_filter] load_change failed for {}",
                        node
                    );
                }
                return Ok(None);
            };
            entry.insert(change.contents);
        }

        let start = node.start.get() as usize;
        let end = node.end.get() as usize;
        let bytes = change_contents.get(&hash).expect("change contents cached");
        if end > bytes.len() {
            if trace_retrieve {
                eprintln!(
                    "[try_retrieve_linear_content_with_filter] span out of bounds for {}",
                    node
                );
            }
            return Ok(None);
        }
        content.extend_from_slice(&bytes[start..end]);
    }

    Ok(Some(content))
}

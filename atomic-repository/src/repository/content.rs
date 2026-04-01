use super::*;
use atomic_core::pristine::OverlayTxn;

impl Repository {
    /// Get the recorded content for a tracked file using the overlay model.
    ///
    /// This is the **two-tier-aware** content retrieval method. It creates an
    /// `OverlayTxn` for the given stack so that graph traversal sees both the
    /// stack's own `STACK_GRAPH` edges and the global `GRAPH`.
    ///
    /// For Shared stacks this is equivalent to `get_file_content_on_stack`.
    /// For Local workspaces this is the only correct way to read content,
    /// because their edges live in `STACK_GRAPH`, not `GRAPH`.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    /// * `stack_name` - The stack whose perspective to use
    ///
    /// # Returns
    ///
    /// The file content as bytes, or `None` if the file is not tracked or
    /// has no recorded content from this stack's perspective.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Read file as seen by the "feature" local workspace
    /// let content = repo.get_file_content_via_overlay("src/main.rs", "feature")?;
    /// ```
    pub fn get_file_content_via_overlay<P: AsRef<Path>>(
        &self,
        path: P,
        stack_name: &str,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        use atomic_core::output::alive::RetrieveOptions;
        use atomic_core::record::workflow::retrieve::retrieve_content_with_filter;

        let path = path.as_ref();
        let normalized = normalize_path(path);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        // Build the overlay for this stack's perspective
        let overlay = OverlayTxn::from_stack(&txn, &stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if file is tracked (tree tables are global, overlay passes through)
        if !is_tracked(&overlay, &normalized)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            return Ok(None);
        }

        // Get inode → position
        let inode = match get_inode(&overlay, &normalized) {
            Ok(Some(inode)) => inode,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        let position = match overlay.inner().inode_position(inode) {
            Ok(Some(pos)) => pos,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        // Build a change filter starting from this stack's own changes,
        // then expand with their dependencies (from the change FILES,
        // not the DEPS table which is for attestations).
        //
        // After a content revise, the stack has A' but not A.  A' depends
        // on A (its hunks modify A's vertices).  Without including A's
        // NodeId in the filter, the alive-graph traversal would exclude
        // A's vertices and fail to produce content.
        let mut change_filter = collect_stack_change_ids(&txn, &stack)?;

        // Expand: for each stack change, load its change file, resolve
        // each dependency hash to a NodeId, and add to the filter.
        let direct_ids: Vec<NodeId> = change_filter.iter().copied().collect();
        for node_id in direct_ids {
            if let Ok(Some(hash)) = txn.get_external(node_id) {
                if let Ok(change) = self.load_change(&hash) {
                    for dep_hash in change.dependencies() {
                        if let Ok(Some(dep_id)) = txn.get_internal(dep_hash) {
                            change_filter.insert(dep_id);
                        }
                    }
                }
            }
        }

        // For local workspaces, also include changes from parent stacks in the
        // overlay chain, since those changes' vertices should be visible too.
        if stack.kind.is_local() {
            let chain = txn
                .resolve_overlay_chain(&stack)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            for &ancestor_id in &chain {
                if ancestor_id == stack.id {
                    continue; // already included above
                }
                if let Some(ancestor) = txn
                    .get_stack_by_id(ancestor_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                {
                    let ancestor_ids = collect_stack_change_ids(&txn, &ancestor)?;
                    change_filter.extend(ancestor_ids);
                }
            }

            // Also include changes from all shared ancestor stacks (the global
            // graph base). Walk the parent chain past the overlay to find the
            // shared stack and include its changes.
            let mut cursor = stack.parent;
            while let Some(pid) = cursor {
                let parent = txn
                    .get_stack_by_id(pid)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                match parent {
                    Some(p) if p.kind.is_shared() => {
                        let shared_ids = collect_stack_change_ids(&txn, &p)?;
                        change_filter.extend(shared_ids);
                        break;
                    }
                    Some(p) => cursor = p.parent,
                    None => break,
                }
            }
        }

        let options = RetrieveOptions::new().with_change_filter(change_filter);

        // Use the overlay for graph traversal so STACK_GRAPH edges are visible
        let content = retrieve_content_with_filter(&overlay, &self.change_store, position, options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if content.is_empty() {
            Ok(None)
        } else {
            Ok(Some(content))
        }
    }

    /// Get file content via overlay, excluding a specific change.
    ///
    /// This is identical to [`Self::get_file_content_via_overlay`] but removes
    /// `exclude_hash` from the change filter. Use this to get the file
    /// content as it was **before** a specific change was applied — pass
    /// the change's hash as `exclude_hash` and you get the prior state.
    pub fn get_file_content_via_overlay_excluding<P: AsRef<Path>>(
        &self,
        path: P,
        stack_name: &str,
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

        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        let overlay = OverlayTxn::from_stack(&txn, &stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if !is_tracked(&overlay, &normalized)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            return Ok(None);
        }

        let inode = match get_inode(&overlay, &normalized) {
            Ok(Some(inode)) => inode,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        let position = match overlay.inner().inode_position(inode) {
            Ok(Some(pos)) => pos,
            Ok(None) => return Ok(None),
            Err(e) => return Err(RepositoryError::Database(e.to_string())),
        };

        let mut change_filter = collect_stack_change_ids(&txn, &stack)?;

        if stack.kind.is_local() {
            let chain = txn
                .resolve_overlay_chain(&stack)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            for &ancestor_id in &chain {
                if ancestor_id == stack.id {
                    continue;
                }
                if let Some(ancestor) = txn
                    .get_stack_by_id(ancestor_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                {
                    let ancestor_ids = collect_stack_change_ids(&txn, &ancestor)?;
                    change_filter.extend(ancestor_ids);
                }
            }

            let mut cursor = stack.parent;
            while let Some(pid) = cursor {
                let parent = txn
                    .get_stack_by_id(pid)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                match parent {
                    Some(p) if p.kind.is_shared() => {
                        let shared_ids = collect_stack_change_ids(&txn, &p)?;
                        change_filter.extend(shared_ids);
                        break;
                    }
                    Some(p) => cursor = p.parent,
                    None => break,
                }
            }
        }

        // Remove the excluded change from the filter
        if let Ok(Some(exclude_id)) = txn.get_internal(exclude_hash) {
            change_filter.remove(&exclude_id);
        }

        let options = RetrieveOptions::new().with_change_filter(change_filter);

        let content = retrieve_content_with_filter(&overlay, &self.change_store, position, options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if content.is_empty() {
            Ok(None)
        } else {
            Ok(Some(content))
        }
    }

    /// Diff two stacks: returns (changes only in A, changes only in B, common changes).
    ///
    /// This is the change-level diff. For file-content diff, use
    /// `get_file_content_via_overlay` for each stack and diff the results.
    ///
    /// # Arguments
    ///
    /// * `stack_a` - First stack name
    /// * `stack_b` - Second stack name
    ///
    /// # Returns
    ///
    /// A tuple of `(only_in_a, only_in_b, in_both)` — each a `Vec<Hash>`.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let (only_feature, only_dev, common) = repo.diff_stacks("feature", "dev")?;
    /// println!("{} changes only in feature", only_feature.len());
    /// println!("{} changes only in dev", only_dev.len());
    /// println!("{} changes in common", common.len());
    /// ```
    #[allow(clippy::type_complexity)]
    pub fn diff_stacks(
        &self,
        stack_a: &str,
        stack_b: &str,
    ) -> Result<(Vec<Hash>, Vec<Hash>, Vec<Hash>), RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let a = txn
            .get_stack(stack_a)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_a.to_string(),
            })?;

        let b = txn
            .get_stack(stack_b)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_b.to_string(),
            })?;

        // Collect hashes from each stack
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

    /// This retrieves the file content from the repository graph as it was
    /// at the last recorded state. This is useful for computing diffs between
    /// the working copy and the recorded state.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    ///
    /// # Returns
    ///
    /// The file content as bytes, or `None` if the file is not tracked or
    /// has no recorded content.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The database cannot be accessed
    /// - Content retrieval fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Get recorded content for a file
    /// if let Some(content) = repo.get_file_content("src/main.rs")? {
    ///     let text = String::from_utf8_lossy(&content);
    ///     println!("Recorded content:\n{}", text);
    /// } else {
    ///     println!("File not tracked or has no content");
    /// }
    /// ```
    pub fn get_file_content<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let path = path.as_ref();
        let normalized = normalize_path(path);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the current stack to build change filter
        // This ensures we only retrieve content from changes in the current stack
        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        let change_filter = collect_stack_change_ids(&txn, &stack)?;

        // Use the filtered retrieval method
        self.get_file_content_with_filter(&txn, &normalized, change_filter, true)
    }

    /// Get the recorded content for a tracked file on a specific stack.
    ///
    /// Like `get_file_content`, but reads from the specified stack instead
    /// of the current stack. This is a **read-only** operation — it does
    /// NOT call `set_current_stack` or write anything to disk.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file (relative to repository root)
    /// * `stack_name` - The stack to read from
    ///
    /// # Returns
    ///
    /// The file content as bytes, or `None` if the file is not tracked
    /// on the specified stack.
    pub fn get_file_content_on_stack<P: AsRef<Path>>(
        &self,
        path: P,
        stack_name: &str,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let path = path.as_ref();
        let normalized = normalize_path(path);

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the specified stack (read-only — no set_current_stack)
        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        let change_filter = collect_stack_change_ids(&txn, &stack)?;

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
    ///   is not in the current stack's history
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
    /// 1. Finds the change's sequence number in the current stack
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

        // Get the current stack
        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        // Find the state before this change
        let state_info = get_state_before_change(&txn, &stack, change_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let state_info = match state_info {
            Some(info) => info,
            None => return Ok(None), // Change not in this stack
        };

        // If this is the first change, there's no content before it
        if state_info.is_first_change() {
            return Ok(None);
        }

        // Get the set of changes applied before this change
        let change_set =
            get_changes_up_to_sequence(&txn, &stack, state_info.parent_max_sequence_exclusive())
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
    ///   or the change is not in the current stack's history
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

        // Get the current stack
        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        // Get all changes up to and including this change
        let change_set = match get_changes_up_to_change(&txn, &stack, change_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(set) => set,
            None => return Ok(None), // Change not in this stack
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

        // Get the current stack
        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        // Get the set of changes up to the sequence
        let change_set = get_changes_up_to_sequence(&txn, &stack, max_sequence)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Retrieve content with the change filter.
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
            + atomic_core::pristine::StackTxnT,
    {
        use atomic_core::output::alive::RetrieveOptions;
        use atomic_core::pristine::overlay::OverlayTxn;
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

        // Build an OverlayTxn so that Local stacks can see their
        // STACK_GRAPH edges.  For Shared stacks the overlay is a
        // pass-through to the global GRAPH — no behaviour change.
        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        let overlay = OverlayTxn::from_stack(txn, &stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Retrieve content using the overlay (sees STACK_GRAPH for Local stacks)
        let content = retrieve_content_with_filter(&overlay, &self.change_store, position, options)
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

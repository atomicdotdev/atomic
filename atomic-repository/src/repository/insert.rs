use super::*;

use crate::apply::{
    filter_missing_in_view, get_missing_changes, get_view_changes as get_view_changes_fn,
    write_change_to_graph, CrossViewInsertOptions, CrossViewInsertOutcome, InsertOptions,
    InsertOutcome, InsertStats,
};

/// Check whether a file's creating change exists ONLY on the given view
/// (and no other view).  Returns `true` when it is safe to remove the
/// file's TREE / INODES entries because no other view needs them.
///
/// When the inode has no INODES position (not yet recorded) the function
/// returns `true` — there is nothing to protect.
///
/// # Complexity
///
/// O(S × log C) where S is the number of views and C is the number of
/// changes per view.  Each view is checked with a single B-tree lookup
/// on `REV_STACK_CHANGES` via [`ViewTxnT::get_change_seq`], rather than
/// linearly scanning the entire change log.
fn is_file_only_on_view<T: GraphTxnT + ViewTxnT + TreeTxnT>(
    txn: &T,
    inode: Inode,
    current_view: &str,
) -> bool {
    // Look up the position for this inode.  If there is no position the
    // file was never recorded, so removing from TREE is safe.
    let position = match txn.inode_position(inode) {
        Ok(Some(pos)) => pos,
        _ => return true,
    };

    let creating_change = position.change;
    if creating_change.is_root() {
        return true;
    }

    // Walk every view and check whether the creating change appears on
    // any view OTHER than `current_view`.
    let view_names = match txn.list_views() {
        Ok(names) => names,
        Err(_) => return true,
    };

    for name in view_names {
        if name == current_view {
            continue;
        }
        let view = match txn.get_view(&name) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        // O(log C) B-tree lookup on REV_STACK_CHANGES instead of
        // iterating the entire change log.
        if let Ok(Some(_seq)) = txn.get_change_seq(&view, creating_change) {
            // Another view still references this file — not safe to remove.
            return false;
        }
    }

    // No other view references the creating change.
    true
}

impl Repository {
    // Change Insertion Methods

    /// Insert a change into the current view.
    ///
    /// This is the high-level method for inserting a single change into the
    /// repository. It loads the change from the change store, validates
    /// dependencies, applies atoms to the graph, and updates the view state.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to insert
    /// * `options` - Options controlling insertion behavior
    ///
    /// # Returns
    ///
    /// An `InsertOutcome` containing the new state and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change is not found in the change store
    /// - Dependencies are missing (unless `apply_dependencies` is set)
    /// - The change is already inserted
    /// - A conflict occurs (unless `allow_conflicts` is set)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_repository::{Repository, InsertOptions};
    ///
    /// let repo = Repository::open(".")?;
    /// let result = repo.insert_change(&hash, InsertOptions::default())?;
    /// println!("New state: {}", result.new_state.to_base32());
    /// ```
    pub fn insert_change(
        &self,
        hash: &Hash,
        options: InsertOptions,
    ) -> Result<InsertOutcome, RepositoryError> {
        // Load the change from the store
        let change = self.load_change(hash)?;

        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Check if the change's edges are already in the global GRAPH.
        //
        // A change is "already in the global graph" when it is registered
        // (has a NodeId) AND at least one of its vertices exists in the
        // GRAPH B-tree.  `has_change_in_graph` performs a single O(log N)
        // range scan — far cheaper and more reliable than the previous
        // approach of loading the Change file and probing individual hunks.
        //
        // This correctly handles:
        //   - Changes recorded on a Draft view (edges in GRAPH only)
        //     → returns false, so hunks are re-applied to the global GRAPH
        //   - Changes already inserted into a Shared view (edges in GRAPH)
        //     → returns true, so redundant hunk application is skipped
        //   - Changes with only EdgeUpdate hunks (no FileAdd/DirAdd)
        //     → correctly detected via the range scan
        let already_in_graph = if let Some(node_id) = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            let in_graph = txn
                .has_change_in_graph(node_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            log::debug!(
                "insert_change: hash={} node_id={:?} already_in_graph={}",
                hash.to_base32(),
                node_id,
                in_graph
            );
            in_graph
        } else {
            log::debug!(
                "insert_change: hash={} not in INTERNAL (new change)",
                hash.to_base32()
            );
            false
        };

        // Register the change to get an internal ID (or get existing ID).
        // (If get_internal succeeded above, register_change just returns
        // the existing ID without re-registering.)
        let change_id = txn
            .register_change(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Determine which view to use
        let view_name = options.view.as_deref().unwrap_or(&self.current_view);
        log::debug!(
            "insert_change: change_id={:?} view={} already_in_graph={} hunks={}",
            change_id,
            view_name,
            already_in_graph,
            change.hunks().len()
        );

        // Populate tree tables for FileAdd/DirAdd/FileDel hunks.
        // This creates the path→inode→position mappings that materialize
        // needs to reconstruct files. Without this, server-side repos (which
        // receive changes via push rather than record) would have an empty tree.
        if !already_in_graph {
            for graph_op in change.hunks() {
                match graph_op {
                    GraphOp::FileAdd {
                        add_inode, path, ..
                    } => {
                        let new_inode = txn
                            .alloc_inode()
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        let inode_position = Position::new(change_id, add_inode.start);
                        txn.put_tree(path, new_inode)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        txn.put_inode(new_inode, inode_position)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                    GraphOp::DirAdd {
                        add_inode, path, ..
                    } => {
                        use atomic_core::pristine::directory_flags;
                        let new_inode = txn
                            .alloc_inode()
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        let inode_position = Position::new(change_id, add_inode.start);
                        txn.put_tree(path, new_inode)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        txn.put_inode(new_inode, inode_position)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        txn.put_directory(new_inode, directory_flags::explicit_empty())
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    }
                    GraphOp::FileDel { path, .. } => {
                        // View-aware: only remove TREE entry when no other
                        // view still references the file's creating change.
                        if let Ok(Some(inode)) = txn.get_inode(path) {
                            let dominated = is_file_only_on_view(&txn, inode, view_name);
                            if dominated {
                                let _ = txn.del_tree(path);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Apply to the graph (skips hunk application if already_in_graph)
        let outcome = write_change_to_graph(
            &mut txn,
            view_name,
            change_id,
            hash,
            &change,
            &options,
            already_in_graph,
        )
        .map_err(|e| RepositoryError::Apply(e.to_string()))?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(outcome)
    }

    /// Insert a change with automatic dependency resolution.
    ///
    /// This method attempts to insert a change and all its missing dependencies.
    /// Dependencies are inserted in topological order (dependencies before
    /// dependents).
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to insert
    /// * `options` - Options controlling insertion behavior
    ///
    /// # Returns
    ///
    /// An `InsertOutcome` containing aggregate statistics for all inserted changes.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Any required change cannot be found
    /// - A cyclic dependency is detected
    /// - Maximum recursion depth is exceeded
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.insert_change_rec(&hash, InsertOptions::default())?;
    /// println!("Inserted {} changes", result.stats.changes_applied);
    /// ```
    pub fn insert_change_rec(
        &self,
        hash: &Hash,
        options: InsertOptions,
    ) -> Result<InsertOutcome, RepositoryError> {
        // Load the target change to get its dependencies
        let _change = self.load_change(hash)?;

        // Get the view name
        let view_name = options.view.as_deref().unwrap_or(&self.current_view);

        // Get a read transaction to check what's already inserted
        let read_txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = read_txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        // Collect all needed changes (including the target)
        let mut to_insert = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(*hash);

        while let Some(current_hash) = queue.pop_front() {
            if visited.contains(&current_hash) {
                continue;
            }
            visited.insert(current_hash);

            // Check if already inserted
            if let Ok(Some(id)) = read_txn.get_internal(&current_hash) {
                if read_txn.get_change_seq(&view, id).ok().flatten().is_some() {
                    continue; // Already inserted
                }
            }

            // Load and queue dependencies
            let dep_change = self.load_change(&current_hash)?;
            for dep in dep_change.dependencies() {
                if !visited.contains(dep) {
                    queue.push_back(*dep);
                }
            }

            to_insert.push(current_hash);
        }

        drop(read_txn);

        // Reverse to get topological order (dependencies first)
        to_insert.reverse();

        // Now insert all changes in order
        let mut aggregate_stats = InsertStats::new();
        let mut final_state = Merkle::ZERO;
        let mut final_sequence = 0u64;
        let mut has_conflicts = false;

        for change_hash in &to_insert {
            let outcome = self.insert_change(change_hash, options.clone())?;
            aggregate_stats.merge(outcome.stats);
            final_state = outcome.new_state;
            final_sequence = outcome.sequence;
            if outcome.has_conflicts {
                has_conflicts = true;
            }
        }

        Ok(InsertOutcome::new(
            final_state,
            final_sequence,
            has_conflicts,
            aggregate_stats,
        ))
    }

    /// Write a recorded change to the repository.
    ///
    /// This method inserts a change that was just recorded, updating both the
    /// graph and the tree tables. It's the integration point between recording
    /// and inserting.
    ///
    /// Unlike `insert_change`, this method:
    /// - Takes the change directly (doesn't load from store)
    /// - Updates tree tables for FileAdd hunks
    /// - Assigns new inodes to added files
    ///
    /// # Arguments
    ///
    /// * `outcome` - The outcome from `record()` containing the change
    /// * `options` - Options controlling insertion behavior
    ///
    /// # Returns
    ///
    /// An `InsertOutcome` with the new state and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change has conflicts and `allow_conflicts` is false
    /// - Database operations fail
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let record_outcome = repo.record(header, options)?;
    /// let apply_outcome = repo.write_recorded(&record_outcome, InsertOptions::default())?;
    /// println!("Inserted with state: {}", apply_outcome.new_state.to_base32());
    /// ```
    pub fn write_recorded(
        &self,
        outcome: &RecordOutcome,
        options: InsertOptions,
    ) -> Result<InsertOutcome, RepositoryError> {
        let change = outcome.change();
        let hash = outcome.hash();

        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Register the change to get an internal ID
        let change_id = txn
            .register_change(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Determine which view to use
        let view_name = options.view.as_deref().unwrap_or(&self.current_view);

        // Before applying atoms, set up tree entries for FileAdd hunks.
        // This creates the inode→position and path→inode mappings needed
        // for the graph operations.
        //
        // Note: put_tree creates both TREE and REV_TREE entries.
        //       put_inode creates both INODES and REV_INODES entries.
        for graph_op in change.hunks() {
            match graph_op {
                GraphOp::FileAdd {
                    add_inode, path, ..
                } => {
                    // Allocate a new inode for this file
                    let new_inode = txn
                        .alloc_inode()
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;

                    // The inode span position is relative to this change.
                    // Since add_inode.start is a ChangePosition within this change's content,
                    // we create an internal position using the change_id we just registered.
                    let inode_position = Position::new(change_id, add_inode.start);

                    // Add to tree tables:
                    // - put_tree: path ↔ inode (TREE and REV_TREE)
                    // - put_inode: inode ↔ position (INODES and REV_INODES)
                    txn.put_tree(path, new_inode)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    txn.put_inode(new_inode, inode_position)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
                GraphOp::DirAdd {
                    add_inode, path, ..
                } => {
                    use atomic_core::pristine::directory_flags;

                    // Allocate a new inode for this directory
                    let new_inode = txn
                        .alloc_inode()
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;

                    // The inode span position is relative to this change.
                    let inode_position = Position::new(change_id, add_inode.start);

                    // Add to tree tables:
                    // - put_tree: path ↔ inode (TREE and REV_TREE)
                    // - put_inode: inode ↔ position (INODES and REV_INODES)
                    // - put_directory: mark inode as directory (DIRECTORIES)
                    txn.put_tree(path, new_inode)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    txn.put_inode(new_inode, inode_position)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                    txn.put_directory(new_inode, directory_flags::explicit_empty())
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
                GraphOp::FileDel { path, .. } => {
                    // View-aware deletion: only remove TREE/INODES entries
                    // when no OTHER view still references the file's creating
                    // change.  The TREE and INODES tables are global — removing
                    // an entry here would make the file invisible on every
                    // view, not just the one where the deletion was recorded.
                    if let Ok(Some(inode)) = txn.get_inode(path) {
                        let dominated = is_file_only_on_view(&txn, inode, view_name);
                        if dominated {
                            let _ = txn.del_tree(path);
                            let _ = txn.del_inode(inode);
                        }
                        // When other views still reference the file we leave
                        // TREE/INODES intact.  The deletion is represented in
                        // the graph via DELETED edges and will be honoured by
                        // materialize's change_filter / retrieve_graph.
                    }
                }
                GraphOp::DirDel { path, .. } => {
                    // Same view-aware logic as FileDel above.
                    if let Ok(Some(inode)) = txn.get_inode(path) {
                        let dominated = is_file_only_on_view(&txn, inode, view_name);
                        if dominated {
                            let _ = txn.del_tree(path);
                            let _ = txn.del_inode(inode);
                            let _ = txn.del_directory(inode);
                        }
                    }
                }
                _ => {}
            }
        }

        // Handle file deletions tracked in the outcome.
        // Since we use GraphOp::Edit with EdgeUpdate for deletions (not GraphOp::FileDel),
        // we need to explicitly remove deleted files from the tree tables.
        // View-aware: only remove if no other view still references the file.
        for deleted_path in outcome.deleted_files() {
            if let Ok(Some(inode)) = txn.get_inode(deleted_path) {
                let dominated = is_file_only_on_view(&txn, inode, view_name);
                if dominated {
                    let _ = txn.del_tree(deleted_path);
                    let _ = txn.del_inode(inode);
                }
            }
        }

        // Apply to the graph
        // For write_recorded, the change is always new (just recorded), so
        // already_in_graph is always false.
        let apply_outcome = write_change_to_graph(
            &mut txn, view_name, change_id, hash, change, &options,
            false, // always_in_graph: freshly recorded changes are never in the graph yet
        )
        .map_err(|e| RepositoryError::Apply(e.to_string()))?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(apply_outcome)
    }

    // Cross-View Insert Methods

    /// Get all changes inserted into a view.
    ///
    /// Returns changes in order from oldest (sequence 0) to newest.
    ///
    /// # Arguments
    ///
    /// * `view_name` - Name of the view to query (None = current view)
    ///
    /// # Returns
    ///
    /// Vector of (sequence, hash) pairs.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let changes = repo.get_view_changes(None)?;
    /// for (seq, hash) in changes {
    ///     println!("#{}: {}", seq, hash.to_base32());
    /// }
    /// ```
    pub fn get_view_changes(
        &self,
        view_name: Option<&str>,
    ) -> Result<Vec<(u64, Hash)>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let name = view_name.unwrap_or(&self.current_view);
        let view = txn
            .get_view(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: name.to_string(),
            })?;

        get_view_changes_fn(&txn, &view).map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Get changes that are in one view but not another.
    ///
    /// This is useful for determining what needs to be inserted when
    /// merging or cherry-picking between views.
    ///
    /// # Arguments
    ///
    /// * `from_view` - Source view name
    /// * `to_view` - Target view name (None = current view)
    ///
    /// # Returns
    ///
    /// Vector of hashes that are in `from_view` but not in `to_view`,
    /// in dependency order.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Find what's in feature that's not in main
    /// let missing = repo.get_missing_changes_between("feature", Some("main"))?;
    /// println!("{} changes to insert", missing.len());
    /// ```
    pub fn get_missing_changes_between(
        &self,
        from_view: &str,
        to_view: Option<&str>,
    ) -> Result<Vec<Hash>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let from = txn
            .get_view(from_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: from_view.to_string(),
            })?;

        let to_name = to_view.unwrap_or(&self.current_view);
        let to = txn
            .get_view(to_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: to_name.to_string(),
            })?;

        get_missing_changes(&txn, &from, &to).map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Get changes up to a specific tag in a view.
    ///
    /// Returns all changes from sequence 0 up to and including the
    /// sequence where the tag was created.
    ///
    /// # Arguments
    ///
    /// * `tag_name` - Name of the tag
    /// * `view_name` - View to search (None = use tag's view)
    ///
    /// # Returns
    ///
    /// Vector of change hashes up to the tagged state.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let changes = repo.get_changes_up_to_tag("v1.0.0", None)?;
    /// println!("{} changes in release", changes.len());
    /// ```
    pub fn get_changes_up_to_tag(
        &self,
        tag_name: &str,
        view_name: Option<&str>,
    ) -> Result<Vec<Hash>, RepositoryError> {
        // Get the tag
        let tag = if let Some(view) = view_name {
            self.get_tag_from_view(tag_name, view)?
        } else {
            // Try current view first, then any view
            self.get_tag(tag_name)?.or(self.get_tag_any_view(tag_name)?)
        };

        let tag = tag.ok_or_else(|| RepositoryError::TagNotFound {
            name: tag_name.to_string(),
        })?;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view = txn
            .get_view(&tag.view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: tag.view.clone(),
            })?;

        // Get changes up to and including the tag's sequence
        crate::apply::get_changes_up_to_seq(&txn, &view, tag.sequence)
            .map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Insert changes from one view into another.
    ///
    /// This is the main method for cross-view operations. It can:
    /// - Insert all missing changes from source to target
    /// - Insert only changes up to a specific tag
    /// - Insert only specific changes
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling the cross-view insert
    ///
    /// # Returns
    ///
    /// A `CrossViewInsertOutcome` with details about what was inserted.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Insert all changes from feature to main
    /// let options = CrossViewInsertOptions::new("feature", "main");
    /// let result = repo.insert_from_view(options)?;
    /// println!("Inserted {} changes", result.changes_applied);
    ///
    /// // Insert changes up to a tag
    /// let options = CrossViewInsertOptions::new("feature", "main")
    ///     .up_to_tag("v1.0.0");
    /// let result = repo.insert_from_view(options)?;
    /// ```
    pub fn insert_from_view(
        &self,
        options: CrossViewInsertOptions,
    ) -> Result<CrossViewInsertOutcome, RepositoryError> {
        let mut outcome = CrossViewInsertOutcome::new();
        outcome.was_dry_run = options.dry_run;

        // Determine which changes to consider
        let source_changes = if !options.only_changes.is_empty() {
            // Use only specified changes
            options.only_changes.clone()
        } else if let Some(ref tag_name) = options.up_to_tag {
            // Get changes up to the tag
            self.get_changes_up_to_tag(tag_name, Some(&options.from_view))?
        } else {
            // Get all changes from source view
            self.get_view_changes(Some(&options.from_view))?
                .into_iter()
                .map(|(_, hash)| hash)
                .collect()
        };

        // Filter to changes not already in target
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let to_view = txn
            .get_view(&options.to_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: options.to_view.clone(),
            })?;

        let missing = filter_missing_in_view(&txn, &to_view, &source_changes)
            .map_err(|e| RepositoryError::Apply(e.to_string()))?;

        // Track skipped changes
        let missing_set: std::collections::HashSet<_> = missing.iter().collect();
        for hash in &source_changes {
            if !missing_set.contains(hash) {
                outcome.skipped_hashes.push(*hash);
            }
        }

        drop(txn);

        if missing.is_empty() {
            // Nothing to insert
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let view = txn
                .get_view(&options.to_view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .unwrap();
            outcome.new_state = view.state;
            outcome.sequence = view.change_count;
            return Ok(outcome);
        }

        // If dry run, just return what would be inserted
        if options.dry_run {
            outcome.applied_hashes = missing;
            outcome.changes_applied = outcome.applied_hashes.len();
            return Ok(outcome);
        }

        // Insert each change in order.
        //
        // When the source view is Draft, its changes were recorded against
        // the view filter (GRAPH).  Inserting those changes
        // into a different view verifies edge context against a different
        // graph view, which produces spurious "missing context" conflicts.
        // These are architecturally expected — not real data conflicts —
        // so we automatically allow them for cross-view insert.
        let source_is_draft = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.get_view(&options.from_view)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .map(|s| s.kind.is_draft())
                .unwrap_or(false)
        };

        let apply_opts = InsertOptions::default()
            .view(&options.to_view)
            .allow_conflict(options.allow_conflicts || source_is_draft);

        for hash in &missing {
            let result = if options.apply_dependencies {
                self.insert_change_rec(hash, apply_opts.clone())
            } else {
                self.insert_change(hash, apply_opts.clone())
            };

            match result {
                Ok(apply_outcome) => {
                    outcome.applied_hashes.push(*hash);
                    outcome.changes_applied += 1;
                    outcome.new_state = apply_outcome.new_state;
                    outcome.sequence = apply_outcome.sequence;
                    if apply_outcome.has_conflicts {
                        outcome.has_conflicts = true;
                    }
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }

        Ok(outcome)
    }

    /// Insert changes up to a tag from one view into another.
    ///
    /// This is a convenience method that combines `get_changes_up_to_tag`
    /// and `insert_from_view`.
    ///
    /// # Arguments
    ///
    /// * `tag_name` - Name of the tag to insert up to
    /// * `from_view` - Source view containing the tag
    /// * `to_view` - Target view (None = current view)
    ///
    /// # Returns
    ///
    /// A `CrossViewInsertOutcome` with details about what was inserted.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Insert release-1.0.0 from feature to main
    /// let result = repo.insert_tag_to_view("release-1.0.0", "feature", Some("main"))?;
    /// ```
    pub fn insert_tag_to_view(
        &self,
        tag_name: &str,
        from_view: &str,
        to_view: Option<&str>,
    ) -> Result<CrossViewInsertOutcome, RepositoryError> {
        let target = to_view.unwrap_or(&self.current_view);

        let options = CrossViewInsertOptions::new(from_view, target)
            .up_to_tag(tag_name)
            .with_dependencies(true);

        self.insert_from_view(options)
    }

    /// Cherry-pick specific changes from one view into another.
    ///
    /// # Arguments
    ///
    /// * `changes` - Hashes of changes to insert
    /// * `from_view` - Source view (for validation)
    /// * `to_view` - Target view (None = current view)
    ///
    /// # Returns
    ///
    /// A `CrossViewInsertOutcome` with details about what was inserted.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.cherry_pick(&[hash1, hash2], "feature", None)?;
    /// ```
    pub fn cherry_pick(
        &self,
        changes: &[Hash],
        _from_view: &str,
        to_view: Option<&str>,
    ) -> Result<CrossViewInsertOutcome, RepositoryError> {
        let target = to_view.unwrap_or(&self.current_view);

        // For cherry-pick, we insert specific changes with dependencies
        let options = CrossViewInsertOptions::new("", target)
            .only_changes(changes.to_vec())
            .with_dependencies(true);

        self.insert_from_view(options)
    }
}

use super::*;

/// Check whether a file's creating change exists ONLY on the given stack
/// (and no other stack).  Returns `true` when it is safe to remove the
/// file's TREE / INODES entries because no other stack needs them.
///
/// When the inode has no INODES position (not yet recorded) the function
/// returns `true` — there is nothing to protect.
///
/// # Complexity
///
/// O(S × log C) where S is the number of stacks and C is the number of
/// changes per stack.  Each stack is checked with a single B-tree lookup
/// on `REV_STACK_CHANGES` via [`StackTxnT::get_change_seq`], rather than
/// linearly scanning the entire change log.
fn is_file_only_on_stack<T: GraphTxnT + StackTxnT + TreeTxnT>(
    txn: &T,
    inode: Inode,
    current_stack: &str,
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

    // Walk every stack and check whether the creating change appears on
    // any stack OTHER than `current_stack`.
    let stack_names = match txn.list_stacks() {
        Ok(names) => names,
        Err(_) => return true,
    };

    for name in stack_names {
        if name == current_stack {
            continue;
        }
        let stack = match txn.get_stack(&name) {
            Ok(Some(s)) => s,
            _ => continue,
        };
        // O(log C) B-tree lookup on REV_STACK_CHANGES instead of
        // iterating the entire change log.
        if let Ok(Some(_seq)) = txn.get_change_seq(&stack, creating_change) {
            // Another stack still references this file — not safe to remove.
            return false;
        }
    }

    // No other stack references the creating change.
    true
}

impl Repository {
    // Change Application Methods

    /// Apply a change to the current stack.
    ///
    /// This is the high-level method for applying a single change to the
    /// repository. It loads the change from the change store, validates
    /// dependencies, applies atoms to the graph, and updates the stack state.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to apply
    /// * `options` - Options controlling application behavior
    ///
    /// # Returns
    ///
    /// An `ApplyOutcome` containing the new state and statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change is not found in the change store
    /// - Dependencies are missing (unless `apply_dependencies` is set)
    /// - The change is already applied
    /// - A conflict occurs (unless `allow_conflicts` is set)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use atomic_repository::{Repository, ApplyOptions};
    ///
    /// let repo = Repository::open(".")?;
    /// let result = repo.apply_change(&hash, ApplyOptions::default())?;
    /// println!("New state: {}", result.new_state.to_base32());
    /// ```
    pub fn apply_change(
        &self,
        hash: &Hash,
        options: ApplyOptions,
    ) -> Result<ApplyOutcome, RepositoryError> {
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
        //   - Changes recorded on a Local stack (edges in STACK_GRAPH only)
        //     → returns false, so hunks are re-applied to the global GRAPH
        //   - Changes already applied to a Shared stack (edges in GRAPH)
        //     → returns true, so redundant hunk application is skipped
        //   - Changes with only EdgeUpdate hunks (no FileAdd/DirAdd)
        //     → correctly detected via the range scan
        let already_in_graph = if let Some(node_id) = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            txn.has_change_in_graph(node_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
        } else {
            false
        };

        // Register the change to get an internal ID (or get existing ID).
        // (If get_internal succeeded above, register_change just returns
        // the existing ID without re-registering.)
        let change_id = txn
            .register_change(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Determine which stack to use
        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);

        // Populate tree tables for FileAdd/DirAdd/FileDel hunks.
        // This creates the path→inode→position mappings that output_working_copy
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
                        // Stack-aware: only remove TREE entry when no other
                        // stack still references the file's creating change.
                        if let Ok(Some(inode)) = txn.get_inode(path) {
                            let dominated = is_file_only_on_stack(&txn, inode, stack_name);
                            if dominated {
                                let _ = txn.del_tree(path);
                            }
                        }
                    }
                    GraphOp::FileMove { add, path, .. } => {
                        // A FileMove reuses the existing inode — look it up via
                        // the inode position stored in add.inode, then update
                        // TREE: remove the old path mapping and insert the new one.
                        //
                        // add.inode is Position<Option<Hash>>; resolve it to
                        // Position<NodeId> so we can call position_inode().
                        let inode_change_id = match &add.inode.change {
                            None => change_id, // self-reference (shouldn't happen for FileMove)
                            Some(h) if *h == Hash::NONE => NodeId::ROOT,
                            Some(h) => txn.get_internal(h).unwrap_or(None).unwrap_or(NodeId::ROOT),
                        };
                        let inode_pos = Position::new(inode_change_id, add.inode.pos);

                        if let Ok(Some(inode)) = txn.position_inode(inode_pos) {
                            // Remove the old TREE entry (old path → inode)
                            if let Ok(Some(old_path)) = txn.get_path(inode) {
                                // Guard: only delete the old path if it differs
                                // from the new path.  When multiple files share
                                // the same inode position (a rare data-integrity
                                // edge case), position_inode may resolve to an
                                // inode whose current path was already updated
                                // by a prior FileMove in this same change.
                                // Deleting it would undo that earlier rename.
                                if old_path != *path {
                                    let _ = txn.del_tree(&old_path);
                                }
                            }
                            // Insert the new TREE entry (new path → inode)
                            let _ = txn.put_tree(path, inode);
                        }
                    }
                    _ => {}
                }
            }
        }

        // Apply to the graph (skips hunk application if already_in_graph)
        let outcome = apply_change_to_graph(
            &mut txn,
            stack_name,
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

    /// Apply a change with automatic dependency resolution.
    ///
    /// This method attempts to apply a change and all its missing dependencies.
    /// Dependencies are applied in topological order (dependencies before
    /// dependents).
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to apply
    /// * `options` - Options controlling application behavior
    ///
    /// # Returns
    ///
    /// An `ApplyOutcome` containing aggregate statistics for all applied changes.
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
    /// let result = repo.apply_change_rec(&hash, ApplyOptions::default())?;
    /// println!("Applied {} changes", result.stats.changes_applied);
    /// ```
    pub fn apply_change_rec(
        &self,
        hash: &Hash,
        options: ApplyOptions,
    ) -> Result<ApplyOutcome, RepositoryError> {
        // Load the target change to get its dependencies
        let _change = self.load_change(hash)?;

        // Get the stack name
        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);

        // Get a read transaction to check what's already applied
        let read_txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = read_txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        // Collect all needed changes (including the target)
        let mut to_apply = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(*hash);

        while let Some(current_hash) = queue.pop_front() {
            if visited.contains(&current_hash) {
                continue;
            }
            visited.insert(current_hash);

            // Check if already applied
            if let Ok(Some(id)) = read_txn.get_internal(&current_hash) {
                if read_txn.get_change_seq(&stack, id).ok().flatten().is_some() {
                    continue; // Already applied
                }
            }

            // Load and queue dependencies
            let dep_change = self.load_change(&current_hash)?;
            for dep in dep_change.dependencies() {
                if !visited.contains(dep) {
                    queue.push_back(*dep);
                }
            }

            to_apply.push(current_hash);
        }

        drop(read_txn);

        // Reverse to get topological order (dependencies first)
        to_apply.reverse();

        // Now apply all changes in order
        let mut aggregate_stats = ApplyStats::new();
        let mut final_state = Merkle::ZERO;
        let mut final_sequence = 0u64;
        let mut has_conflicts = false;

        for change_hash in &to_apply {
            let outcome = self.apply_change(change_hash, options.clone())?;
            aggregate_stats.merge(outcome.stats);
            final_state = outcome.new_state;
            final_sequence = outcome.sequence;
            if outcome.has_conflicts {
                has_conflicts = true;
            }
        }

        Ok(ApplyOutcome::new(
            final_state,
            final_sequence,
            has_conflicts,
            aggregate_stats,
        ))
    }

    /// Apply a recorded change to the repository.
    ///
    /// This method applies a change that was just recorded, updating both the
    /// graph and the tree tables. It's the integration point between recording
    /// and applying.
    ///
    /// Unlike `apply_change`, this method:
    /// - Takes the change directly (doesn't load from store)
    /// - Updates tree tables for FileAdd hunks
    /// - Assigns new inodes to added files
    ///
    /// # Arguments
    ///
    /// * `outcome` - The outcome from `record()` containing the change
    /// * `options` - Options controlling application behavior
    ///
    /// # Returns
    ///
    /// An `ApplyOutcome` with the new state and statistics.
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
    /// let apply_outcome = repo.apply_recorded(&record_outcome, ApplyOptions::default())?;
    /// println!("Applied with state: {}", apply_outcome.new_state.to_base32());
    /// ```
    pub fn apply_recorded(
        &self,
        outcome: &RecordOutcome,
        options: ApplyOptions,
    ) -> Result<ApplyOutcome, RepositoryError> {
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

        // Determine which stack to use
        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);

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
                    // Stack-aware deletion: only remove TREE/INODES entries
                    // when no OTHER stack still references the file's creating
                    // change.  The TREE and INODES tables are global — removing
                    // an entry here would make the file invisible on every
                    // stack, not just the one where the deletion was recorded.
                    if let Ok(Some(inode)) = txn.get_inode(path) {
                        let dominated = is_file_only_on_stack(&txn, inode, stack_name);
                        if dominated {
                            let _ = txn.del_tree(path);
                            let _ = txn.del_inode(inode);
                        }
                        // When other stacks still reference the file we leave
                        // TREE/INODES intact.  The deletion is represented in
                        // the graph via DELETED edges and will be honoured by
                        // output_working_copy's change_filter / retrieve_graph.
                    }
                }
                GraphOp::DirDel { path, .. } => {
                    // Same stack-aware logic as FileDel above.
                    if let Ok(Some(inode)) = txn.get_inode(path) {
                        let dominated = is_file_only_on_stack(&txn, inode, stack_name);
                        if dominated {
                            let _ = txn.del_tree(path);
                            let _ = txn.del_inode(inode);
                            let _ = txn.del_directory(inode);
                        }
                    }
                }
                GraphOp::FileMove { add, path, .. } => {
                    // A FileMove reuses the existing inode — look it up via
                    // the inode position stored in add.inode, then update
                    // TREE: remove the old path mapping and insert the new one.
                    //
                    // add.inode is Position<Option<Hash>>; resolve it to
                    // Position<NodeId> so we can call position_inode().
                    let inode_change_id = match &add.inode.change {
                        None => change_id, // self-reference (shouldn't happen for FileMove)
                        Some(h) if *h == Hash::NONE => NodeId::ROOT,
                        Some(h) => txn.get_internal(h).unwrap_or(None).unwrap_or(NodeId::ROOT),
                    };
                    let inode_pos = Position::new(inode_change_id, add.inode.pos);

                    if let Ok(Some(inode)) = txn.position_inode(inode_pos) {
                        // Remove the old TREE entry (old path → inode)
                        if let Ok(Some(old_path)) = txn.get_path(inode) {
                            // Guard: only delete the old path if it differs
                            // from the new path.  When multiple files share
                            // the same inode position (a rare data-integrity
                            // edge case), position_inode may resolve to an
                            // inode whose current path was already updated
                            // by a prior FileMove in this same change.
                            // Deleting it would undo that earlier rename.
                            if old_path != *path {
                                let _ = txn.del_tree(&old_path);
                            }
                        }
                        // Insert the new TREE entry (new path → inode)
                        let _ = txn.put_tree(path, inode);
                    }
                }
                _ => {}
            }
        }

        // Handle file deletions tracked in the outcome.
        // Since we use GraphOp::Edit with EdgeUpdate for deletions (not GraphOp::FileDel),
        // we need to explicitly remove deleted files from the tree tables.
        // Stack-aware: only remove if no other stack still references the file.
        for deleted_path in outcome.deleted_files() {
            if let Ok(Some(inode)) = txn.get_inode(deleted_path) {
                let dominated = is_file_only_on_stack(&txn, inode, stack_name);
                if dominated {
                    let _ = txn.del_tree(deleted_path);
                    let _ = txn.del_inode(inode);
                }
            }
        }

        // Apply to the graph
        // For apply_recorded, the change is always new (just recorded), so
        // already_in_graph is always false.
        let apply_outcome = apply_change_to_graph(
            &mut txn, stack_name, change_id, hash, change, &options,
            false, // always_in_graph: freshly recorded changes are never in the graph yet
        )
        .map_err(|e| RepositoryError::Apply(e.to_string()))?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(apply_outcome)
    }

    // Cross-Stack Apply Methods

    /// Get all changes applied to a stack.
    ///
    /// Returns changes in order from oldest (sequence 0) to newest.
    ///
    /// # Arguments
    ///
    /// * `stack_name` - Name of the stack to query (None = current stack)
    ///
    /// # Returns
    ///
    /// Vector of (sequence, hash) pairs.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let changes = repo.get_stack_changes(None)?;
    /// for (seq, hash) in changes {
    ///     println!("#{}: {}", seq, hash.to_base32());
    /// }
    /// ```
    pub fn get_stack_changes(
        &self,
        stack_name: Option<&str>,
    ) -> Result<Vec<(u64, Hash)>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let name = stack_name.unwrap_or(&self.current_stack);
        let stack = txn
            .get_stack(name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: name.to_string(),
            })?;

        get_stack_changes(&txn, &stack).map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Get changes that are in one stack but not another.
    ///
    /// This is useful for determining what needs to be applied when
    /// merging or cherry-picking between stacks.
    ///
    /// # Arguments
    ///
    /// * `from_stack` - Source stack name
    /// * `to_stack` - Target stack name (None = current stack)
    ///
    /// # Returns
    ///
    /// Vector of hashes that are in `from_stack` but not in `to_stack`,
    /// in dependency order.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Find what's in feature that's not in main
    /// let missing = repo.get_missing_changes_between("feature", Some("main"))?;
    /// println!("{} changes to apply", missing.len());
    /// ```
    pub fn get_missing_changes_between(
        &self,
        from_stack: &str,
        to_stack: Option<&str>,
    ) -> Result<Vec<Hash>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let from = txn
            .get_stack(from_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: from_stack.to_string(),
            })?;

        let to_name = to_stack.unwrap_or(&self.current_stack);
        let to = txn
            .get_stack(to_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: to_name.to_string(),
            })?;

        get_missing_changes(&txn, &from, &to).map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Get changes up to a specific tag in a stack.
    ///
    /// Returns all changes from sequence 0 up to and including the
    /// sequence where the tag was created.
    ///
    /// # Arguments
    ///
    /// * `tag_name` - Name of the tag
    /// * `stack_name` - Stack to search (None = use tag's stack)
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
        stack_name: Option<&str>,
    ) -> Result<Vec<Hash>, RepositoryError> {
        // Get the tag
        let tag = if let Some(stack) = stack_name {
            self.get_tag_from_stack(tag_name, stack)?
        } else {
            // Try current stack first, then any stack
            self.get_tag(tag_name)?
                .or(self.get_tag_any_stack(tag_name)?)
        };

        let tag = tag.ok_or_else(|| RepositoryError::TagNotFound {
            name: tag_name.to_string(),
        })?;

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = txn
            .get_stack(&tag.stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: tag.stack.clone(),
            })?;

        // Get changes up to and including the tag's sequence
        crate::apply::get_changes_up_to_seq(&txn, &stack, tag.sequence)
            .map_err(|e| RepositoryError::Apply(e.to_string()))
    }

    /// Apply changes from one stack to another.
    ///
    /// This is the main method for cross-stack operations. It can:
    /// - Apply all missing changes from source to target
    /// - Apply only changes up to a specific tag
    /// - Apply only specific changes
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling the cross-stack apply
    ///
    /// # Returns
    ///
    /// A `CrossStackApplyOutcome` with details about what was applied.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Apply all changes from feature to main
    /// let options = CrossStackApplyOptions::new("feature", "main");
    /// let result = repo.apply_from_stack(options)?;
    /// println!("Applied {} changes", result.changes_applied);
    ///
    /// // Apply changes up to a tag
    /// let options = CrossStackApplyOptions::new("feature", "main")
    ///     .up_to_tag("v1.0.0");
    /// let result = repo.apply_from_stack(options)?;
    /// ```
    pub fn apply_from_stack(
        &self,
        options: CrossStackApplyOptions,
    ) -> Result<CrossStackApplyOutcome, RepositoryError> {
        let mut outcome = CrossStackApplyOutcome::new();
        outcome.was_dry_run = options.dry_run;

        // Determine which changes to consider
        let source_changes = if !options.only_changes.is_empty() {
            // Use only specified changes
            options.only_changes.clone()
        } else if let Some(ref tag_name) = options.up_to_tag {
            // Get changes up to the tag
            self.get_changes_up_to_tag(tag_name, Some(&options.from_stack))?
        } else {
            // Get all changes from source stack
            self.get_stack_changes(Some(&options.from_stack))?
                .into_iter()
                .map(|(_, hash)| hash)
                .collect()
        };

        // Filter to changes not already in target
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let to_stack = txn
            .get_stack(&options.to_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: options.to_stack.clone(),
            })?;

        let missing = filter_missing_in_stack(&txn, &to_stack, &source_changes)
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
            // Nothing to apply
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            let stack = txn
                .get_stack(&options.to_stack)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .unwrap();
            outcome.new_state = stack.state;
            outcome.sequence = stack.change_count;
            return Ok(outcome);
        }

        // If dry run, just return what would be applied
        if options.dry_run {
            outcome.applied_hashes = missing;
            outcome.changes_applied = outcome.applied_hashes.len();
            return Ok(outcome);
        }

        // Apply each change in order.
        //
        // When the source stack is Local, its changes were recorded against
        // the overlay view (STACK_GRAPH ∪ GRAPH).  Applying those changes
        // to a different stack verifies edge context against a different
        // graph view, which produces spurious "missing context" conflicts.
        // These are architecturally expected — not real data conflicts —
        // so we automatically allow them for cross-stack apply.
        let source_is_local = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.get_stack(&options.from_stack)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .map(|s| s.kind.is_local())
                .unwrap_or(false)
        };

        let apply_opts = ApplyOptions::default()
            .stack(&options.to_stack)
            .allow_conflict(options.allow_conflicts || source_is_local);

        for hash in &missing {
            let result = if options.apply_dependencies {
                self.apply_change_rec(hash, apply_opts.clone())
            } else {
                self.apply_change(hash, apply_opts.clone())
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

    /// Apply changes up to a tag from one stack to another.
    ///
    /// This is a convenience method that combines `get_changes_up_to_tag`
    /// and `apply_from_stack`.
    ///
    /// # Arguments
    ///
    /// * `tag_name` - Name of the tag to apply up to
    /// * `from_stack` - Source stack containing the tag
    /// * `to_stack` - Target stack (None = current stack)
    ///
    /// # Returns
    ///
    /// A `CrossStackApplyOutcome` with details about what was applied.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Apply release-1.0.0 from feature to main
    /// let result = repo.apply_tag_to_stack("release-1.0.0", "feature", Some("main"))?;
    /// ```
    pub fn apply_tag_to_stack(
        &self,
        tag_name: &str,
        from_stack: &str,
        to_stack: Option<&str>,
    ) -> Result<CrossStackApplyOutcome, RepositoryError> {
        let target = to_stack.unwrap_or(&self.current_stack);

        let options = CrossStackApplyOptions::new(from_stack, target)
            .up_to_tag(tag_name)
            .with_dependencies(true);

        self.apply_from_stack(options)
    }

    /// Cherry-pick specific changes from one stack to another.
    ///
    /// # Arguments
    ///
    /// * `changes` - Hashes of changes to apply
    /// * `from_stack` - Source stack (for validation)
    /// * `to_stack` - Target stack (None = current stack)
    ///
    /// # Returns
    ///
    /// A `CrossStackApplyOutcome` with details about what was applied.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let result = repo.cherry_pick(&[hash1, hash2], "feature", None)?;
    /// ```
    pub fn cherry_pick(
        &self,
        changes: &[Hash],
        _from_stack: &str,
        to_stack: Option<&str>,
    ) -> Result<CrossStackApplyOutcome, RepositoryError> {
        let target = to_stack.unwrap_or(&self.current_stack);

        // For cherry-pick, we apply specific changes with dependencies
        let options = CrossStackApplyOptions::new("", target)
            .only_changes(changes.to_vec())
            .with_dependencies(true);

        self.apply_from_stack(options)
    }
}

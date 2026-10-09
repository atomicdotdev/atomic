use std::collections::HashSet;

use atomic_core::pristine::ViewState;

use super::*;
use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataTransition, MetadataValue, OperationKind, OperationScope,
    RepoStateRef, ViewStateRef,
};

impl Repository {
    fn authoritative_working_copy_view_name(&self) -> Result<String, RepositoryError> {
        let working_copy = self.require_working_copy_id()?;
        self.desired_view_name(working_copy)
    }

    // History Methods

    /// Get a forward history log for the current view.
    ///
    /// For **draft** views, only changes that are "new" on this view are
    /// returned — inherited changes from ancestor views are filtered out.
    /// Pass `--all` (via [`HistoryOptions::include_inherited`]) to see
    /// the full change log including inherited entries.
    ///
    /// Returns an iterator over history entries starting from the given
    /// sequence number and proceeding forward (oldest to newest).
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling the history query
    ///
    /// # Returns
    ///
    /// A vector of history entries.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let history = repo.log(HistoryOptions::default().limit(10))?;
    /// for entry in history {
    ///     println!("#{}: {}", entry.sequence, entry.hash.to_base32());
    /// }
    /// ```
    pub fn log(
        &self,
        options: HistoryOptions,
    ) -> Result<Vec<crate::history::HistoryEntry>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view_name = options.view.as_deref().unwrap_or(&self.current_view);
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        if view_name.starts_with("wc/") {
            return Ok(Vec::new());
        }

        // For draft views, build a set of ancestor change NodeIds so we
        // can filter out inherited entries.  This makes `atomic log` on a
        // draft view show only "what's new" rather than the full history
        // of every ancestor view.
        let ancestor_ids: Option<ViewMembershipSet> =
            if view.kind.is_draft() && !options.include_inherited {
                Some(Self::collect_ancestor_change_ids(&txn, &view)?)
            } else {
                None
            };

        let iter = crate::history::log(&txn, &view, &options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Collect entries, loading headers if requested
        let mut entries = Vec::new();
        for result in iter {
            let mut entry = result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            // Skip inherited entries on draft views
            if let Some(ref ids) = ancestor_ids {
                if ids.contains(entry.node_id) {
                    continue;
                }
            }

            // Snapshots are private working-copy state, not normal history.
            if let Ok(change) = self.load_change(&entry.hash) {
                if change.kind().is_snapshot() {
                    continue;
                }
                if options.load_headers {
                    entry = entry.with_change_header(change.hashed.header.clone());
                }
            }

            entries.push(entry);
        }

        Ok(entries)
    }

    /// Get the ordered direct membership of a view and its full parent chain.
    ///
    /// Unlike [`log`](Self::log) — which for a draft view returns only that
    /// view's own changes — this includes membership inherited from every
    /// ancestor. Entries preserve root-to-leaf view-log order with first
    /// duplicates removed.
    ///
    /// This is deliberately not a graph traversal closure: dependencies omitted
    /// from direct `VIEW_CHANGES` membership do not appear here. Callers that
    /// read graph bytes or copy a self-contained causal state must build a
    /// [`GraphVisibilityClosure`] through [`graph_visibility_closure`].
    ///
    /// # Arguments
    ///
    /// * `view_name` - The view to query, or `None` for the current view.
    pub fn effective_history(
        &self,
        view_name: Option<&str>,
    ) -> Result<Vec<crate::history::HistoryEntry>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view_name = view_name.unwrap_or(&self.current_view);
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        let membership = view_membership(&txn, &view)?;
        let chain = txn
            .resolve_full_view_chain(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut entries_by_id = std::collections::HashMap::new();
        for chain_view in &chain {
            let iter = crate::history::log(&txn, chain_view, &HistoryOptions::default())
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            for result in iter {
                let entry = result.map_err(|e| RepositoryError::Database(e.to_string()))?;
                entries_by_id.entry(entry.node_id).or_insert(entry);
            }
        }

        Ok(membership
            .iter()
            .filter_map(|change_id| entries_by_id.remove(change_id))
            .collect())
    }

    /// Get a reverse history log (most recent first).
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling the history query
    ///
    /// # Returns
    ///
    /// A vector of history entries in reverse order.
    pub fn reverse_log(
        &self,
        options: HistoryOptions,
    ) -> Result<Vec<crate::history::HistoryEntry>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view_name = options.view.as_deref().unwrap_or(&self.current_view);
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        if view_name.starts_with("wc/") {
            return Ok(Vec::new());
        }

        let mut entries = crate::history::reverse_log(&txn, &view, &options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // For draft views, filter out inherited entries
        if view.kind.is_draft() && !options.include_inherited {
            let ancestor_ids = Self::collect_ancestor_change_ids(&txn, &view)?;
            entries.retain(|e| !ancestor_ids.contains(e.node_id));
        }

        // Snapshots are private working-copy state, not normal history.
        entries.retain_mut(|entry| {
            if let Ok(change) = self.load_change(&entry.hash) {
                if change.kind().is_snapshot() {
                    return false;
                }
                if options.load_headers {
                    entry.header = Some(change.hashed.header.clone());
                }
            }
            true
        });

        Ok(entries)
    }

    /// Collect all change NodeIds from ancestor views' VIEW_CHANGES.
    ///
    /// Walks the parent chain and gathers each ancestor view's own
    /// change entries.  The result is used by `log()` / `reverse_log()`
    /// to filter out inherited entries on draft views.
    fn collect_ancestor_change_ids<T: ViewTxnT>(
        txn: &T,
        view: &atomic_core::pristine::ViewState,
    ) -> Result<ViewMembershipSet, RepositoryError> {
        let chain = txn
            .resolve_full_view_chain(view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let mut ordered = Vec::new();
        for ancestor in chain.iter().take(chain.len().saturating_sub(1)) {
            for entry in txn
                .iter_changes(ancestor, 0)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                let (_sequence, change_id, _state) =
                    entry.map_err(|e| RepositoryError::Database(e.to_string()))?;
                ordered.push(change_id);
            }
        }
        Ok(ViewMembershipSet::from_ordered(ordered))
    }

    /// Get a summary of the current view's history.
    ///
    /// # Returns
    ///
    /// A `HistorySummary` with aggregate statistics.
    pub fn history_summary(&self) -> Result<HistorySummary, RepositoryError> {
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

        crate::history::history_summary(&txn, &view)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    // Unrecord Methods

    /// Unrecord a change from the current view.
    ///
    /// This removes the change from the view's change log without deleting the change
    /// itself. The change remains in the change store and graph, and can be
    /// re-applied later. This is similar to Gerrit's workflow where a patch can
    /// be removed from a change set, modified, and re-inserted.
    ///
    /// # Arguments
    ///
    /// * `hash` - Hash of the change to unrecord
    /// * `options` - Options controlling the unrecord behavior
    ///
    /// # Returns
    ///
    /// An `UnrecordOutcome` with details about what was unrecorded, including
    /// the original sequence number (useful for re-insertion).
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Unrecord a specific change
    /// let outcome = repo.unrecord(&hash, UnrecordOptions::default())?;
    /// println!("Removed from sequence {}", outcome.original_sequence.unwrap());
    ///
    /// // Later, re-insert at the original position
    /// repo.reinsert_change(&hash, outcome.original_sequence)?;
    /// ```
    pub fn unrecord(
        &self,
        hash: &Hash,
        options: UnrecordOptions,
    ) -> Result<UnrecordOutcome, RepositoryError> {
        let view_name = match options.view.as_deref() {
            Some(view) => view.to_string(),
            None => self.authoritative_working_copy_view_name()?,
        };
        if options.dry_run {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let view = txn
                .get_view(&view_name)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: view_name.clone(),
                })?;
            // Dev #196 parity: preview and execution reject the same unsafe
            // operations — the inherited-change guard runs here too.
            let change_id = txn
                .get_internal(hash)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: hash.to_base32(),
                })?;
            self.check_unrecord_safety(&txn, &view, hash, change_id)?;
            return crate::unrecord::preview_unrecord(&txn, &view, &[*hash], &options)
                .map_err(|error| RepositoryError::Unrecord(error.to_string()));
        }
        let working_copy = self.require_working_copy_id()?;
        let operation_lock = self.try_lock_operation(working_copy)?;
        if let OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: OperationScope::WorkingCopy(working_copy).to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let preflight_txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = preflight_txn
            .get_view(&view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;
        let change_id = preflight_txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: hash.to_base32(),
            })?;

        // Dev #196 parity: the inherited-change guard must run on the
        // preflight path too — preview and execution reject the same unsafe
        // operations before any journal lease is prepared.
        self.check_unrecord_safety(&preflight_txn, &view, hash, change_id)?;

        let original_seq = preflight_txn
            .get_change_seq(&view, change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| {
                RepositoryError::Unrecord(format!(
                    "Change {} is not in view '{}'",
                    hash.to_base32(),
                    view_name
                ))
            })?;
        let mut after_view_state = Merkle::ZERO;
        for row in preflight_txn
            .iter_changes(&view, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            let (_, candidate, _) = row.map_err(|e| RepositoryError::Database(e.to_string()))?;
            if candidate == change_id {
                continue;
            }
            let candidate_hash = preflight_txn
                .get_external(candidate)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: candidate.to_string(),
                })?;
            after_view_state = after_view_state.next(&candidate_hash);
        }
        let before_view_state = view.state;
        drop(preflight_txn);

        let before_record = self.working_copy_record(working_copy)?;
        let mut after_record = before_record.clone();
        if before_record.desired_view == view.id {
            after_record.desired_state = after_view_state;
        }
        let operation = self.prepare_metadata_operation(
            &operation_lock,
            OperationKind::Unrecord,
            None,
            RepoStateRef {
                view: Some(ViewStateRef {
                    name: view_name.clone(),
                    state: before_view_state,
                    set_id: None,
                }),
                working_copy: Some(super::operation::working_copy_state_ref(before_record)),
                git: None,
            },
            RepoStateRef {
                view: Some(ViewStateRef {
                    name: view_name.clone(),
                    state: after_view_state,
                    set_id: None,
                }),
                working_copy: Some(super::operation::working_copy_state_ref(after_record)),
                git: None,
            },
            vec![MetadataTransition {
                target: MetadataTarget::ViewChange {
                    view: view_name.clone(),
                    change: *hash,
                },
                expected_old: MetadataValue::Sequence(original_seq),
                expected_new: MetadataValue::Absent,
            }],
            vec![*hash],
            ActorRef::System {
                name: "repository-unrecord".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let mut view = txn
            .get_view(&view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;
        let change_id = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: hash.to_base32(),
            })?;
        let original_seq = txn
            .del_change(&mut view, change_id, hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if original_seq.is_none() {
            return Err(RepositoryError::Unrecord(format!(
                "Change {} is not in view '{}'",
                hash.to_base32(),
                view_name
            )));
        }

        // Update the view, then align every derived tree cache to the new
        // visibility before committing. In particular, unrecording FileMove
        // must restore its exact graph-backed source path and stable inode.
        txn.update_view(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let affected_tree_paths = self.realign_tree_projection_in_txn(&mut txn, &view_name)?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // ── Post-unrecord: invalidate file index for affected paths ───
        //
        // After removing a change from VIEW_CHANGES, the FILE_INDEX is
        // stale — it still has entries keyed to the old content hashes.
        // This causes `status` to think files are clean when they're not.
        //
        // We do NOT re-materialize the working copy (that would overwrite
        // the user's disk changes). Instead, we just delete FILE_INDEX
        // entries for affected paths so that the next `status` call does
        // a full content comparison against the graph.
        if let Ok(mut idx_txn) = self.pristine.write_txn() {
            for path in &affected_tree_paths {
                let _ = idx_txn.del_file_index(path);
            }
            if let Ok(change) = self.load_change(hash) {
                for op in change.hunks() {
                    if let Some(path) = op.path() {
                        let _ = idx_txn.del_file_index(path);
                    }
                }
            }
            let _ = idx_txn.commit();
        }

        self.apply_operation_metadata_locked(&operation_lock, operation.id())?;
        self.finalize_operation_verified(&operation_lock, operation.id())?;

        // Build outcome
        let mut outcome = UnrecordOutcome::new(vec![*hash], view.state, view.change_count);
        outcome.stats.direct_unrecords = 1;

        Ok(outcome)
    }

    fn check_unrecord_safety<T: ViewTxnT>(
        &self,
        txn: &T,
        view: &ViewState,
        hash: &Hash,
        change_id: NodeId,
    ) -> Result<(), RepositoryError> {
        if txn.get_change_seq(view, change_id)?.is_none() {
            return Err(RepositoryError::Unrecord(format!(
                "Change {} is not in view '{}'",
                hash.to_base32(),
                view.name
            )));
        }

        // Forked views can contain copied references to ancestor changes.
        // Removing that copy cannot hide the inherited change, so refuse it.
        let mut remaining_ids = collect_view_change_ids(txn, view)?;
        let mut ancestor = if view.kind.is_draft() {
            view.parent
        } else {
            None
        };
        let mut seen_views = HashSet::from([view.id]);
        while let Some(id) = ancestor {
            if !seen_views.insert(id) {
                return Err(RepositoryError::Unrecord("Cyclic view ancestry".into()));
            }
            let parent = txn
                .get_view_by_id(id)?
                .ok_or_else(|| RepositoryError::Unrecord(format!("Missing ancestor view {id}")))?;
            if txn.get_change_seq(&parent, change_id)?.is_some() {
                return Err(RepositoryError::Unrecord(format!(
                    "Change {} is inherited from view '{}'; unrecord it there instead",
                    hash.to_base32(),
                    parent.name
                )));
            }
            remaining_ids = ViewMembershipSet::from_ordered(
                remaining_ids
                    .iter()
                    .copied()
                    .chain(collect_view_change_ids(txn, &parent)?.iter().copied()),
            );
            ancestor = if parent.kind.is_draft() {
                parent.parent
            } else {
                None
            };
        }

        let mut pending = Vec::new();
        for id in remaining_ids.iter().copied() {
            if id != change_id {
                let candidate = txn.get_external(id)?.ok_or_else(|| {
                    RepositoryError::Unrecord(format!("Missing hash for change {id}"))
                })?;
                pending.push(candidate);
            }
        }
        let mut checked = HashSet::new();
        while let Some(candidate) = pending.pop() {
            if !checked.insert(candidate) {
                continue;
            }
            let id = txn.get_internal(&candidate)?;
            let dependencies = match id {
                Some(id) if txn.is_change_deps_indexed(id)? => txn.get_change_deps(id)?,
                // Old repositories can predate the dependency index. Never
                // mistake an unindexed change for one with no dependencies.
                _ => self.load_change(&candidate)?.dependencies().to_vec(),
            };
            if dependencies.contains(hash) {
                return Err(RepositoryError::Unrecord(format!(
                    "Cannot unrecord {}: change {} in the dependency closure of view '{}' depends on it; unrecord dependent changes first",
                    hash.to_base32(), candidate.to_base32(), view.name
                )));
            }
            pending.extend(dependencies);
        }
        Ok(())
    }

    /// Unrecord the last change from the current view.
    ///
    /// This is a convenience method for unrecording the most recent change.
    ///
    /// # Arguments
    ///
    /// * `options` - Options controlling the unrecord behavior
    ///
    /// # Returns
    ///
    /// An `UnrecordOutcome` with details about what was unrecorded.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Undo the last change
    /// let outcome = repo.unrecord_last(UnrecordOptions::default())?;
    /// ```
    pub fn unrecord_last(
        &self,
        options: UnrecordOptions,
    ) -> Result<UnrecordOutcome, RepositoryError> {
        // Get the last change hash
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view_name = match options.view.as_deref() {
            Some(view) => view.to_string(),
            None => self.authoritative_working_copy_view_name()?,
        };
        let view = txn
            .get_view(&view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;

        let last_hash = crate::unrecord::get_last_change(&txn, &view)
            .map_err(|e| RepositoryError::Unrecord(e.to_string()))?
            .ok_or_else(|| RepositoryError::Unrecord("View is empty".to_string()))?;

        drop(txn);

        self.unrecord(&last_hash, options)
    }

    /// Reinsert a previously unrecorded change at a specific position.
    ///
    /// This is part of the Gerrit-like workflow where a change can be removed,
    /// modified, and re-inserted at its original position (or appended).
    ///
    /// # Arguments
    ///
    /// * `hash` - Hash of the change to reinsert
    /// * `at_sequence` - The sequence position to insert at (None = append to end)
    ///
    /// # Returns
    ///
    /// The new state and sequence after reinsertion.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Unrecord, modify, and reinsert at original position
    /// let outcome = repo.unrecord(&hash, UnrecordOptions::default())?;
    /// // ... modify the change ...
    /// repo.reinsert_change(&hash, outcome.original_sequence)?;
    /// ```
    pub fn reinsert_change(
        &self,
        hash: &Hash,
        at_sequence: Option<u64>,
    ) -> Result<(Merkle, u64), RepositoryError> {
        let view = self.authoritative_working_copy_view_name()?;
        self.reinsert_change_on_view(&view, hash, at_sequence)
    }

    /// Reinsert a change into an explicit view at its original or requested sequence.
    pub fn reinsert_change_on_view(
        &self,
        view_name: &str,
        hash: &Hash,
        at_sequence: Option<u64>,
    ) -> Result<(Merkle, u64), RepositoryError> {
        let change = self.load_change(hash)?;

        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the view
        let mut view = txn
            .open_or_create_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        self.ensure_change_allowed_in_view(&txn, view_name, &change)?;

        // Get internal ID (must already be registered)
        let change_id = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: hash.to_base32(),
            })?;

        // Determine insertion point
        let insert_at = at_sequence.unwrap_or(view.change_count);

        // Reinsert the change
        txn.reinsert_change(&mut view, change_id, hash, insert_at)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Reinsert changes visibility immediately; keep TREE and its reverse
        // index in the same transaction so FileMove cannot remain projected at
        // its pre-reinsert source path.
        txn.update_view(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let affected_tree_paths = self.realign_tree_projection_in_txn(&mut txn, view_name)?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if let Ok(mut idx_txn) = self.pristine.write_txn() {
            for path in affected_tree_paths {
                let _ = idx_txn.del_file_index(&path);
            }
            let _ = idx_txn.commit();
        }

        Ok((view.state, view.change_count))
    }

    /// Converge a view's own change log to a target effective set by
    /// **removing** every own change absent from `target`.
    ///
    /// This is the removal half of set-based view convergence (the add half is
    /// [`insert_change`](crate::Repository::insert_change)). A durable view
    /// record declares the view's effective change set (its own changes plus
    /// everything inherited through its parent chain); any of the view's OWN
    /// changes not in that set were removed upstream — e.g. by `view split`,
    /// which moves changes out of a view — and are unrecorded here so the local
    /// view matches the declared set.
    ///
    /// Only the view's **own** log is considered: inherited changes live in an
    /// ancestor view's log (converge the ancestor to drop those) and are never
    /// touched here, so passing a `target` that omits an inherited change does
    /// not remove it. Because `target` is the *effective* set, any own change
    /// that should remain — inherited or not — is present in it and kept.
    ///
    /// Removal is at the `VIEW_CHANGES` level only, exactly like
    /// [`unrecord`](crate::Repository::unrecord): the change bytes and graph
    /// edges remain, so the removal is reversible via
    /// [`reinsert_change`](crate::Repository::reinsert_change) or
    /// [`insert_change`](crate::Repository::insert_change). The working copy is
    /// **not** re-materialized (a caller that needs the tree on disk updated
    /// must do so separately); stale `FILE_INDEX` entries for affected paths are
    /// cleared so the next `status` recomputes against the graph.
    ///
    /// Idempotent: a view already matching its target removes nothing and
    /// returns an empty vector. On success returns the hashes removed.
    pub fn retain_view_changes(
        &self,
        view_name: &str,
        target: &HashSet<Hash>,
    ) -> Result<Vec<Hash>, RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;

        // Snapshot the view's own change ids first (the iterator borrows the
        // txn immutably; collecting frees it for the mutable `del_change`).
        let own_ids: Vec<NodeId> = txn
            .iter_changes(&view, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .map(|entry| entry.map(|(_seq, change_id, _merkle)| change_id))
            .collect::<Result<_, _>>()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Any own change whose external hash is not in the target set is stale.
        let mut to_remove: Vec<(NodeId, Hash)> = Vec::new();
        for change_id in own_ids {
            let hash = txn
                .get_external(change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "change {} has no external hash",
                        change_id.0
                    ))
                })?;
            if !target.contains(&hash) {
                to_remove.push((change_id, hash));
            }
        }

        if to_remove.is_empty() {
            // Leave the write txn unwritten — nothing to converge.
            return Ok(Vec::new());
        }

        // `del_change` re-derives the sequence from `change_id` on each call, so
        // it is robust to the resequencing it performs internally; removal order
        // does not affect the final set.
        let mut removed = Vec::with_capacity(to_remove.len());
        for (change_id, hash) in &to_remove {
            let seq = txn
                .del_change(&mut view, *change_id, hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            if seq.is_some() {
                removed.push(*hash);
            }
        }

        txn.update_view(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Post-removal: drop stale FILE_INDEX entries for affected paths so the
        // next `status` does a full content comparison against the graph rather
        // than trusting an index keyed to now-removed content (mirrors
        // `unrecord`). We do NOT re-materialize — that would clobber on-disk work.
        for hash in &removed {
            if let Ok(change) = self.load_change(hash) {
                if let Ok(mut idx_txn) = self.pristine.write_txn() {
                    for op in change.hunks() {
                        if let Some(p) = op.path() {
                            let _ = idx_txn.del_file_index(p);
                        }
                    }
                    let _ = idx_txn.commit();
                }
            }
        }

        Ok(removed)
    }

    /// Check if a change can be unrecorded.
    ///
    /// This checks whether the change is in the view and whether it has
    /// any dependents that would also need to be unrecorded.
    ///
    /// # Arguments
    ///
    /// * `hash` - Hash of the change to check
    ///
    /// # Returns
    ///
    /// Information about the change's dependencies and whether it can be
    /// safely unrecorded.
    pub fn can_unrecord(
        &self,
        hash: &Hash,
    ) -> Result<crate::unrecord::UnrecordDependencyInfo, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let view_name = self.authoritative_working_copy_view_name()?;
        let view = txn
            .get_view(&view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.clone(),
            })?;

        crate::unrecord::check_can_unrecord(&txn, &view, hash, &UnrecordOptions::default())
            .map_err(|e| RepositoryError::Unrecord(e.to_string()))
    }
}

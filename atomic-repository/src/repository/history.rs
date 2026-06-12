use super::*;

impl Repository {
    // History Methods

    /// Get a forward history log for the current view.
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

        let iter = crate::history::log(&txn, &view, &options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Collect entries, loading headers if requested
        let mut entries = Vec::new();
        for result in iter {
            let mut entry = result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            // Load header if requested
            if options.load_headers {
                if let Ok(change) = self.load_change(&entry.hash) {
                    entry = entry.with_change_header(change.hashed.header.clone());
                }
            }

            entries.push(entry);
        }

        Ok(entries)
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

        let mut entries = crate::history::reverse_log(&txn, &view, &options)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Load headers if requested
        if options.load_headers {
            for entry in &mut entries {
                if let Ok(change) = self.load_change(&entry.hash) {
                    entry.header = Some(change.hashed.header.clone());
                }
            }
        }

        Ok(entries)
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
        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Determine which view to use
        let view_name = options.view.as_deref().unwrap_or(&self.current_view);

        // Get the view
        let mut view = txn
            .open_or_create_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get internal ID
        let change_id = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: hash.to_base32(),
            })?;

        // Check if this is a dry run
        if options.dry_run {
            // Preview mode - just return what would happen
            let preview = crate::unrecord::preview_unrecord(&txn, &view, &[*hash], &options)
                .map_err(|e| RepositoryError::Unrecord(e.to_string()))?;
            return Ok(preview);
        }

        // Remove the change from the view
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

        // Update the view
        txn.update_view(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

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

        // Build outcome
        let mut outcome = UnrecordOutcome::new(vec![*hash], view.state, view.change_count);
        outcome.stats.direct_unrecords = 1;

        Ok(outcome)
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

        let view_name = options.view.as_deref().unwrap_or(&self.current_view);
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
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
        // Get write transaction
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the view
        let mut view = txn
            .open_or_create_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

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

        // Update the view
        txn.update_view(&view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok((view.state, view.change_count))
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

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;

        crate::unrecord::check_can_unrecord(&txn, &view, hash, &UnrecordOptions::default())
            .map_err(|e| RepositoryError::Unrecord(e.to_string()))
    }
}

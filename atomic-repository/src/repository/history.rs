use super::*;

impl Repository {
    // History Methods

    /// Get a forward history log for the current stack.
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

        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);
        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        let iter = crate::history::log(&txn, &stack, &options)
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

        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);
        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        let mut entries = crate::history::reverse_log(&txn, &stack, &options)
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

    /// Get a summary of the current stack's history.
    ///
    /// # Returns
    ///
    /// A `HistorySummary` with aggregate statistics.
    pub fn history_summary(&self) -> Result<HistorySummary, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        crate::history::history_summary(&txn, &stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    // Unrecord Methods

    /// Unrecord a change from the current stack.
    ///
    /// This removes the change from the stack's view without deleting the change
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

        // Determine which stack to use
        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);

        // Get the stack
        let mut stack = txn
            .open_or_create_stack(stack_name)
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
            let preview = crate::unrecord::preview_unrecord(&txn, &stack, &[*hash], &options)
                .map_err(|e| RepositoryError::Unrecord(e.to_string()))?;
            return Ok(preview);
        }

        // Remove the change from the stack
        let original_seq = txn
            .del_change(&mut stack, change_id, hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if original_seq.is_none() {
            return Err(RepositoryError::Unrecord(format!(
                "Change {} is not in stack '{}'",
                hash.to_base32(),
                stack_name
            )));
        }

        // Update the stack
        txn.update_stack(&stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Build outcome
        let mut outcome = UnrecordOutcome::new(vec![*hash], stack.state, stack.change_count);
        outcome.stats.direct_unrecords = 1;

        Ok(outcome)
    }

    /// Unrecord the last change from the current stack.
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

        let stack_name = options.stack.as_deref().unwrap_or(&self.current_stack);
        let stack = txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: stack_name.to_string(),
            })?;

        let last_hash = crate::unrecord::get_last_change(&txn, &stack)
            .map_err(|e| RepositoryError::Unrecord(e.to_string()))?
            .ok_or_else(|| RepositoryError::Unrecord("Stack is empty".to_string()))?;

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

        // Get the stack
        let mut stack = txn
            .open_or_create_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get internal ID (must already be registered)
        let change_id = txn
            .get_internal(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: hash.to_base32(),
            })?;

        // Determine insertion point
        let insert_at = at_sequence.unwrap_or(stack.change_count);

        // Reinsert the change
        txn.reinsert_change(&mut stack, change_id, hash, insert_at)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Update the stack
        txn.update_stack(&stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Commit the transaction
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok((stack.state, stack.change_count))
    }

    /// Check if a change can be unrecorded.
    ///
    /// This checks whether the change is in the stack and whether it has
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

        let stack = txn
            .get_stack(&self.current_stack)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::StackNotFound {
                name: self.current_stack.clone(),
            })?;

        crate::unrecord::check_can_unrecord(&txn, &stack, hash, &UnrecordOptions::default())
            .map_err(|e| RepositoryError::Unrecord(e.to_string()))
    }
}

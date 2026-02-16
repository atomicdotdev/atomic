use super::*;

impl Repository {
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

        // Collect all change NodeIds in the current stack
        let mut change_filter: HashSet<NodeId> = HashSet::new();
        let iter = txn
            .iter_changes(&stack, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in iter {
            let (_seq, node_id, _merkle) =
                result.map_err(|e| RepositoryError::Database(e.to_string()))?;
            change_filter.insert(node_id);
        }

        // Use the filtered retrieval method
        self.get_file_content_with_filter(&txn, &normalized, change_filter)
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

        // Collect all change NodeIds in the specified stack
        let mut change_filter: HashSet<NodeId> = HashSet::new();
        let iter = txn
            .iter_changes(&stack, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in iter {
            let (_seq, node_id, _merkle) =
                result.map_err(|e| RepositoryError::Database(e.to_string()))?;
            change_filter.insert(node_id);
        }

        // Use the filtered retrieval method
        self.get_file_content_with_filter(&txn, &normalized, change_filter)
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
    ///                is not in the current stack's history
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
    /// using [`get_file_content_at_sequence`] with a cached change set.
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

        // Retrieve content with the change filter
        self.get_file_content_with_filter(&txn, &normalized, change_set)
    }

    /// Get file content as it was AFTER a specific change was applied.
    ///
    /// This method retrieves the content of a file at the state immediately
    /// after a change was applied. Combined with [`get_file_content_before_change`],
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
    ///                or the change is not in the current stack's history
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

        // Retrieve content with the change filter
        self.get_file_content_with_filter(&txn, &normalized, change_set)
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

        // Retrieve content with the change filter
        self.get_file_content_with_filter(&txn, &normalized, change_set)
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
    ) -> Result<Option<Vec<u8>>, RepositoryError>
    where
        T: atomic_core::pristine::GraphTxnT + atomic_core::pristine::TreeTxnT,
    {
        use atomic_core::output::alive::RetrieveOptions;
        use atomic_core::record::workflow::retrieve::retrieve_content_with_filter;

        // Check if file is tracked
        if !is_tracked(txn, normalized_path)
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

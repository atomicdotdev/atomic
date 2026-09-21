use super::*;
use atomic_core::pristine::{CachedGraphTxn, ViewGraph};

fn graph_visibility_at_change<T: ViewTxnT>(
    txn: &T,
    view: &atomic_core::pristine::ViewState,
    change_hash: &Hash,
    inclusive: bool,
) -> Result<Option<GraphVisibilityClosure>, RepositoryError> {
    let Some(change_id) = txn
        .get_internal(change_hash)
        .map_err(|error| RepositoryError::Database(error.to_string()))?
    else {
        return Ok(None);
    };
    let full = graph_visibility_closure(txn, view)?;
    let ordered: Vec<NodeId> = full.iter_dependency_first().copied().collect();
    let Some(position) = ordered.iter().position(|candidate| *candidate == change_id) else {
        return Ok(None);
    };
    let end = position + usize::from(inclusive);
    let membership = ViewMembershipSet::from_ordered(ordered.into_iter().take(end));
    graph_visibility_from_membership(txn, &membership).map(Some)
}

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
        match self.get_materialized_entry_on_view(path, &self.current_view)? {
            MaterializedEntry::Absent { .. } => Ok(None),
            MaterializedEntry::Present { bytes, .. } => Ok(Some(bytes)),
        }
    }

    /// Get file content through the canonical view-aware retrieval path.
    ///
    /// The CRDT tables are currently ambient and cannot prove per-view
    /// dependency visibility. Until the semantic layer records view-scoped
    /// state transitions, this compatibility entry point delegates to
    /// [`Self::get_file_content`] so incomplete dependency metadata fails before
    /// bytes are read and sibling-view content cannot leak.
    pub fn get_file_content_via_crdt<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        self.get_file_content(path)
    }

    /// Get file content through the view-aware semantic consumer for a
    /// SPECIFIC view (the requested-closure form of
    /// [`Self::get_file_content_via_crdt`]).
    ///
    /// The CRDT branch/leaf tables are ambient — shared across views and
    /// updated by every applied change — so a direct
    /// `output_file_via_crdt` read of a concurrently edited path renders a
    /// sibling view's bytes rather than the requested closure's. This entry
    /// point resolves the requested view's effective closure and returns
    /// exactly that closure's bytes (the canonical closure-filtered
    /// projection), so a sibling lineage can never leak into the requested
    /// view's semantic view. It is the view-parameterized companion to the
    /// compatibility entry point above and preserves the same no-leak
    /// contract.
    pub fn get_file_content_via_crdt_on_view<P: AsRef<Path>>(
        &self,
        path: P,
        view_name: &str,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        self.get_file_content_on_view(path, view_name)
    }

    /// Whether an inode renders any alive content for the requested visibility,
    /// using the inode-scoped `INODE_GRAPH` fast path before the exhaustive
    /// global retrieval.
    ///
    /// `is_file_alive_via_retrieval` runs `retrieve_graph` (global `GRAPH`
    /// traversal) for EVERY absent path-claim entry on every projection; on a
    /// large repository that is a per-entry bottleneck (measured 14–62 s per
    /// entry on the workspace graph, 659 entries per projection). The
    /// inode-linear walk reads the file's own `INODE_GRAPH` rows, so a path
    /// that simply has no alive content resolves without touching the global
    /// graph. A non-linear (fork/delete-through) structure still falls back to
    /// the exhaustive retrieval, so the verdict stays exact.
    pub(crate) fn inode_renders_alive_content<T>(
        &self,
        txn: &T,
        inode: Inode,
        position: atomic_core::types::Position<NodeId>,
        visibility: &GraphVisibilityClosure,
    ) -> Result<bool, RepositoryError>
    where
        T: atomic_core::pristine::GraphTxnT
            + atomic_core::pristine::InodeGraphOps<
                InodeError = atomic_core::pristine::PristineError,
            >
            + atomic_core::pristine::TreeTxnT,
    {
        let options =
            atomic_core::output::alive::RetrieveOptions::new()
                .with_graph_visibility(visibility.clone());
        match try_retrieve_linear_content_with_filter(
            txn,
            &self.change_store,
            inode,
            position,
            &options,
        ) {
            Ok(Some(content)) => return Ok(!content.is_empty()),
            Ok(None) => {}
            Err(_) => {}
        }
        super::status::is_file_alive_via_retrieval(txn, inode, position, visibility)
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
        let normalized = normalize_path(path.as_ref());

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

        let Some(visibility) = graph_visibility_at_change(&txn, &view, exclude_hash, false)? else {
            return Ok(None);
        };

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;
        match self.get_materialized_entry_with_visibility(&cached_txn, &normalized, visibility)? {
            MaterializedEntry::Absent { .. } => Ok(None),
            MaterializedEntry::Present { bytes, .. } => Ok(Some(bytes)),
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
        match self.get_materialized_entry_on_view(path, view_name)? {
            MaterializedEntry::Absent { .. } => Ok(None),
            MaterializedEntry::Present { bytes, .. } => Ok(Some(bytes)),
        }
    }

    /// Resolve a file as an explicit present/absent materialization entry on a view.
    ///
    /// Presence comes from the operation-aware path lifecycle projection. Empty
    /// rendered bytes therefore remain a present tracked file, while a visible
    /// whole-file deletion is absent even if global TREE metadata survives for
    /// another view.
    pub fn get_materialized_entry_on_view<P: AsRef<Path>>(
        &self,
        path: P,
        view_name: &str,
    ) -> Result<MaterializedEntry, RepositoryError> {
        let normalized = normalize_path(path.as_ref());
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let visibility = graph_visibility_closure(&txn, &view)?;
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &visibility,
        )?;
        let projection = self.project_tree_for_visibility_scoped(
            &txn,
            &claim_visibility,
            Some(normalized.as_str()),
        )?;
        if let Some(conflict) = projection.name_conflicts.get(&normalized) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path '{}' has an unresolved name conflict ({} path(s), {} claimant(s)); refusing to choose content",
                    normalized,
                    conflict.paths.len(),
                    conflict.sides.len()
                ),
            });
        }
        let Some(item) = projection.present.get(&normalized) else {
            let inode = projection
                .absent
                .iter()
                .find(|entry| entry.path() == normalized)
                .and_then(MaterializedEntry::inode);
            return Ok(MaterializedEntry::absent(normalized, inode));
        };
        if item.is_directory {
            return Ok(MaterializedEntry::absent(normalized, Some(item.inode)));
        }
        let bytes = retrieve_content_with_filter_fast(
            &txn,
            &self.change_store,
            item.inode,
            item.position,
            atomic_core::output::alive::RetrieveOptions::new().with_graph_visibility(visibility),
        )
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(MaterializedEntry::present(normalized, item.inode, bytes))
    }

    /// Read a path through an exact assembly visibility: `view`'s validated
    /// closure minus `excluded` changes (CB-9B review R2).
    ///
    /// Merge baselines must consult the same exact visibility that recording
    /// and staged verification use — the full-view render would mix sibling
    /// lineages (or their conflicts) into the baseline and fabricate false
    /// causality. Storage errors propagate; only genuine absence is `None`.
    pub fn get_file_content_on_view_excluding<P: AsRef<Path>>(
        &self,
        path: P,
        view_name: &str,
        excluded: &[Hash],
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let normalized = normalize_path(path.as_ref());
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let visibility =
            super::filter::assembly_visibility_excluding(&txn, &view, excluded)?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        if projection.name_conflicts.contains_key(&normalized) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path '{}' has an unresolved name conflict inside the exact assembly closure; refusing to choose content",
                    normalized
                ),
            });
        }
        let Some(item) = projection.present.get(&normalized) else {
            return Ok(None);
        };
        if item.is_directory {
            return Ok(None);
        }
        let bytes = retrieve_content_with_filter_fast(
            &txn,
            &self.change_store,
            item.inode,
            item.position,
            atomic_core::output::alive::RetrieveOptions::new()
                .with_graph_visibility(visibility),
        )
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(Some(bytes))
    }

    /// Project the graph-backed inode attributes (mode/kind) for one path
    /// through an exact assembly visibility (CB-9C).
    ///
    /// Import paths consult the same visibility that staged verification
    /// uses, so a delta's expected Git mode/kind is compared against exactly
    /// the state the importing change will build on. Returns `None` when the
    /// path is absent from the projection (untracked/new); conflicted
    /// registers are returned as-is so callers can fail closed.
    pub fn projected_attributes_on_view_excluding(
        &self,
        path: &str,
        view_name: &str,
        excluded: &[Hash],
    ) -> Result<Option<atomic_core::output::InodeAttributeProjection>, RepositoryError> {
        let normalized = normalize_path(Path::new(path));
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view = txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: view_name.to_string(),
            })?;
        let visibility =
            super::filter::assembly_visibility_excluding(&txn, &view, excluded)?;
        let projection = self.project_tree_for_visibility(&txn, &visibility)?;
        if projection.name_conflicts.contains_key(&normalized) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path '{}' has an unresolved name conflict inside the exact assembly closure; refusing to project attributes",
                    normalized
                ),
            });
        }
        let Some(item) = projection.present.get(&normalized) else {
            return Ok(None);
        };
        if item.is_directory {
            return Ok(None);
        }
        let projected = atomic_core::output::project_inode_attributes(
            &txn,
            item.position,
            &visibility.iter_dependency_first().copied().collect(),
        )
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
        Ok(Some(projected))
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

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;
        let visibility = graph_visibility_closure(&txn, &view)?;
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &visibility,
        )?;
        let projection = self.project_tree_for_visibility(&txn, &claim_visibility)?;
        if projection.name_conflicts.contains_key(&normalized) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path '{}' has an unresolved name conflict; refusing to choose content",
                    normalized
                ),
            });
        }
        let Some(item) = projection.present.get(&normalized) else {
            return Ok(None);
        };
        if item.is_directory {
            return Ok(None);
        }

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view_graph = ViewGraph::new(&cached_txn, visibility);
        let result =
            retrieve_content_with_options(&view_graph, &self.change_store, item.position, options)
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

        let view = txn
            .get_view(&self.current_view)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: self.current_view.clone(),
            })?;
        let visibility = graph_visibility_closure(&txn, &view)?;
        let claim_visibility = super::name_resolution::path_claim_visibility_for_view(
            &txn,
            &self.change_store,
            &view,
            &visibility,
        )?;
        let projection = self.project_tree_for_visibility(&txn, &claim_visibility)?;
        if projection.name_conflicts.contains_key(&normalized) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path '{}' has an unresolved name conflict; refusing to choose content",
                    normalized
                ),
            });
        }
        let Some(item) = projection.present.get(&normalized) else {
            return Ok(false);
        };
        if item.is_directory {
            return Ok(false);
        }

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;
        let view_graph = ViewGraph::new(&cached_txn, visibility);
        let has = has_content(&view_graph, &self.change_store, item.position)
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

        let Some(visibility) = graph_visibility_at_change(&txn, &view, change_hash, false)? else {
            return Ok(None);
        };

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;
        match self.get_materialized_entry_with_visibility(&cached_txn, &normalized, visibility)? {
            MaterializedEntry::Absent { .. } => Ok(None),
            MaterializedEntry::Present { bytes, .. } => Ok(Some(bytes)),
        }
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

        let Some(visibility) = graph_visibility_at_change(&txn, &view, change_hash, true)? else {
            return Ok(None);
        };

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;
        match self.get_materialized_entry_with_visibility(&cached_txn, &normalized, visibility)? {
            MaterializedEntry::Absent { .. } => Ok(None),
            MaterializedEntry::Present { bytes, .. } => Ok(Some(bytes)),
        }
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

        let membership = view_membership_at_sequence(&txn, &view, max_sequence)?;
        let visibility = graph_visibility_from_membership(&txn, &membership)?;

        let cached_txn =
            CachedGraphTxn::new(&txn).map_err(|e| RepositoryError::Database(e.to_string()))?;
        match self.get_materialized_entry_with_visibility(&cached_txn, &normalized, visibility)? {
            MaterializedEntry::Absent { .. } => Ok(None),
            MaterializedEntry::Present { bytes, .. } => Ok(Some(bytes)),
        }
    }

    /// Resolve lifecycle presence and render bytes under validated visibility.
    pub(super) fn get_materialized_entry_with_visibility<T>(
        &self,
        txn: &T,
        normalized_path: &str,
        visibility: GraphVisibilityClosure,
    ) -> Result<MaterializedEntry, RepositoryError>
    where
        T: atomic_core::pristine::GraphTxnT
            + atomic_core::pristine::TreeTxnT
            + atomic_core::pristine::InodeGraphOps<InodeError = atomic_core::pristine::PristineError>,
    {
        use atomic_core::output::alive::RetrieveOptions;

        let claim_txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let projection = self.project_tree_for_visibility(&claim_txn, &visibility)?;
        if let Some(conflict) = projection.name_conflicts.get(normalized_path) {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "path '{}' has an unresolved name conflict ({} path(s), {} claimant(s)); refusing to choose content",
                    normalized_path,
                    conflict.paths.len(),
                    conflict.sides.len()
                ),
            });
        }
        let Some(item) = projection.present.get(normalized_path) else {
            let inode = projection
                .absent
                .iter()
                .find(|entry| entry.path() == normalized_path)
                .and_then(MaterializedEntry::inode);
            return Ok(MaterializedEntry::absent(normalized_path, inode));
        };
        if item.is_directory {
            return Ok(MaterializedEntry::absent(normalized_path, Some(item.inode)));
        }

        let options = RetrieveOptions::new().with_graph_visibility(visibility);
        let bytes = retrieve_content_with_filter_fast(
            txn,
            &self.change_store,
            item.inode,
            item.position,
            options,
        )
        .map_err(|e: atomic_core::record::RecordError| RepositoryError::Database(e.to_string()))?;

        Ok(MaterializedEntry::present(
            normalized_path,
            item.inode,
            bytes,
        ))
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
        working_copy: WorkingCopyId,
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
        self.archive(working_copy, destination, options)
    }
}

pub(crate) fn retrieve_content_with_filter_fast<T, C>(    txn: &T,
    changes: &C,
    inode: Inode,
    position: Position<NodeId>,
    options: atomic_core::output::alive::RetrieveOptions,
) -> atomic_core::record::RecordResult<Vec<u8>>
where
    T: atomic_core::pristine::GraphTxnT
        + atomic_core::pristine::InodeGraphOps<InodeError = atomic_core::pristine::PristineError>
        + atomic_core::pristine::TreeTxnT,
    C: atomic_core::change::ChangeStore,
{
    retrieve_content_with_filter_fast_with_fork_info(txn, changes, inode, position, options)
        .map(|(content, _had_fork_structure)| content)
}

/// Like [`retrieve_content_with_filter_fast`], but also reports whether
/// retrieval had to resolve fork/cyclic structure to produce the content.
///
/// The inode-linear fast path only ever follows an unambiguous single
/// successor chain (bailing to the full graph walk on any fork), so a fast-
/// path hit never involves fork resolution and always reports `false`. Only
/// the fallback to `retrieve_content_with_filter_and_fork_info` can report
/// `true`. Callers that use this content as `old_content` for a subsequent
/// positional diff must consult this flag: a fork/cyclic resolution is not
/// guaranteed to structurally match a plain checkout, so diffing against it
/// can corrupt the file (hence the whole-file-replace safety fallback).
pub(crate) fn retrieve_content_with_filter_fast_with_fork_info<T, C>(
    txn: &T,
    changes: &C,
    inode: Inode,
    position: Position<NodeId>,
    options: atomic_core::output::alive::RetrieveOptions,
) -> atomic_core::record::RecordResult<(Vec<u8>, bool)>
where
    T: atomic_core::pristine::GraphTxnT
        + atomic_core::pristine::InodeGraphOps<InodeError = atomic_core::pristine::PristineError>
        + atomic_core::pristine::TreeTxnT,
    C: atomic_core::change::ChangeStore,
{
    let trace_retrieve = std::env::var_os("ATOMIC_TRACE_RETRIEVE").is_some();
    let debug_retrieve = std::env::var("ATOMIC_DEBUG_RETRIEVE").is_ok();
    let retrieve_start = std::time::Instant::now();

    // Try the inode-linear fast path first (works with or without filter).
    if let Some(content) =
        try_retrieve_linear_content_with_filter(txn, changes, inode, position, &options)?
    {
        if debug_retrieve && retrieve_start.elapsed().as_millis() > 100 {
            eprintln!(
                "RETRIEVE fast path ms={} bytes={}",
                retrieve_start.elapsed().as_millis(),
                content.len()
            );
        }
        if trace_retrieve {
            eprintln!(
                "[retrieve_content_with_filter_fast] inode fast path hit bytes={}",
                content.len()
            );
        }
        return Ok((content, false));
    }

    if trace_retrieve {
        eprintln!("[retrieve_content_with_filter_fast] falling back to retrieve_graph");
    }

    let result = match atomic_core::pristine::InodeScopedGraph::new(txn, inode) {
        // The byte-graph walk must resolve edge destinations through the
        // file-local INODE_GRAPH index; on a multi-GB global GRAPH the
        // unscoped walk is orders of magnitude slower. `InodeScopedGraph`
        // falls back to the global lookup per call when the inode index is
        // not populated, so the result is unchanged.
        Ok(scoped) => atomic_core::record::workflow::retrieve::
            retrieve_content_with_filter_and_fork_info(&scoped, changes, position, options),
        Err(_) => atomic_core::record::workflow::retrieve::
            retrieve_content_with_filter_and_fork_info(txn, changes, position, options),
    };
    if debug_retrieve && retrieve_start.elapsed().as_millis() > 200 {
        eprintln!(
            "RETRIEVE fallback ms={} bytes={:?} inode={}",
            retrieve_start.elapsed().as_millis(),
            result.as_ref().ok().map(|(b, _)| b.len()),
            inode.get()
        );
    }
    result
}

/// Outcome of choosing the next vertex in a linear (single-path) content walk.
enum LinearStep {
    /// Exactly one live successor — continue the walk.
    Follow(atomic_core::types::GraphNode<NodeId>),
    /// No successor — the chain ends here.
    End,
    /// The structure cannot be linearised (a conflict fork with more than one
    /// live successor, or a delete-through where the live continuation is only
    /// reachable past a deleted vertex). Callers fall back to `retrieve_graph`.
    Bail,
}

/// Choose the single live successor of `current` for a linear content walk.
///
/// In the additive graph model a superseded span keeps its original alive
/// `BLOCK` edge *alongside* a new `DELETED` edge. A walk that merely skipped
/// `DELETED` edges would therefore still follow the alive edge into the
/// superseded span and resurrect stale content. This helper instead treats any
/// destination that has a visible `DELETED` edge as dead, so an in-place
/// replacement (delete old span + insert new span off the same predecessor)
/// resolves to just the new span. Genuine conflict forks and delete-throughs
/// return [`LinearStep::Bail`] so the caller defers to the full graph walk.
fn select_linear_successor<T>(
    txn: &T,
    inode: Inode,
    adj: &mut atomic_core::pristine::InodeAdjState,
    options: &atomic_core::output::alive::RetrieveOptions,
) -> atomic_core::record::RecordResult<LinearStep>
where
    T: atomic_core::pristine::GraphTxnT
        + atomic_core::pristine::InodeGraphOps<InodeError = atomic_core::pristine::PristineError>,
{
    use atomic_core::types::EdgeFlags;

    let mut alive: Vec<atomic_core::types::GraphNode<NodeId>> = Vec::new();
    let mut deleted: std::collections::HashSet<atomic_core::types::GraphNode<NodeId>> =
        std::collections::HashSet::new();

    while let Some(edge_result) = txn.next_inode_adj(adj) {
        let edge = edge_result?;
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
        let edge_dest = edge.dest();
        let dest = txn.find_block_in_inode(inode, edge_dest)?.ok_or_else(|| {
            atomic_core::pristine::PristineError::BlockNotFound {
                change: edge_dest.change.get(),
                pos: edge_dest.pos.get(),
            }
        })?;
        if !options.passes_filter(dest.change) {
            continue;
        }
        if flags.contains(EdgeFlags::DELETED) {
            deleted.insert(dest);
        } else if !alive.contains(&dest) {
            alive.push(dest);
        }
    }

    // A destination that is both alive (original edge) and deleted (superseding
    // edge) is dead from this view's perspective.
    alive.retain(|d| !deleted.contains(d));

    match alive.len() {
        0 => {
            if deleted.is_empty() {
                Ok(LinearStep::End)
            } else {
                // The only successors here were deleted; the live continuation
                // lies past a dead vertex — defer to the full graph walk.
                Ok(LinearStep::Bail)
            }
        }
        1 => Ok(LinearStep::Follow(alive[0])),
        _ => Ok(LinearStep::Bail),
    }
}

fn try_retrieve_linear_content_with_filter<T, C>(
    txn: &T,
    changes: &C,
    inode: Inode,
    position: Position<NodeId>,
    options: &atomic_core::output::alive::RetrieveOptions,
) -> atomic_core::record::RecordResult<Option<Vec<u8>>>
where
    T: atomic_core::pristine::GraphTxnT
        + atomic_core::pristine::InodeGraphOps<InodeError = atomic_core::pristine::PristineError>
        + atomic_core::pristine::TreeTxnT,
    C: atomic_core::change::ChangeStore,
{
    #[allow(unused_imports)]
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

        let mut adj = txn.init_inode_adj(inode, current, EdgeFlags::BLOCK, EdgeFlags::all())?;

        let dest = match select_linear_successor(txn, inode, &mut adj, options)? {
            LinearStep::Follow(d) => d,
            LinearStep::End => break,
            // A conflict fork or delete-through that the linear walk cannot
            // represent: defer to the full graph retrieval.
            LinearStep::Bail => return Ok(None),
        };

        let is_inode_marker = dest.start == dest.end && dest.start == position.pos;
        if !is_inode_marker && !dest.change.is_root() && dest.start != dest.end {
            vertices.push(dest);
        }

        current = dest;
    }

    let mut content = Vec::new();
    for node in vertices {
        let hash = txn.get_external(node.change)?.ok_or_else(|| {
            atomic_core::pristine::PristineError::ChangeNotFound {
                id: node.change.get(),
            }
        })?;

        // Performance (RFC §21 measured budgets, CB-13C AC-3): copy ONLY
        // this vertex's span instead of loading the whole change. The
        // former per-vertex `get_change` cache hit CLONES the entire change
        // (hunks, FileOps, provenance, contents) for every vertex — for a
        // 5000-file imported change that is a ~130KB clone per retrieval
        // call, ~18s across a status pass. `get_contents` peeks the cache
        // (no clone) and copies the requested byte range.
        let start = node.start.get() as usize;
        let end = node.end.get() as usize;
        if start > end {
            return Err(atomic_core::pristine::PristineError::InvalidVertex {
                message: format!(
                    "content span {}..{} for {} is inverted in change {}",
                    start,
                    end,
                    node,
                    hash
                ),
            }
            .into());
        }
        let mut span = vec![0u8; end - start];
        let copied = changes
            .get_contents(
                |id| txn.get_external(id).ok().flatten(),
                node,
                &mut span,
            )
            .map_err(|error| {
                atomic_core::record::RecordError::Io(std::io::Error::other(format!(
                    "content span {}..{} for {} exceeds change {} content length: {}",
                    start,
                    end,
                    node,
                    hash,
                    error
                )))
            })?;
        if copied != end - start {
            return Err(atomic_core::pristine::PristineError::InvalidVertex {
                message: format!(
                    "content span {}..{} for {} in change {} copied {} bytes (expected {})",
                    start,
                    end,
                    node,
                    hash,
                    copied,
                    end - start
                ),
            }
            .into());
        }
        content.extend_from_slice(&span);
    }

    Ok(Some(content))
}

use super::*;

impl Repository {
    // Change Storage Methods

    /// Get a reference to the change store.
    ///
    /// This provides direct access to the underlying [`ChangeStore`] for
    /// advanced operations like iteration or cache management.
    #[inline]
    pub fn change_store(&self) -> &ChangeStore {
        &self.change_store
    }

    // Ignore Rules

    /// Load ignore rules for this repository.
    ///
    /// This loads patterns from:
    /// - Global config: `~/.config/atomic/ignore`
    /// - Repository-local: `.atomicignore` in repository root
    ///
    /// The returned [`IgnoreRules`] can be used to check if paths should be
    /// ignored during tracking or status operations.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let repo = Repository::open(".")?;
    /// let rules = repo.ignore_rules();
    ///
    /// if rules.is_ignored(Path::new("target/debug"), true) {
    ///     println!("Path is ignored");
    /// }
    /// ```
    pub fn ignore_rules(&self) -> IgnoreRules {
        IgnoreRules::load(&self.root)
    }

    /// Check if a path should be ignored.
    ///
    /// This is a convenience method that loads ignore rules and checks the path.
    /// If you need to check multiple paths, use [`Self::ignore_rules()`] instead
    /// to avoid reloading the rules for each check.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to check (relative to repository root)
    /// * `is_dir` - Whether the path is a directory
    ///
    /// # Returns
    ///
    /// `true` if the path should be ignored, `false` otherwise.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let rules = self.ignore_rules();
        rules.is_ignored(path, is_dir)
    }

    /// Save a change to the repository.
    ///
    /// The change is serialized and written to the `.atomic/changes/` directory
    /// using a content-addressed two-level directory structure. The change is
    /// also cached for efficient subsequent access.
    ///
    /// # Arguments
    ///
    /// * `change` - The change to save
    ///
    /// # Returns
    ///
    /// The hash of the saved change, which can be used to retrieve it later.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The directory cannot be created
    /// - The file cannot be written
    /// - Serialization fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let change = record_changes(&repo, files)?;
    /// let hash = repo.save_change(&change)?;
    /// println!("Saved change: {}", hash.to_base32());
    /// ```
    pub fn save_change(&self, change: &Change) -> Result<Hash, RepositoryError> {
        self.change_store
            .save_change(change)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Save a change using pre-serialized V3 bytes (hash-stable).
    ///
    /// This writes the exact V3 bytes to disk, ensuring the file hash matches
    /// the hash registered in the pristine graph. Without this, re-serializing
    /// the deserialized Change can produce a different hash (different hash table
    /// ordering, different chunk boundaries, etc.), causing "change not found"
    /// errors on push.
    ///
    /// # Arguments
    ///
    /// * `hash` - The content hash (from the original serialization)
    /// * `v3_bytes` - The exact V3 bytes to write to disk
    /// * `_change` - The deserialized Change (unused, kept for API compatibility)
    pub(crate) fn save_change_bytes(
        &self,
        hash: &Hash,
        v3_bytes: &[u8],
        _change: &Change,
    ) -> Result<Hash, RepositoryError> {
        // Write the exact V3 bytes to the file store (no re-serialization).
        // This ensures the hash in the filename matches the hash in the pristine.
        let change_path = self.change_store.change_path(hash);
        if let Some(parent) = change_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&change_path, v3_bytes)?;

        Ok(*hash)
    }

    /// Load a change from the repository.
    ///
    /// If the change is in the cache, it's returned directly. Otherwise,
    /// it's loaded from disk, verified, and cached.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to load
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The change doesn't exist (`ChangeNotFound`)
    /// - The file is corrupted (hash mismatch)
    /// - Deserialization fails
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let change = repo.load_change(&hash)?;
    /// println!("Message: {}", change.hashed.header.message);
    /// ```
    pub fn load_change(&self, hash: &Hash) -> Result<Change, RepositoryError> {
        self.change_store.load_change(hash).map_err(|e| match e {
            ChangeStoreError::NotFound { hash } => RepositoryError::ChangeNotFound { hash },
            other => RepositoryError::Database(other.to_string()),
        })
    }

    /// Check if a change exists in the repository.
    ///
    /// This checks both the cache and the filesystem. Note that this
    /// doesn't verify the integrity of the change file.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to check
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// if repo.has_change(&hash) {
    ///     let change = repo.load_change(&hash)?;
    ///     // ...
    /// }
    /// ```
    pub fn has_change(&self, hash: &Hash) -> bool {
        self.change_store.has_change(hash)
    }

    /// Return every normal change registered in the repository's common graph.
    ///
    /// This is the authoritative object inventory for sync negotiation: changes
    /// are repository-global graph nodes, not owned by a particular view. A
    /// client advertises these hashes as `haves` so the server never resends a
    /// `.change` merely because it is absent from one view's own metadata.
    pub fn registered_change_hashes(&self) -> Result<Vec<Hash>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.list_registered_changes()
            .map(|items| items.into_iter().map(|(_, hash)| hash).collect())
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Backfill the pristine normal-change dependency index from stored changes.
    ///
    /// This is intended for legacy repositories that predate the `CHANGE_DEPS`
    /// index. It is deliberately explicit: interactive commands such as
    /// `status` should not repair the index by scanning `.change` files.
    ///
    /// Returns `(indexed, skipped, failed)` counts.
    pub fn repair_change_dependency_index(
        &self,
        force: bool,
    ) -> Result<(usize, usize, usize), RepositoryError> {
        let registered = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.list_registered_changes()
                .map_err(|e| RepositoryError::Database(e.to_string()))?
        };

        let mut indexed = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;

        for (change_id, hash) in registered {
            if !force {
                let txn = self
                    .pristine
                    .read_txn()
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                if txn
                    .is_change_deps_indexed(change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?
                {
                    skipped += 1;
                    continue;
                }
            }

            let change = match self.load_change(&hash) {
                Ok(change) => change,
                Err(e) => {
                    log::warn!(
                        "failed to load change {} while repairing dependency index: {}",
                        hash.to_base32(),
                        e
                    );
                    failed += 1;
                    continue;
                }
            };

            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.put_change_deps(change_id, change.dependencies())
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            indexed += 1;
        }

        Ok((indexed, skipped, failed))
    }

    // Attestation Methods

    /// Save an attestation to the repository.
    ///
    /// Serializes the attestation to disk and registers it in the graph
    /// with `node_type::ATTESTATION`. Also registers dependencies from
    /// `changes_covered` in the DEPS table so the graph knows which
    /// changes this attestation covers.
    ///
    /// # Arguments
    ///
    /// * `attestation` - The attestation to save
    ///
    /// # Returns
    ///
    /// The content hash of the saved attestation.
    pub fn save_attestation(
        &self,
        attestation: &atomic_core::change::Attestation,
    ) -> Result<Hash, RepositoryError> {
        use atomic_core::pristine::MutTxnT;

        // Save to disk
        let hash = self
            .change_store
            .save_attestation(attestation)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Register in the graph
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let attest_id = txn
            .register_attestation(&hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Register dependencies: attestation → covered changes. A downloaded
        // change may not have been inserted into a view yet (`--download-only`),
        // but it still needs an internal ID so this relationship is durable.
        // Insertion reuses the same content-addressed registration later.
        for change_hash in &attestation.changes_covered {
            let change_id = txn
                .register_change(change_hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.put_dep(attest_id, change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // If chained, register dependency on previous attestation too
        if let Some(ref prev_hash) = attestation.previous_attestation {
            if let Ok(Some(prev_id)) = txn.get_internal(prev_hash) {
                txn.put_dep(attest_id, prev_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        Ok(hash)
    }

    /// Build and persist the latest content-addressed manifest for a session.
    ///
    /// `parent_session`/`fork_turn` record fork lineage when a session was
    /// created by forking another at a specific turn boundary.
    pub fn publish_session_manifest_with_fork(
        &self,
        session_id: &str,
        parent_session: Option<Hash>,
        fork_turn: Option<u32>,
    ) -> Result<Hash, RepositoryError> {
        use atomic_core::change::session::SessionManifest;

        let (_, turns) = self.get_session_ledger(session_id)?.ok_or_else(|| {
            RepositoryError::Database(format!("session not found: {}", session_id))
        })?;
        let goal_provenance = turns
            .iter()
            .find_map(|turn| turn.goal.as_ref().map(|_| turn.provenance_hash));
        let manifest = SessionManifest {
            schema_version: 2,
            session_id: session_id.to_string(),
            goal_provenance,
            turns,
            parent_session,
            fork_turn,
        };

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let hash = txn
            .save_session_manifest(&manifest)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(hash)
    }

    /// Build and persist the latest content-addressed manifest for a session,
    /// preserving fork lineage from the current head.
    pub fn publish_session_manifest(&self, session_id: &str) -> Result<Hash, RepositoryError> {
        let (parent_session, fork_turn) = match self.get_session_head(session_id)? {
            Some(head) => match self.get_session_manifest(&head)? {
                Some(manifest) => (manifest.parent_session, manifest.fork_turn),
                None => (None, None),
            },
            None => (None, None),
        };
        self.publish_session_manifest_with_fork(session_id, parent_session, fork_turn)
    }

    /// Load an immutable session manifest by content hash.
    pub fn get_session_manifest(
        &self,
        hash: &Hash,
    ) -> Result<Option<atomic_core::change::session::SessionManifest>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.get_session_manifest(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Resolve the latest manifest hash for an external session ID.
    pub fn get_session_head(&self, session_id: &str) -> Result<Option<Hash>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.get_session_head(session_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Store a session manifest received from another repository.
    ///
    /// Idempotent: storing the same content hash twice is a no-op. The
    /// convenience head advances to this manifest for its session ID.
    pub fn ingest_session_manifest(
        &self,
        manifest: &atomic_core::change::session::SessionManifest,
    ) -> Result<Hash, RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let hash = txn
            .save_session_manifest(manifest)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(hash)
    }

    /// Rebuild session indexes from stored provenance graphs.
    ///
    /// Idempotent: existing `(session, provenance)` pairs are skipped, so a
    /// rebuilt repository never gains duplicate turns. Corrupt provenance
    /// files are reported in the result rather than aborting the rebuild.
    /// Returns `(indexed_count, skipped_existing, corrupt_count)`.
    pub fn rebuild_session_index(&self) -> Result<(usize, usize, usize), RepositoryError> {
        let mut indexed = 0usize;
        let mut skipped = 0usize;
        let mut corrupt = 0usize;

        // Collect (hash, graph) pairs first so each write txn is short-lived.
        let mut graphs: Vec<(Hash, atomic_core::change::ProvenanceGraph)> = Vec::new();
        for result in self.change_store.iter_provenance_graphs() {
            match result {
                Ok(hash) => match self.load_provenance_graph(&hash) {
                    Ok(graph) => graphs.push((hash, graph)),
                    Err(_) => corrupt += 1,
                },
                Err(_) => corrupt += 1,
            }
        }

        // Group by session. The core index derives canonical turn order from
        // the complete set, independent of this ingestion order.
        let mut by_session: std::collections::BTreeMap<
            String,
            Vec<(Hash, atomic_core::change::ProvenanceGraph)>,
        > = std::collections::BTreeMap::new();
        let mut head_manifests = std::collections::BTreeMap::new();
        let mut fork_parents = std::collections::BTreeMap::new();
        for (hash, graph) in graphs {
            by_session
                .entry(graph.session_id.clone())
                .or_default()
                .push((hash, graph));
        }
        // Forked children can contain only inherited turns, whose provenance
        // files still name the parent session. Include both existing ledgers
        // and portable manifests so rebuild can create or migrate the child.
        {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            for record in txn
                .list_session_records()
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                if record.turn_count > 0 {
                    by_session.entry(record.session_id).or_default();
                }
            }
            for (head_session_id, head) in txn
                .list_session_heads()
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                match txn.get_session_manifest(&head) {
                    Ok(Some(manifest)) if manifest.session_id == head_session_id => {
                        if let (Some(parent_hash), Some(fork_turn)) =
                            (manifest.parent_session, manifest.fork_turn)
                        {
                            if let Ok(Some(parent)) = txn.get_session_manifest(&parent_hash) {
                                fork_parents.insert(
                                    head_session_id.clone(),
                                    (parent.session_id, parent_hash, fork_turn),
                                );
                            }
                        }
                        by_session.entry(head_session_id.clone()).or_default();
                        head_manifests.insert(head_session_id, manifest);
                    }
                    Ok(Some(_)) => {
                        log::warn!(
                            "Skipping session head {} because its manifest names another session",
                            head_session_id
                        );
                    }
                    Ok(None) => {
                        log::warn!(
                            "Session head {} points to a missing manifest",
                            head_session_id
                        );
                    }
                    Err(error) => {
                        log::warn!(
                            "Skipping unreadable session manifest for {}: {}",
                            head_session_id,
                            error
                        );
                    }
                }
            }
        }

        for (session_id, mut session_graphs) in by_session {
            session_graphs.sort_by(|(a_hash, a), (b_hash, b)| {
                a.timestamp
                    .cmp(&b.timestamp)
                    .then_with(|| a_hash.as_bytes().cmp(b_hash.as_bytes()))
            });

            // Read existing reverse entries once to detect already-indexed
            // provenance hashes.
            let (mut existing, had_record): (std::collections::HashSet<Hash>, bool) = {
                let txn = self
                    .pristine
                    .read_txn()
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                let record = txn
                    .get_session_record(&session_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                let mut set = std::collections::HashSet::new();
                if record.is_some() {
                    for turn in txn
                        .get_session_turns(&session_id)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?
                    {
                        set.insert(turn.provenance_hash);
                    }
                }
                (set, record.is_some())
            };

            let json_path = self
                .dot_dir
                .join("sessions")
                .join(format!("{}.json", session_id))
                .to_string_lossy()
                .to_string();

            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            for (hash, graph) in &session_graphs {
                if existing.contains(hash) {
                    skipped += 1;
                    continue;
                }
                txn.index_session_turn(&session_id, &json_path, hash, graph)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                existing.insert(*hash);
                indexed += 1;
            }
            if let Some(manifest) = head_manifests.get(&session_id) {
                for turn in &manifest.turns {
                    if existing.insert(turn.provenance_hash) {
                        txn.index_inherited_turn(&session_id, &json_path, turn)
                            .map_err(|e| RepositoryError::Database(e.to_string()))?;
                        indexed += 1;
                    }
                }
                if manifest.turns.is_empty() && !had_record {
                    txn.index_empty_session(&session_id, &json_path, None, None)
                        .map_err(|e| RepositoryError::Database(e.to_string()))?;
                }
            }
            // Also repairs turn numbers, causal edges, and legacy todo IDs
            // when every provenance graph was already indexed.
            txn.normalize_session_turn_order(&session_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            if let Some((parent_session_id, parent_manifest, fork_turn)) =
                fork_parents.get(&session_id)
            {
                txn.emit_session_fork_kg(
                    &session_id,
                    parent_session_id,
                    *fork_turn,
                    parent_manifest,
                )
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            if let Err(e) = self.publish_session_manifest(&session_id) {
                log::warn!("Manifest publication failed for {}: {}", session_id, e);
            }
        }

        Ok((indexed, skipped, corrupt))
    }

    /// Reconcile lifecycle metadata for an agent session.
    ///
    /// Called from session start/end hooks. `ended_at: None` marks the
    /// session active (clearing a stale end marker on resume); `Some(ts)`
    /// marks it ended. Best-effort callers treat failures as recoverable
    /// divergence — the JSON session file remains the runtime fallback and
    /// `rebuild_session_index` can reconcile later.
    pub fn upsert_session_lifecycle(
        &self,
        session_id: &str,
        view_name: Option<String>,
        parent_view: Option<String>,
        ended_at: Option<i64>,
    ) -> Result<(), RepositoryError> {
        let json_path = self
            .dot_dir
            .join("sessions")
            .join(format!("{}.json", session_id))
            .to_string_lossy()
            .to_string();
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.upsert_session_lifecycle(session_id, &json_path, view_name, parent_view, ended_at)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    /// Create a forked session from a parent at a turn boundary.
    ///
    /// The child inherits the parent's turn records through `fork_turn` as an
    /// immutable ledger prefix. No provenance graphs or session rows are
    /// copied — the child manifest references the parent by content hash.
    ///
    /// Returns `(parent_manifest_hash, child_manifest_hash)`.
    pub fn fork_session(
        &self,
        parent_session_id: &str,
        fork_turn: u32,
        child_session_id: &str,
    ) -> Result<(Hash, Hash), RepositoryError> {
        // Resolve the parent ledger and manifest.
        let (parent_record, parent_turns) =
            self.get_session_ledger(parent_session_id)?.ok_or_else(|| {
                RepositoryError::Database(format!(
                    "parent session not found: {}",
                    parent_session_id
                ))
            })?;

        let last_parent_turn = parent_record.turn_count.saturating_sub(1);
        if fork_turn > last_parent_turn && parent_record.turn_count > 0 {
            return Err(RepositoryError::Database(format!(
                "fork turn {} exceeds parent turn {} for session {}",
                fork_turn, last_parent_turn, parent_session_id
            )));
        }

        let parent_manifest = self
            .publish_session_manifest(parent_session_id)
            .map_err(|e| RepositoryError::Database(format!("parent manifest: {}", e)))?;

        // Seed the child session record with the inherited ledger prefix:
        // turns 0..=fork_turn from the parent. The child reuses the parent's
        // immutable turn rows — we copy the references, not the provenance.
        let inherited: Vec<atomic_core::change::session::SessionTurn> = parent_turns
            .into_iter()
            .filter(|t| t.turn_number <= fork_turn)
            .map(|mut t| {
                t.session_id = child_session_id.to_string();
                t
            })
            .collect();

        let child_json_path = self
            .dot_dir
            .join("sessions")
            .join(format!("{}.json", child_session_id))
            .to_string_lossy()
            .to_string();

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        for turn in &inherited {
            txn.index_inherited_turn(child_session_id, &child_json_path, turn)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }
        // Create the child record even when no turns are inherited.
        if inherited.is_empty() {
            txn.index_empty_session(
                child_session_id,
                &child_json_path,
                parent_record.view_name.clone(),
                parent_record.parent_view.clone(),
            )
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }
        // Taxonomy: child session prov:wasDerivedFrom parent session (ATOM-16),
        // in the same transaction as the inherited ledger prefix.
        txn.emit_session_fork_kg(
            child_session_id,
            parent_session_id,
            fork_turn,
            &parent_manifest,
        )
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let child_manifest = self
            .publish_session_manifest_with_fork(
                child_session_id,
                Some(parent_manifest),
                Some(fork_turn),
            )
            .map_err(|e| RepositoryError::Database(format!("child manifest: {}", e)))?;

        Ok((parent_manifest, child_manifest))
    }

    /// Load an attestation from the repository by hash.
    pub fn load_attestation(
        &self,
        hash: &Hash,
    ) -> Result<atomic_core::change::Attestation, RepositoryError> {
        self.change_store
            .load_attestation(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Check if an attestation exists in the repository.
    pub fn has_attestation(&self, hash: &Hash) -> bool {
        self.change_store.has_attestation(hash)
    }

    /// Find all attestations that cover a specific change.
    ///
    /// Uses REV_DEPS to find nodes that depend on the given change,
    /// then filters by `node_type::ATTESTATION`.
    ///
    /// # Arguments
    ///
    /// * `change_hash` - The hash of the change to find attestations for
    ///
    /// # Returns
    ///
    /// A vector of `(Hash, Attestation)` pairs covering this change.
    pub fn find_attestations_for_change(
        &self,
        change_hash: &Hash,
    ) -> Result<Vec<(Hash, atomic_core::change::Attestation)>, RepositoryError> {
        use atomic_core::pristine::{node_type, GraphTxnT};

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the internal ID for this change
        let change_id = match txn
            .get_internal(change_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };

        // Look up REV_DEPS: who depends on this change?
        let rev_deps = txn
            .get_rev_deps(change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut attestations = Vec::new();

        for dep_id in rev_deps {
            // Check if this dependent is an attestation
            let node_type_val = txn
                .get_node_type(dep_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            if node_type_val != Some(node_type::ATTESTATION) {
                continue;
            }

            // Get the external hash
            let dep_hash = match txn
                .get_external(dep_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                Some(h) => h,
                None => continue,
            };

            // Load the attestation from disk
            match self.load_attestation(&dep_hash) {
                Ok(attest) => attestations.push((dep_hash, attest)),
                Err(_) => continue, // File missing or corrupt — skip
            }
        }

        Ok(attestations)
    }

    /// Find all attestations relevant to a view.
    ///
    /// Iterates over all changes in the view, checks REV_DEPS for each,
    /// and collects unique attestations. Returns them with coverage info
    /// showing which changes each attestation covers within this view.
    ///
    /// # Arguments
    ///
    /// * `view_name` - The name of the view to query
    ///
    /// # Returns
    ///
    /// A vector of `(Hash, Attestation, Vec<Hash>)` where the third element
    /// is the subset of `changes_covered` that are in this view.
    pub fn find_attestations_for_view(
        &self,
        view_name: &str,
    ) -> Result<Vec<(Hash, atomic_core::change::Attestation, Vec<Hash>)>, RepositoryError> {
        use atomic_core::pristine::{node_type, GraphTxnT, ViewTxnT};
        use std::collections::{HashMap, HashSet};

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the view
        let view = match txn
            .get_view(view_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(s) => s,
            None => return Ok(Vec::new()),
        };

        // Collect all change IDs and hashes in this view
        let mut view_change_ids: HashSet<u64> = HashSet::new();
        let mut view_change_hashes: HashSet<Hash> = HashSet::new();

        let iter = txn
            .iter_changes(&view, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in iter {
            let (_seq, change_id, _merkle) =
                result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            view_change_ids.insert(change_id.get());

            if let Ok(Some(hash)) = txn.get_external(change_id) {
                view_change_hashes.insert(hash);
            }
        }

        // For each change in the view, find attestation nodes via REV_DEPS
        let mut seen_attestations: HashMap<Hash, (atomic_core::change::Attestation, Vec<Hash>)> =
            HashMap::new();

        for change_id_raw in &view_change_ids {
            let change_id = NodeId::new(*change_id_raw);

            let rev_deps = match txn.get_rev_deps(change_id) {
                Ok(ids) => ids,
                Err(_) => continue,
            };

            for dep_id in rev_deps {
                // Check node type
                let node_type_val = match txn.get_node_type(dep_id) {
                    Ok(Some(t)) => t,
                    _ => continue,
                };

                if node_type_val != node_type::ATTESTATION {
                    continue;
                }

                // Get external hash
                let attest_hash = match txn.get_external(dep_id) {
                    Ok(Some(h)) => h,
                    _ => continue,
                };

                // Skip if we've already processed this attestation
                if seen_attestations.contains_key(&attest_hash) {
                    // Add the current change to coverage if covered
                    if let Ok(Some(change_hash)) = txn.get_external(change_id) {
                        if let Some((attest, covered)) = seen_attestations.get_mut(&attest_hash) {
                            if attest.covers_change(&change_hash) && !covered.contains(&change_hash)
                            {
                                covered.push(change_hash);
                            }
                        }
                    }
                    continue;
                }

                // Load attestation
                let attest = match self.load_attestation(&attest_hash) {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                // Compute which of this attestation's covered changes are in this view
                let covered_in_view: Vec<Hash> = attest
                    .changes_covered
                    .iter()
                    .filter(|h| view_change_hashes.contains(h))
                    .cloned()
                    .collect();

                seen_attestations.insert(attest_hash, (attest, covered_in_view));
            }
        }

        // Convert to output format
        let results: Vec<_> = seen_attestations
            .into_iter()
            .map(|(hash, (attest, covered))| (hash, attest, covered))
            .collect();

        Ok(results)
    }

    /// Delete a change from the repository.
    ///
    /// This removes the change file from disk and from the cache.
    /// Note that this does NOT remove the change from any stacks - use
    /// `unrecord` for that.
    ///
    /// # Arguments
    ///
    /// * `hash` - The hash of the change to delete
    ///
    /// # Returns
    ///
    /// `true` if the change was deleted, `false` if it didn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be deleted.
    ///
    /// # Warning
    ///
    /// Deleting a change that is still referenced by a view will cause
    /// errors when trying to access that view. Use with caution.
    pub fn delete_change(&self, hash: &Hash) -> Result<bool, RepositoryError> {
        self.change_store
            .delete_change(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Count the number of changes stored in the repository.
    ///
    /// This scans the entire changes directory.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let count = repo.count_changes()?;
    /// println!("Repository has {} changes", count);
    /// ```
    pub fn count_changes(&self) -> Result<usize, RepositoryError> {
        self.change_store
            .count_changes()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Iterate over all change hashes stored in the repository.
    ///
    /// This scans the changes directory and yields the hash of each
    /// change file found. The iteration order is not guaranteed.
    ///
    /// # Performance
    ///
    /// This method reads the filesystem and should be used sparingly
    /// on repositories with many changes.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// for result in repo.iter_changes() {
    ///     match result {
    ///         Ok(hash) => println!("Found change: {}", hash.to_base32()),
    ///         Err(e) => eprintln!("Error: {}", e),
    ///     }
    /// }
    /// ```
    pub fn iter_changes(&self) -> impl Iterator<Item = Result<Hash, RepositoryError>> + '_ {
        self.change_store
            .iter_changes()
            .map(|r| r.map_err(|e| RepositoryError::Database(e.to_string())))
    }

    /// Find a change by hash prefix.
    ///
    /// This searches through all stored changes to find one whose hash
    /// starts with the given prefix. Useful for CLI commands that allow
    /// abbreviated hashes.
    ///
    /// # Arguments
    ///
    /// * `prefix` - The hash prefix (case-insensitive, at least 2 characters)
    ///
    /// # Returns
    ///
    /// * `Ok(Some(hash))` - Found a unique matching change
    /// * `Ok(None)` - No change matched the prefix
    /// * `Err(_)` - Multiple changes matched (ambiguous) or I/O error
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Find a change by abbreviated hash
    /// if let Some(hash) = repo.find_change_by_prefix("ABCD")? {
    ///     let change = repo.load_change(&hash)?;
    ///     println!("Found: {}", hash.to_base32());
    /// }
    /// ```
    pub fn find_change_by_prefix(&self, prefix: &str) -> Result<Option<Hash>, RepositoryError> {
        let prefix_upper = prefix.to_uppercase();
        let mut matches = Vec::new();

        for result in self.iter_changes() {
            let hash = result?;
            let hash_str = hash.to_base32();
            if hash_str.starts_with(&prefix_upper) {
                matches.push(hash);
                // If we find more than one, it's ambiguous
                if matches.len() > 1 {
                    return Err(RepositoryError::AmbiguousHash {
                        prefix: prefix.to_string(),
                        matches: matches.iter().map(|h| h.to_base32()).collect(),
                    });
                }
            }
        }

        Ok(matches.into_iter().next())
    }

    // =========================================================================
    // Provenance Graph Operations
    // =========================================================================

    /// Save a provenance graph to the repository.
    ///
    /// Serializes the graph to disk, registers it in the pristine database
    /// with `node_type::PROVENANCE`, and records dependencies on the
    /// changes this graph explains.
    ///
    /// # Returns
    ///
    /// The content hash of the saved provenance graph.
    pub fn save_provenance_graph(
        &self,
        graph: &atomic_core::change::ProvenanceGraph,
    ) -> Result<Hash, RepositoryError> {
        use atomic_core::pristine::MutTxnT;

        // Save to disk
        let hash = self
            .change_store
            .save_provenance_graph(graph)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Register in the graph
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let prov_id = txn
            .register_provenance(&hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Register dependencies: provenance → explained changes. Sidecars can
        // arrive with `--download-only`, before their changes are inserted into
        // a view. Registering the content hash now gives the relationship a
        // stable internal ID; insertion reuses that registration later.
        for change_hash in &graph.changes_explained {
            let change_id = txn
                .register_change(change_hash)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.put_dep(prov_id, change_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // If chained, register dependency on previous provenance graph too
        if let Some(ref prev_hash) = graph.previous {
            if let Ok(Some(prev_id)) = txn.get_internal(prev_hash) {
                txn.put_dep(prov_id, prev_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }

        // Populate session tables from the provenance graph, for any agent
        // (Sherpa, Claude Code, OpenCode, generic atomic-agent, ...). The
        // write transaction is still open, so this is atomic with the
        // provenance registration above.
        txn.populate_session_tables(prov_id.get(), graph)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Maintain the Atomic-native session ledger alongside the provenance
        // registration. The JSON file is a mutable runtime cache; this index
        // connects the external session identity to immutable turn objects.
        let json_path = self
            .dot_dir
            .join("sessions")
            .join(format!("{}.json", graph.session_id))
            .to_string_lossy()
            .to_string();
        txn.index_session_turn(&graph.session_id, &json_path, &hash, graph)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Publish the immutable session manifest after the provenance/index
        // transaction commits. A failed publication never invalidates the
        // already-committed provenance graph; it can be rebuilt later.
        if let Err(e) = self.publish_session_manifest(&graph.session_id) {
            log::warn!(
                "Session {} indexed but manifest publication failed: {}",
                graph.session_id,
                e
            );
        }

        Ok(hash)
    }

    /// Load a provenance graph from the repository by hash.
    pub fn load_provenance_graph(
        &self,
        hash: &Hash,
    ) -> Result<atomic_core::change::ProvenanceGraph, RepositoryError> {
        self.change_store
            .load_provenance_graph(hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Check if a provenance graph exists in the repository.
    pub fn has_provenance_graph(&self, hash: &Hash) -> bool {
        self.change_store.has_provenance_graph(hash)
    }

    /// Find all provenance graphs that explain a specific change.
    ///
    /// Uses REV_DEPS to find nodes that depend on the given change,
    /// then filters by `node_type::PROVENANCE`.
    ///
    /// # Arguments
    ///
    /// * `change_hash` - The hash of the change to find provenance for
    ///
    /// # Returns
    ///
    /// A vector of `(Hash, ProvenanceGraph)` pairs explaining this change.
    pub fn find_provenance_for_change(
        &self,
        change_hash: &Hash,
    ) -> Result<Vec<(Hash, atomic_core::change::ProvenanceGraph)>, RepositoryError> {
        use atomic_core::pristine::{node_type, GraphTxnT};

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the internal ID for this change
        let change_id = match txn
            .get_internal(change_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };

        // Look up REV_DEPS: who depends on this change?
        let rev_deps = txn
            .get_rev_deps(change_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut graphs = Vec::new();

        for dep_id in rev_deps {
            // Check if this dependent is a provenance graph
            let node_type_val = txn
                .get_node_type(dep_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;

            if node_type_val != Some(node_type::PROVENANCE) {
                continue;
            }

            // Get the external hash
            let dep_hash = match txn
                .get_external(dep_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?
            {
                Some(h) => h,
                None => continue,
            };

            // Load the provenance graph from disk
            match self.load_provenance_graph(&dep_hash) {
                Ok(graph) => graphs.push((dep_hash, graph)),
                Err(_) => continue, // File missing or corrupt — skip
            }
        }

        Ok(graphs)
    }

    /// Find all provenance graphs that explain a change by scanning disk.
    ///
    /// A read-only fallback for [`Self::find_provenance_for_change`]: REV_DEPS
    /// registration is best-effort (`save_provenance_graph` records the reverse
    /// dependency only when the explained change is already internal), so a graph
    /// whose change was not yet internal is invisible to REV_DEPS. This iterates
    /// every provenance graph file and keeps those whose `changes_explained`
    /// contains `change_hash`. It writes nothing — pure compute-on-demand.
    ///
    /// # Returns
    ///
    /// A vector of `(Hash, ProvenanceGraph)` pairs explaining this change,
    /// ordered by timestamp (oldest first).
    pub fn find_provenance_for_change_scan(
        &self,
        change_hash: &Hash,
    ) -> Result<Vec<(Hash, atomic_core::change::ProvenanceGraph)>, RepositoryError> {
        let mut graphs = Vec::new();

        for result in self.change_store.iter_provenance_graphs() {
            let hash = result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            match self.load_provenance_graph(&hash) {
                Ok(graph) if graph.explains_change(change_hash) => {
                    graphs.push((hash, graph));
                }
                _ => continue,
            }
        }

        // Sort by timestamp (oldest first) for stable ordering.
        graphs.sort_by_key(|(_, g)| g.timestamp);

        Ok(graphs)
    }

    /// Find all provenance graphs for a session by scanning disk.
    ///
    /// Iterates over all provenance graph files and filters by session ID.
    /// This is a full scan — use `find_provenance_for_change` when you have
    /// a specific change hash.
    ///
    /// # Arguments
    ///
    /// * `session_id` - The session identifier to match
    ///
    /// # Returns
    ///
    /// A vector of `(Hash, ProvenanceGraph)` pairs for this session,
    /// ordered by timestamp (oldest first).
    pub fn find_provenance_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<(Hash, atomic_core::change::ProvenanceGraph)>, RepositoryError> {
        let mut graphs = Vec::new();

        for result in self.change_store.iter_provenance_graphs() {
            let hash = result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            match self.load_provenance_graph(&hash) {
                Ok(graph) if graph.session_id == session_id => {
                    graphs.push((hash, graph));
                }
                _ => continue,
            }
        }

        // Sort by timestamp (oldest first) for chain reconstruction
        graphs.sort_by_key(|(_, g)| g.timestamp);

        Ok(graphs)
    }

    /// Read the Atomic-native indexed ledger for a session.
    ///
    /// Unlike `find_provenance_for_session`, this reads only the session index
    /// and turn records; it does not scan unrelated provenance files.
    pub fn get_session_ledger(
        &self,
        session_id: &str,
    ) -> Result<
        Option<(
            atomic_core::change::session::SessionRecord,
            Vec<atomic_core::change::session::SessionTurn>,
        )>,
        RepositoryError,
    > {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let record = txn
            .get_session_record(session_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        match record {
            Some(record) => {
                let turns = txn
                    .get_session_turns(session_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
                Ok(Some((record, turns)))
            }
            None => Ok(None),
        }
    }

    /// List recent session ledgers, newest activity first.
    ///
    /// Recency is the latest turn timestamp, falling back to `ended_at` and
    /// then `started_at`. Session ID breaks timestamp ties deterministically.
    pub fn list_session_ledgers(
        &self,
        limit: usize,
    ) -> Result<
        Vec<(
            atomic_core::change::session::SessionRecord,
            Vec<atomic_core::change::session::SessionTurn>,
        )>,
        RepositoryError,
    > {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let records = txn
            .list_session_records()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let mut ledgers = Vec::with_capacity(records.len());
        for record in records {
            let turns = txn
                .get_session_turns(&record.session_id)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            ledgers.push((record, turns));
        }
        ledgers.sort_by(|(a_record, a_turns), (b_record, b_turns)| {
            let activity =
                |record: &atomic_core::change::session::SessionRecord,
                 turns: &[atomic_core::change::session::SessionTurn]| {
                    turns
                        .last()
                        .map(|turn| turn.timestamp)
                        .or(record.ended_at)
                        .unwrap_or(record.started_at)
                };
            activity(b_record, b_turns)
                .cmp(&activity(a_record, a_turns))
                .then_with(|| a_record.session_id.cmp(&b_record.session_id))
        });
        ledgers.truncate(limit);
        Ok(ledgers)
    }

    // =========================================================================
    // Session Data Queries (Sherpa-enriched provenance)
    // =========================================================================

    /// Get the full ordered replay log for a provenance graph.
    ///
    /// Returns all session events ordered by sequence number, for provenance
    /// from any agent. Empty only if the graph has no nodes / no session data.
    pub fn get_session_events(
        &self,
        provenance_hash: &Hash,
    ) -> Result<Vec<atomic_core::change::session::SessionEvent>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let provenance_id = txn
            .get_internal(provenance_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: provenance_hash.to_base32(),
            })?;
        txn.get_session_events(provenance_id.get())
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Get all todos for a provenance graph.
    ///
    /// Returns snapshots of all todo items from the turn, for any agent.
    /// Empty if the provenance recorded no todos / no session data.
    pub fn get_session_todos(
        &self,
        provenance_hash: &Hash,
    ) -> Result<Vec<atomic_core::change::session::TodoSnapshot>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let provenance_id = txn
            .get_internal(provenance_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: provenance_hash.to_base32(),
            })?;
        txn.get_session_todos(provenance_id.get())
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Get phase timing breakdown for a provenance graph.
    ///
    /// Returns timing data for each phase in the turn. Populated from the
    /// per-phase token breakdown Sherpa graphs carry; empty for agents that
    /// do not emit phase timing.
    pub fn get_session_phases(
        &self,
        provenance_hash: &Hash,
    ) -> Result<Vec<atomic_core::change::session::PhaseTimingEntry>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let provenance_id = txn
            .get_internal(provenance_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: provenance_hash.to_base32(),
            })?;
        txn.get_session_phases(provenance_id.get())
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Get intent metadata for a provenance graph.
    ///
    /// Returns the intent entry when the graph carries a Goal node with intent
    /// `detail`, `None` otherwise.
    pub fn get_session_intent(
        &self,
        provenance_hash: &Hash,
    ) -> Result<Option<atomic_core::change::session::IntentEntry>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let provenance_id = txn
            .get_internal(provenance_hash)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
            .ok_or_else(|| RepositoryError::ChangeNotFound {
                hash: provenance_hash.to_base32(),
            })?;
        txn.get_session_intent(provenance_id.get())
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Check if a provenance graph has populated session data.
    ///
    /// Returns `true` when the graph has at least one indexed session event
    /// (every provenance node produces one, for any agent). This is a fast
    /// gate for the UI. Note it is intentionally broader than
    /// [`Self::get_session_intent`]: a generic agent graph whose Goal node
    /// carries no intent `detail` still has session data (events, todos).
    pub fn has_session_data(&self, provenance_hash: &Hash) -> bool {
        let txn = match self.pristine.read_txn() {
            Ok(t) => t,
            Err(_) => return false,
        };

        let provenance_id = match txn.get_internal(provenance_hash) {
            Ok(Some(id)) => id.get(),
            _ => return false,
        };

        txn.get_session_events(provenance_id)
            .map(|events| !events.is_empty())
            .unwrap_or(false)
    }
}

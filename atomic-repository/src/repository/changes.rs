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

        // Register dependencies: attestation → covered changes
        for change_hash in &attestation.changes_covered {
            if let Ok(Some(change_id)) = txn.get_internal(change_hash) {
                txn.put_dep(attest_id, change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
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

    /// Find all attestations relevant to a stack.
    ///
    /// Iterates over all changes in the stack, checks REV_DEPS for each,
    /// and collects unique attestations. Returns them with coverage info
    /// showing which changes each attestation covers within this stack.
    ///
    /// # Arguments
    ///
    /// * `stack_name` - The name of the stack to query
    ///
    /// # Returns
    ///
    /// A vector of `(Hash, Attestation, Vec<Hash>)` where the third element
    /// is the subset of `changes_covered` that are in this stack.
    pub fn find_attestations_for_stack(
        &self,
        stack_name: &str,
    ) -> Result<Vec<(Hash, atomic_core::change::Attestation, Vec<Hash>)>, RepositoryError> {
        use atomic_core::pristine::{node_type, GraphTxnT, StackTxnT};
        use std::collections::{HashMap, HashSet};

        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Get the stack
        let stack = match txn
            .get_stack(stack_name)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        {
            Some(s) => s,
            None => return Ok(Vec::new()),
        };

        // Collect all change IDs and hashes in this stack
        let mut stack_change_ids: HashSet<u64> = HashSet::new();
        let mut stack_change_hashes: HashSet<Hash> = HashSet::new();

        let iter = txn
            .iter_changes(&stack, 0)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        for result in iter {
            let (_seq, change_id, _merkle) =
                result.map_err(|e| RepositoryError::Database(e.to_string()))?;

            stack_change_ids.insert(change_id.get());

            if let Ok(Some(hash)) = txn.get_external(change_id) {
                stack_change_hashes.insert(hash);
            }
        }

        // For each change in the stack, find attestation nodes via REV_DEPS
        let mut seen_attestations: HashMap<Hash, (atomic_core::change::Attestation, Vec<Hash>)> =
            HashMap::new();

        for change_id_raw in &stack_change_ids {
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

                // Compute which of this attestation's covered changes are in this stack
                let covered_in_stack: Vec<Hash> = attest
                    .changes_covered
                    .iter()
                    .filter(|h| stack_change_hashes.contains(h))
                    .cloned()
                    .collect();

                seen_attestations.insert(attest_hash, (attest, covered_in_stack));
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
    /// Deleting a change that is still referenced by a stack will cause
    /// errors when trying to access that stack. Use with caution.
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

        // Register dependencies: provenance → explained changes
        for change_hash in &graph.changes_explained {
            if let Ok(Some(change_id)) = txn.get_internal(change_hash) {
                txn.put_dep(prov_id, change_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }

        // If chained, register dependency on previous provenance graph too
        if let Some(ref prev_hash) = graph.previous {
            if let Ok(Some(prev_id)) = txn.get_internal(prev_hash) {
                txn.put_dep(prov_id, prev_id)
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

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
}

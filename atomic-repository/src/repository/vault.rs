use super::*;

use atomic_core::pristine::vault::{
    MemorySummary, SkillSummary, VaultEntry, VaultEntryType, VaultManifest,
};
use atomic_core::types::{Hash, Merkle};

/// Directory name for the vault at the project root.
const VAULT_DIR: &str = ".vault";

impl Repository {
    /// Path to the vault directory (`.vault/` at the project root).
    pub fn vault_dir(&self) -> PathBuf {
        self.root.join(VAULT_DIR)
    }

    /// Check if this repository has a vault initialized.
    pub fn has_vault(&self) -> Result<bool, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        use atomic_core::pristine::VaultTxnT;
        txn.has_vault()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Initialize the vault in this repository.
    ///
    /// Creates the vault tables in the pristine database and the
    /// `.vault/` directory structure on disk. This is idempotent —
    /// calling it on an already-initialized vault is a no-op.
    ///
    /// # Directory structure created:
    /// ```text
    /// .vault/
    /// ├── goals/
    /// ├── memory/
    /// ├── skills/
    /// ├── intents/
    /// └── scratch/
    /// ```
    pub fn init_vault(&self) -> Result<(), RepositoryError> {
        // Initialize vault tables in redb
        {
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            use atomic_core::pristine::VaultMutTxnT;
            txn.init_vault()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Create directory structure on disk
        let vault_dir = self.vault_dir();
        let subdirs = ["goals", "memory", "skills", "intents", "scratch", "prompts"];
        for subdir in &subdirs {
            std::fs::create_dir_all(vault_dir.join(subdir))?;
        }

        // Initialize knowledge graph tables
        {
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            use atomic_core::pristine::KgMutTxnT;
            txn.init_kg()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // Initialize embeddings table
        self.init_embeddings()?;

        // Install default vault content (skills, memory index, system prompt)
        self.vault_install_defaults()?;

        // NOTE: The vault files are NOT added+recorded here. The CLI init
        // command handles that in the correct sequence:
        //   1. Create .atomicignore → add → record
        //   2. Create .vault/ defaults → add → record
        // This avoids the TREE/status deadlock where add() marks files as
        // tracked but status() doesn't see them without graph positions.

        Ok(())
    }

    /// Bootstrap vault tables from an existing `.vault/` working copy.
    ///
    /// Called after pull/clone when `.vault/` files exist on disk (materialized
    /// from the graph) but the redb vault tables haven't been initialized yet.
    /// This happens when another user initialized the vault and pushed changes
    /// that were then pulled by a new clone/collaborator.
    ///
    /// Unlike `init_vault()`, this does NOT install default content — the
    /// pulled files are the authoritative source.
    pub fn bootstrap_vault_from_working_copy(&self) -> Result<(), RepositoryError> {
        log::info!("Bootstrapping vault tables from existing .vault/ working copy");

        // 1. Initialize vault tables in redb
        {
            log::info!("Initializing vault redb tables");
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            use atomic_core::pristine::VaultMutTxnT;
            txn.init_vault()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // 2. Initialize knowledge graph tables
        {
            log::info!("Initializing knowledge graph tables");
            let mut txn = self
                .pristine
                .write_txn()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            use atomic_core::pristine::KgMutTxnT;
            txn.init_kg()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.commit()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        // 3. Initialize embeddings table
        log::info!("Initializing embeddings table");
        self.init_embeddings()?;

        // 4. Deflate existing on-disk .vault/ markdown files into the redb tables
        log::info!("Recording existing .vault/ files into redb");
        let updated = self.vault_record_working_copy()?;
        log::info!(
            "Bootstrapped {} vault entries from working copy",
            updated.len()
        );

        Ok(())
    }

    /// Store content in the vault.
    ///
    /// Creates a `VaultEntry` from the given parameters, stores it in the
    /// database, and updates the manifest atomically. Returns the content
    /// hash (Blake3) of the stored entry.
    ///
    /// If an entry already exists at this path, it is replaced and the
    /// manifest is updated accordingly.
    pub fn vault_store(
        &self,
        path: &str,
        entry_type: VaultEntryType,
        content: Vec<u8>,
        frontmatter_json: String,
    ) -> Result<Hash, RepositoryError> {
        let now = chrono::Utc::now().to_rfc3339();
        let entry = VaultEntry::new(entry_type, content, frontmatter_json, now.clone());
        let content_hash = Hash::from_bytes(entry.content_hash);

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        use atomic_core::pristine::{KgMutTxnT, VaultMutTxnT, VaultTxnT};

        let previous_entry = txn
            .get_vault_entry(path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let cascade_updates = match previous_entry.as_ref() {
            Some(previous) if previous.entry_type != entry_type => {
                unlink_goal_from_intent_entries(&mut txn, path, previous, &now)?
            }
            _ => Vec::new(),
        };
        if let Some(previous_entry_type) = previous_entry
            .as_ref()
            .map(|previous| previous.entry_type)
            .filter(|previous| *previous != entry_type)
        {
            txn.init_kg()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.del_kg_node(&super::vault_triples::entry_subject(
                path,
                previous_entry_type,
            ))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        txn.put_vault_entry(path, &entry)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Update manifest
        let mut manifest = txn
            .get_vault_manifest()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        update_manifest_for_store(&mut manifest, path, &entry, previous_entry.as_ref(), &now);
        apply_cascaded_intent_manifest_updates(&mut manifest, &cascade_updates, &now);
        txn.put_vault_manifest(&manifest)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        self.vault_auto_index(path);
        for update in cascade_updates {
            self.vault_auto_index(&update.path);
            if let Err(error) = self.vault_materialize(&update.path) {
                log::warn!(
                    "Failed to materialize Intent '{}' after Goal unlink: {}",
                    update.path,
                    error
                );
            }
        }

        Ok(content_hash)
    }

    /// Store a pre-built vault entry. Also updates the manifest.
    ///
    /// This is a lower-level method for internal use and tests where the
    /// caller has already constructed a `VaultEntry`. For the common case
    /// prefer [`vault_store`](Self::vault_store) which accepts raw parameters.
    pub fn vault_store_entry(
        &self,
        path: &str,
        entry: &VaultEntry,
    ) -> Result<Hash, RepositoryError> {
        let content_hash = Hash::from_bytes(entry.content_hash);
        let now = chrono::Utc::now().to_rfc3339();

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        use atomic_core::pristine::{KgMutTxnT, VaultMutTxnT, VaultTxnT};

        let previous_entry = txn
            .get_vault_entry(path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let cascade_updates = match previous_entry.as_ref() {
            Some(previous) if previous.entry_type != entry.entry_type => {
                unlink_goal_from_intent_entries(&mut txn, path, previous, &now)?
            }
            _ => Vec::new(),
        };
        if let Some(previous_entry_type) = previous_entry
            .as_ref()
            .map(|previous| previous.entry_type)
            .filter(|previous| *previous != entry.entry_type)
        {
            txn.init_kg()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            txn.del_kg_node(&super::vault_triples::entry_subject(
                path,
                previous_entry_type,
            ))
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        txn.put_vault_entry(path, entry)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut manifest = txn
            .get_vault_manifest()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        update_manifest_for_store(&mut manifest, path, entry, previous_entry.as_ref(), &now);
        apply_cascaded_intent_manifest_updates(&mut manifest, &cascade_updates, &now);
        txn.put_vault_manifest(&manifest)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        self.vault_auto_index(path);
        for update in cascade_updates {
            self.vault_auto_index(&update.path);
            if let Err(error) = self.vault_materialize(&update.path) {
                log::warn!(
                    "Failed to materialize Intent '{}' after Goal unlink: {}",
                    update.path,
                    error
                );
            }
        }

        Ok(content_hash)
    }

    /// Automatically index a vault entry into the KG and embed it.
    ///
    /// Called as a side effect of `vault_store` and `vault_store_entry`.
    /// Best-effort — logs on failure but never fails the parent operation.
    fn vault_auto_index(&self, path: &str) {
        // Signed attestation blobs must not pollute semantic search, and they
        // contribute no KG triples (vault_extract_kg early-returns empty for
        // Attestation). Skip embedding entirely for them.
        let is_attestation = matches!(
            self.vault_retrieve(path),
            Ok(Some(e)) if e.entry_type == VaultEntryType::Attestation
        );

        // KG indexing — safe: extract_kg returns empty for Attestation, so
        // vault_store_kg deletes any stale rows and stores nothing.
        if let Err(e) = self.vault_index_kg(path) {
            log::debug!("Auto KG index for {}: {}", path, e);
        }

        if is_attestation {
            return; // do NOT embed
        }

        // Embedding
        let provider = crate::ai::resolve_embedding_provider();
        let config = EmbedConfig {
            max_chunk_tokens: 512,
            dimensions: provider.dimensions,
        };
        let embed_fn = |text: &str| -> Vec<f32> {
            provider
                .embed_sync(&[text.to_string()])
                .ok()
                .and_then(|v| v.into_iter().next())
                .unwrap_or_else(|| hash_embed(text, provider.dimensions))
        };
        if let Err(e) = self.vault_embed(path, &embed_fn, &config) {
            log::debug!("Auto embed for {}: {}", path, e);
        }
    }

    /// Retrieve a vault entry by path.
    pub fn vault_retrieve(
        &self,
        path: &str,
    ) -> Result<Option<atomic_core::pristine::VaultEntry>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        use atomic_core::pristine::VaultTxnT;
        txn.get_vault_entry(path)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// List vault entries, optionally filtered by prefix and entry type.
    pub fn vault_list(
        &self,
        prefix: &str,
        entry_type_filter: Option<atomic_core::pristine::VaultEntryType>,
    ) -> Result<Vec<atomic_core::pristine::VaultEntryMeta>, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        use atomic_core::pristine::VaultTxnT;
        txn.list_vault_entries(prefix, entry_type_filter)
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Delete a vault entry by path. Also updates the manifest.
    pub fn vault_delete(&self, path: &str) -> Result<bool, RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        use atomic_core::pristine::{EmbeddingsMutTxnT, KgMutTxnT, VaultMutTxnT, VaultTxnT};

        let previous_entry = txn
            .get_vault_entry(path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();
        let cascade_updates = if let Some(previous_entry) = previous_entry.as_ref() {
            unlink_goal_from_intent_entries(&mut txn, path, previous_entry, &now)?
        } else {
            Vec::new()
        };
        let entry_type = previous_entry
            .as_ref()
            .map(|entry| entry.entry_type)
            .unwrap_or_else(|| infer_entry_type(path));

        txn.init_kg()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let existed = txn
            .del_vault_entry(path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // KG and embeddings are derived from the Vault entry. Clean both even
        // if the source entry is already missing so a retry repairs stale data.
        let node_id = super::vault_triples::entry_subject(path, entry_type);
        // Remove projected child nodes (tasks, acceptance criteria, scope,
        // constraints) before the subject so an intent deletion does not orphan
        // them or their SATISFIES/TOUCHES edges. No-op for non-intent subjects.
        super::vault_triples::delete_intent_child_nodes(&mut txn, &node_id)?;
        txn.del_kg_node(&node_id)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.del_embeddings(path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if existed {
            let mut manifest = txn
                .get_vault_manifest()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            update_manifest_for_delete(&mut manifest, path, previous_entry.as_ref(), &now);
            apply_cascaded_intent_manifest_updates(&mut manifest, &cascade_updates, &now);
            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        for update in cascade_updates {
            self.vault_auto_index(&update.path);
            if let Err(error) = self.vault_materialize(&update.path) {
                log::warn!(
                    "Failed to materialize Intent '{}' after Goal deletion: {}",
                    update.path,
                    error
                );
            }
        }
        Ok(existed)
    }

    /// Get the vault manifest.
    pub fn vault_manifest(&self) -> Result<atomic_core::pristine::VaultManifest, RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        use atomic_core::pristine::VaultTxnT;
        txn.get_vault_manifest()
            .map_err(|e| RepositoryError::Database(e.to_string()))
    }

    /// Get just the vault manifest's merkle hash for cheap state comparison.
    ///
    /// Two repositories with the same manifest merkle have identical vault state.
    /// This is O(1) — it reads one field from the manifest.
    pub fn vault_manifest_merkle(&self) -> Result<Hash, RepositoryError> {
        let manifest = self.vault_manifest()?;
        if manifest.merkle.is_empty() {
            Ok(Hash::ZERO)
        } else {
            Hash::from_base32(manifest.merkle.as_bytes())
                .ok_or_else(|| RepositoryError::Database("invalid manifest merkle".to_string()))
        }
    }

    /// Materialize a single vault entry to disk.
    ///
    /// Reads the entry from redb and writes it as a markdown file with
    /// YAML frontmatter to `.vault/<path>`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The entry doesn't exist in redb
    /// - The file can't be written (permissions, disk full, etc.)
    pub fn vault_materialize(&self, path: &str) -> Result<(), RepositoryError> {
        let entry = self
            .vault_retrieve(path)?
            .ok_or_else(|| RepositoryError::FileNotFound {
                path: PathBuf::from(path),
            })?;

        let disk_path = self.vault_dir().join(path);

        // Ensure parent directories exist
        if let Some(parent) = disk_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let markdown = render_entry_to_markdown(&entry);
        std::fs::write(&disk_path, markdown)?;

        Ok(())
    }

    /// Materialize all vault entries to disk.
    ///
    /// Reads every entry from redb and writes it as a markdown file.
    /// Also writes the manifest as `_manifest.json`.
    ///
    /// This is useful after `atomic pull` or for rebuilding the vault
    /// directory from scratch.
    pub fn vault_materialize_all(&self) -> Result<usize, RepositoryError> {
        let entries = self.vault_list("", None)?;
        let mut count = 0;

        for meta in &entries {
            self.vault_materialize(&meta.path)?;
            count += 1;
        }

        // Also write the manifest file
        let manifest = self.vault_manifest()?;
        let manifest_path = self.vault_dir().join("_manifest.json");
        let manifest_json = serde_json::to_string_pretty(&manifest)
            .map_err(|e| RepositoryError::Serialization(e.to_string()))?;
        std::fs::write(&manifest_path, manifest_json)?;

        Ok(count)
    }

    /// Recompute the vault manifest's file_count and total_bytes from scratch.
    ///
    /// This is O(n) where n is the number of vault entries. Use it after
    /// bulk operations or when you suspect the incremental counts are off.
    pub fn vault_recompute_manifest_totals(&self) -> Result<(), RepositoryError> {
        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        use atomic_core::pristine::{VaultMutTxnT, VaultTxnT};

        let entries = txn
            .list_vault_entries("", None)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let mut manifest = txn
            .get_vault_manifest()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        manifest.file_count = entries.len() as u32;
        manifest.total_bytes = entries.iter().map(|e| e.content_size as u64).sum();
        manifest.updated_at = chrono::Utc::now().to_rfc3339();

        txn.put_vault_manifest(&manifest)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        Ok(())
    }

    /// Scan the vault working copy for changed files.
    ///
    /// Walks `.vault/` and compares each markdown file against its
    /// stored entry in redb. Returns a list of paths that are new or modified.
    ///
    /// Files are identified as changed if:
    /// - They don't exist in redb (new file)
    /// - Their body hash or user frontmatter differs from the stored entry
    ///
    /// Ignores non-`.md` files and `_manifest.json`.
    pub fn vault_scan_working_copy(&self) -> Result<Vec<VaultFileChange>, RepositoryError> {
        let vault_dir = self.vault_dir();
        if !vault_dir.exists() {
            return Ok(Vec::new());
        }

        let mut changes = Vec::new();

        // Walk the vault directory
        for entry in walkdir::WalkDir::new(&vault_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();

            // Skip directories
            if path.is_dir() {
                continue;
            }

            // Skip non-markdown files
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "md" {
                continue;
            }

            // Skip manifest
            if path.file_name().and_then(|n| n.to_str()) == Some("_manifest.json") {
                continue;
            }

            // Get vault-relative path
            let rel_path =
                path.strip_prefix(&vault_dir)
                    .map_err(|_| RepositoryError::InvalidOperation {
                        message: format!("path not under vault: {}", path.display()),
                    })?;
            let rel_path_str = rel_path.to_string_lossy().to_string();
            // Normalize to forward slashes (for Windows compat)
            let rel_path_str = rel_path_str.replace('\\', "/");

            // Read file content
            let file_content = std::fs::read_to_string(path)?;

            // Parse into frontmatter + body
            let (frontmatter_json, body) = parse_markdown_frontmatter(&file_content);

            // Body hashes remain useful for content/embedding caches, but
            // frontmatter is also knowledge: status, labels, identity, and
            // descriptions affect retrieval and KG extraction.
            let body_bytes = body.as_bytes();
            let new_hash = Hash::of(body_bytes);

            // Check if this is new or changed
            let existing = self.vault_retrieve(&rel_path_str)?;
            let change_type = match &existing {
                Some(entry)
                    if Hash::from_bytes(entry.content_hash) == new_hash
                        && frontmatter_json_equal(&entry.frontmatter_json, &frontmatter_json) =>
                {
                    continue; // Unchanged, skip
                }
                Some(_) => VaultChangeType::Modified,
                None => VaultChangeType::New,
            };

            changes.push(VaultFileChange {
                path: rel_path_str,
                change_type,
                frontmatter_json,
                content: body_bytes.to_vec(),
            });
        }

        // Also check for deleted entries (in redb but not on disk)
        let all_entries = self.vault_list("", None)?;
        for meta in all_entries {
            let disk_path = vault_dir.join(&meta.path);
            if !disk_path.exists() {
                changes.push(VaultFileChange {
                    path: meta.path,
                    change_type: VaultChangeType::Deleted,
                    frontmatter_json: String::new(),
                    content: Vec::new(),
                });
            }
        }

        Ok(changes)
    }

    /// Deflate changed vault files from disk into redb.
    ///
    /// Scans the vault working copy, detects changes, and updates the
    /// database. Returns the list of paths that were updated.
    ///
    /// This is the inverse of `vault_materialize_all`. Together they form
    /// the inflate/deflate cycle:
    /// - `vault_materialize_all`: redb -> disk (for humans to read/edit)
    /// - `vault_record_working_copy`: disk -> redb (after humans edit)
    pub fn vault_record_working_copy(&self) -> Result<Vec<String>, RepositoryError> {
        let changes = self.vault_scan_working_copy()?;

        let mut updated_paths = Vec::new();

        for change in &changes {
            match change.change_type {
                VaultChangeType::New | VaultChangeType::Modified => {
                    // Infer entry type from path
                    let entry_type = infer_entry_type(&change.path);

                    self.vault_store(
                        &change.path,
                        entry_type,
                        change.content.clone(),
                        change.frontmatter_json.clone(),
                    )?;

                    updated_paths.push(change.path.clone());
                }
                VaultChangeType::Deleted => {
                    self.vault_delete(&change.path)?;
                    updated_paths.push(change.path.clone());
                }
            }
        }

        // Index any intent entries the manifest is missing — even when no
        // file changed. Entries can predate this call (clone bootstrap
        // deflates inherited files) and `update_manifest_for_store` leaves
        // intent summaries to higher-level methods; without this, inherited
        // intents exist in redb but never show up in `intent list`.
        self.vault_reconcile_intent_manifest()?;

        Ok(updated_paths)
    }
}

// ── Manifest helpers ────────────────────────────────────────────────────

struct CascadedIntentUpdate {
    path: String,
    previous_entry: VaultEntry,
    current_entry: VaultEntry,
    intent_id: Option<String>,
    goal_count: u32,
}

/// Remove a Goal reference from every current Intent source before that Goal
/// is deleted or reclassified. The Vault entries are updated in the caller's
/// transaction; KG re-indexing and materialization happen after commit.
fn unlink_goal_from_intent_entries(
    txn: &mut atomic_core::pristine::WriteTxn<'_>,
    path: &str,
    entry: &VaultEntry,
    now: &str,
) -> Result<Vec<CascadedIntentUpdate>, RepositoryError> {
    use atomic_core::pristine::{VaultMutTxnT, VaultTxnT};

    if entry.entry_type != VaultEntryType::Session {
        return Ok(Vec::new());
    }
    let Some(goal_id) = vault_frontmatter_string(entry, "goal_id").or_else(|| {
        path.strip_prefix("goals/")
            .and_then(|rest| rest.strip_suffix("/_goal.md"))
            .map(str::to_string)
    }) else {
        return Ok(Vec::new());
    };

    let intents = txn
        .list_vault_entries("intents/", Some(VaultEntryType::Intent))
        .map_err(|e| RepositoryError::Database(e.to_string()))?;
    let mut updates = Vec::new();
    for meta in intents {
        let Some(mut current_entry) = txn
            .get_vault_entry(&meta.path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?
        else {
            continue;
        };
        let previous_entry = current_entry.clone();
        let Ok(mut frontmatter) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
            &current_entry.frontmatter_json,
        ) else {
            continue;
        };

        let Some(goals) = frontmatter
            .get_mut("goals")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        let previous_len = goals.len();
        goals.retain(|goal| goal.as_str() != Some(goal_id.as_str()));
        if goals.len() == previous_len {
            continue;
        }
        let goal_count = goals.len() as u32;
        let intent_id = frontmatter
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        current_entry.frontmatter_json = serde_json::to_string(&frontmatter)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        current_entry.updated_at = now.to_string();
        txn.put_vault_entry(&meta.path, &current_entry)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        updates.push(CascadedIntentUpdate {
            path: meta.path,
            previous_entry,
            current_entry,
            intent_id,
            goal_count,
        });
    }
    Ok(updates)
}

fn apply_cascaded_intent_manifest_updates(
    manifest: &mut VaultManifest,
    updates: &[CascadedIntentUpdate],
    now: &str,
) {
    for update in updates {
        update_manifest_for_store(
            manifest,
            &update.path,
            &update.current_entry,
            Some(&update.previous_entry),
            now,
        );
        let summary_id = update
            .intent_id
            .as_ref()
            .filter(|intent_id| manifest.intents.contains_key(*intent_id))
            .cloned()
            .or_else(|| {
                manifest
                    .intents
                    .iter()
                    .find(|(_, summary)| summary.vault_path == update.path)
                    .map(|(intent_id, _)| intent_id.clone())
            });
        if let Some(summary_id) = summary_id {
            if let Some(summary) = manifest.intents.get_mut(&summary_id) {
                summary.goals = update.goal_count;
            }
        }
    }
}

fn vault_frontmatter_string(entry: &VaultEntry, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&entry.frontmatter_json)
        .ok()
        .and_then(|value| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

/// Update the manifest when a vault entry is stored.
fn update_manifest_for_store(
    manifest: &mut VaultManifest,
    path: &str,
    entry: &VaultEntry,
    previous_entry: Option<&VaultEntry>,
    now: &str,
) {
    let hash_str = Hash::from_bytes(entry.content_hash).to_base32();
    let bytes = entry.content_size() as u64;

    // A path may change entry type. Remove the old classification and any
    // higher-level Goal/Intent summary before inserting the new one so the
    // manifest represents one current entry.
    if let Some(previous) =
        previous_entry.filter(|previous| previous.entry_type != entry.entry_type)
    {
        remove_manifest_summary_for_entry(manifest, path, previous);
    }
    manifest.memory.remove(path);
    manifest.skills.remove(path);

    // Update type-specific index.
    match entry.entry_type {
        VaultEntryType::Memory => {
            manifest.memory.insert(
                path.to_string(),
                MemorySummary {
                    hash: hash_str.clone(),
                    bytes,
                    updated_at: now.to_string(),
                },
            );
        }
        VaultEntryType::Skill => {
            manifest.skills.insert(
                path.to_string(),
                SkillSummary {
                    hash: hash_str.clone(),
                    bytes,
                },
            );
        }
        // For Session, Intent, ToolResult, Scratch — we update counts only.
        // Full session/intent summaries are populated by higher-level methods.
        _ => {}
    }

    // Maintain exact incremental totals for insert and replacement.
    if let Some(previous) = previous_entry {
        manifest.total_bytes = manifest
            .total_bytes
            .saturating_sub(previous.content_size() as u64)
            .saturating_add(bytes);
    } else {
        manifest.file_count = manifest.file_count.saturating_add(1);
        manifest.total_bytes = manifest.total_bytes.saturating_add(bytes);
    }

    // Update merkle: Hash(previous_merkle || content_hash)
    let prev_merkle = if manifest.merkle.is_empty() {
        Merkle::ZERO
    } else {
        Merkle::from_base32(manifest.merkle.as_bytes()).unwrap_or(Merkle::ZERO)
    };
    let content_hash = Hash::from_bytes(entry.content_hash);
    let new_merkle = prev_merkle.next(&content_hash);
    manifest.merkle = new_merkle.to_base32();

    manifest.updated_at = now.to_string();
}

/// Update the manifest when a vault entry is deleted.
fn update_manifest_for_delete(
    manifest: &mut VaultManifest,
    path: &str,
    previous_entry: Option<&VaultEntry>,
    now: &str,
) {
    // Remove from type-specific indexes
    manifest.memory.remove(path);
    manifest.skills.remove(path);
    if let Some(previous_entry) = previous_entry {
        remove_manifest_summary_for_entry(manifest, path, previous_entry);
    }

    // Decrement file count (floor at 0)
    manifest.file_count = manifest.file_count.saturating_sub(1);
    if let Some(previous_entry) = previous_entry {
        manifest.total_bytes = manifest
            .total_bytes
            .saturating_sub(previous_entry.content_size() as u64);
    }
    // The merkle also changes on delete.
    let prev_merkle = if manifest.merkle.is_empty() {
        Merkle::ZERO
    } else {
        Merkle::from_base32(manifest.merkle.as_bytes()).unwrap_or(Merkle::ZERO)
    };
    // Hash path as the "delete event" to advance the merkle
    let delete_hash = Hash::of(format!("delete:{}", path).as_bytes());
    let new_merkle = prev_merkle.next(&delete_hash);
    manifest.merkle = new_merkle.to_base32();

    manifest.updated_at = now.to_string();
}

/// Remove the manifest summary owned by a Goal or Intent Vault entry.
///
/// Goal summaries are keyed by `goal_id`, while Intent summaries are keyed by
/// display ID and carry their Vault path. Neither map is keyed by `path`, so a
/// plain `remove(path)` leaves ghost summaries behind.
fn remove_manifest_summary_for_entry(manifest: &mut VaultManifest, path: &str, entry: &VaultEntry) {
    match entry.entry_type {
        VaultEntryType::Session => {
            let goal_id = vault_frontmatter_string(entry, "goal_id").or_else(|| {
                path.strip_prefix("goals/")
                    .and_then(|rest| rest.strip_suffix("/_goal.md"))
                    .map(str::to_string)
            });
            if let Some(goal) = goal_id.and_then(|goal_id| manifest.goals.remove(&goal_id)) {
                if let Some(intent_id) = goal.intent {
                    if let Some(intent) = manifest.intents.get_mut(&intent_id) {
                        intent.goals = intent.goals.saturating_sub(1);
                    }
                }
            }
        }
        VaultEntryType::Intent => {
            let intent_id = vault_frontmatter_string(entry, "id");
            manifest.intents.retain(|id, summary| {
                summary.vault_path != path && intent_id.as_deref() != Some(id.as_str())
            });
        }
        _ => {}
    }
}

// ── Materialization helpers ─────────────────────────────────────────────

/// Render a vault entry to a markdown string with YAML frontmatter.
///
/// The frontmatter is constructed from:
/// 1. The entry's `frontmatter_json` fields (user-provided metadata)
/// 2. System metadata (entry_type, content_hash, created_at, updated_at)
///
/// The system fields are always included to enable round-trip (deflation
/// can reconstruct the VaultEntry from the file).
fn render_entry_to_markdown(entry: &VaultEntry) -> String {
    let mut output = String::new();
    output.push_str("---\n");

    // Parse the frontmatter JSON into a map so we can merge system fields
    let mut fm: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&entry.frontmatter_json).unwrap_or_default();

    // Add system fields (these take precedence)
    fm.insert(
        "entry_type".to_string(),
        serde_json::Value::String(entry.entry_type.to_string()),
    );
    fm.insert(
        "content_hash".to_string(),
        serde_json::Value::String(Hash::from_bytes(entry.content_hash).to_base32()),
    );
    fm.insert(
        "created_at".to_string(),
        serde_json::Value::String(entry.created_at.clone()),
    );
    fm.insert(
        "updated_at".to_string(),
        serde_json::Value::String(entry.updated_at.clone()),
    );

    // Write frontmatter in a stable order: user fields first (sorted), then system fields
    let system_keys = ["entry_type", "content_hash", "created_at", "updated_at"];
    let mut user_keys: Vec<&String> = fm
        .keys()
        .filter(|k| !system_keys.contains(&k.as_str()))
        .collect();
    user_keys.sort();

    // Write user fields
    for key in &user_keys {
        write_frontmatter_field(&mut output, key, &fm[key.as_str()]);
    }

    // Write system fields
    for key in &system_keys {
        if let Some(value) = fm.get(*key) {
            write_frontmatter_field(&mut output, key, value);
        }
    }

    output.push_str("---\n");

    // Content body
    let content = String::from_utf8_lossy(&entry.content_bytes);
    if !content.is_empty() {
        output.push_str(&content);
        // Ensure file ends with a newline
        if !content.ends_with('\n') {
            output.push('\n');
        }
    }

    output
}

/// Write a single frontmatter field in YAML format.
fn write_frontmatter_field(output: &mut String, key: &str, value: &serde_json::Value) {
    use std::fmt::Write;
    match value {
        serde_json::Value::String(s) => {
            // Keep simple strings readable, but quote values that our parser
            // would otherwise reinterpret as another scalar/container type.
            // JSON string syntax is valid YAML and preserves escapes.
            let parsed_unquoted = parse_yaml_value(s);
            let has_edge_whitespace = s.chars().next().is_some_and(char::is_whitespace)
                || s.chars().last().is_some_and(char::is_whitespace);
            if parsed_unquoted != serde_json::Value::String(s.clone())
                || s.contains(": ")
                || s.contains('#')
                || s.contains('\n')
                || s.starts_with('{')
                || s.starts_with('[')
                || s.starts_with('"')
                || s.starts_with('\'')
                || has_edge_whitespace
            {
                let rendered = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
                let _ = writeln!(output, "{}: {}", key, rendered);
            } else {
                let _ = writeln!(output, "{}: {}", key, s);
            }
        }
        serde_json::Value::Number(n) => {
            let _ = writeln!(output, "{}: {}", key, n);
        }
        serde_json::Value::Bool(b) => {
            let _ = writeln!(output, "{}: {}", key, b);
        }
        serde_json::Value::Array(arr) => {
            let rendered = serde_json::to_string(arr).unwrap_or_else(|_| "[]".to_string());
            let _ = writeln!(output, "{}: {}", key, rendered);
        }
        serde_json::Value::Object(_) => {
            // Inline JSON for nested objects
            let _ = writeln!(output, "{}: {}", key, value);
        }
        serde_json::Value::Null => {
            let _ = writeln!(output, "{}: null", key);
        }
    }
}

// ── Deflation helpers ───────────────────────────────────────────────────

/// Type of change detected in the vault working copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultChangeType {
    New,
    Modified,
    Deleted,
}

/// A detected change in the vault working copy.
#[derive(Debug, Clone)]
pub struct VaultFileChange {
    /// Vault-relative path.
    pub path: String,
    /// What kind of change was detected.
    pub change_type: VaultChangeType,
    /// Parsed frontmatter as JSON (empty for deletions).
    pub frontmatter_json: String,
    /// File content body without frontmatter (empty for deletions).
    pub content: Vec<u8>,
}

/// Parse a markdown file's YAML frontmatter and body.
///
/// Expects the file to optionally start with `---\n...\n---\n`.
/// Returns `(frontmatter_json, body)` where frontmatter_json is the
/// YAML frontmatter converted to a JSON string, and body is everything
/// after the closing `---`.
///
/// If there's no frontmatter, returns `("{}", full_content)`.
/// Parse vault markdown frontmatter. Public within the crate so that
/// `vault_intent_update` can read disk content before storing.
pub(crate) fn parse_vault_frontmatter(content: &str) -> (String, String) {
    parse_markdown_frontmatter(content)
}

fn parse_markdown_frontmatter(content: &str) -> (String, String) {
    // Check for frontmatter delimiter
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return ("{}".to_string(), content.to_string());
    }

    // Find closing delimiter
    let after_first = if content.starts_with("---\r\n") { 5 } else { 4 };
    let rest = &content[after_first..];

    let (close_idx, skip_len) = if let Some(idx) = rest.find("\n---\n") {
        (idx, 5) // "\n---\n" is 5 bytes
    } else if let Some(idx) = rest.find("\n---\r\n") {
        (idx, 6) // "\n---\r\n" is 6 bytes
    } else if rest.ends_with("\n---") {
        (rest.len() - 3, 3)
    } else {
        // No closing delimiter — treat entire content as body
        return ("{}".to_string(), content.to_string());
    };

    let yaml_str = &rest[..close_idx];
    let body_start = after_first + close_idx + skip_len;
    let body = if body_start <= content.len() {
        content[body_start..].to_string()
    } else {
        String::new()
    };

    // Convert YAML frontmatter to JSON
    let frontmatter_json = yaml_frontmatter_to_json(yaml_str);

    (frontmatter_json, body)
}

fn frontmatter_json_equal(left: &str, right: &str) -> bool {
    match (
        serde_json::from_str::<serde_json::Value>(left),
        serde_json::from_str::<serde_json::Value>(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Convert simple YAML frontmatter to a JSON string.
///
/// Handles common YAML patterns:
/// - `key: value` -> `{"key": "value"}`
/// - `key: 123` -> `{"key": 123}`
/// - `key: true/false` -> `{"key": true/false}`
/// - `key: [a, b, c]` -> `{"key": ["a", "b", "c"]}`
/// - `key: {json}` -> `{"key": {json}}`
///
/// This is intentionally simple — we don't need a full YAML parser.
/// The frontmatter is our own output (from materialization), so we
/// know the format.
fn yaml_frontmatter_to_json(yaml: &str) -> String {
    let mut map = serde_json::Map::new();

    for line in yaml.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Split on first ': '
        let Some(colon_idx) = line.find(": ") else {
            // Handle `key:` with no value (treated as empty string)
            if let Some(key) = line.strip_suffix(':') {
                map.insert(
                    key.trim().to_string(),
                    serde_json::Value::String(String::new()),
                );
            }
            continue;
        };

        let key = line[..colon_idx].trim().to_string();
        let value_str = line[colon_idx + 2..].trim();

        // Skip system fields that we inject during materialization
        // (they'll be reconstructed, not stored in frontmatter_json)
        if matches!(
            key.as_str(),
            "entry_type" | "content_hash" | "created_at" | "updated_at"
        ) {
            continue;
        }

        let value = parse_yaml_value(value_str);
        map.insert(key, value);
    }

    serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
}

/// Parse a YAML value string into a serde_json::Value.
fn parse_yaml_value(s: &str) -> serde_json::Value {
    // Quoted values remain strings even when their contents look like another
    // scalar type. Decode JSON-style double-quoted escapes losslessly.
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        return serde_json::from_str::<String>(s)
            .map(serde_json::Value::String)
            .unwrap_or_else(|_| serde_json::Value::String(s[1..s.len() - 1].to_string()));
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        return serde_json::Value::String(s[1..s.len() - 1].replace("''", "'"));
    }

    // Try as integer
    if let Ok(n) = s.parse::<i64>() {
        return serde_json::Value::Number(n.into());
    }
    // Try as float
    if let Ok(n) = s.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(n) {
            return serde_json::Value::Number(n);
        }
    }

    // Try as boolean / null
    match s {
        "true" => return serde_json::Value::Bool(true),
        "false" => return serde_json::Value::Bool(false),
        "null" => return serde_json::Value::Null,
        _ => {}
    }

    // Materialized arrays/objects use JSON syntax, which is also valid YAML.
    // Parse it first so quoted commas and nested values remain lossless.
    if (s.starts_with('[') && s.ends_with(']')) || (s.starts_with('{') && s.ends_with('}')) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(s) {
            return value;
        }
    }

    // Also accept simple hand-authored YAML arrays such as [a, b, c].
    if s.starts_with('[') && s.ends_with(']') {
        let inner = s[1..s.len() - 1].trim();
        if inner.is_empty() {
            return serde_json::Value::Array(Vec::new());
        }
        let items: Vec<serde_json::Value> = inner
            .split(',')
            .map(|item| parse_yaml_value(item.trim()))
            .collect();
        return serde_json::Value::Array(items);
    }

    // Default: string
    serde_json::Value::String(s.to_string())
}

/// Infer the vault entry type from its path.
fn infer_entry_type(path: &str) -> VaultEntryType {
    if path.starts_with("goals/") {
        if path.contains("toolu_") || path.ends_with("_tool.md") {
            VaultEntryType::ToolResult
        } else {
            VaultEntryType::Session
        }
    } else if path.starts_with("memory/") {
        VaultEntryType::Memory
    } else if path.starts_with("skills/") {
        VaultEntryType::Skill
    } else if path.starts_with("intents/") {
        VaultEntryType::Intent
    } else if path.starts_with("attestations/") {
        VaultEntryType::Attestation
    } else {
        // scratch/ prefix or any unknown path
        VaultEntryType::Scratch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::pristine::vault::{GoalSummary, IntentSummary, TokenCounts};
    use atomic_core::pristine::{EmbeddingsTxnT, VaultEntryType, VaultMutTxnT, VaultTxnT};
    use tempfile::tempdir;

    #[test]
    fn test_init_vault() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        // Vault should not exist yet
        assert!(!repo.has_vault().unwrap());
        assert!(!repo.vault_dir().exists());

        // Initialize vault
        repo.init_vault().unwrap();

        // Vault should now exist
        assert!(repo.has_vault().unwrap());
        assert!(repo.vault_dir().exists());

        // Subdirectories should exist
        assert!(repo.vault_dir().join("goals").exists());
        assert!(repo.vault_dir().join("memory").exists());
        assert!(repo.vault_dir().join("skills").exists());
        assert!(repo.vault_dir().join("intents").exists());
        assert!(repo.vault_dir().join("scratch").exists());
    }

    #[test]
    fn test_init_vault_idempotent() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        repo.init_vault().unwrap();
        repo.init_vault().unwrap(); // Should not fail

        assert!(repo.has_vault().unwrap());
    }

    #[test]
    fn test_vault_store_returns_hash() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let hash = repo
            .vault_store(
                "memory/architecture.md",
                VaultEntryType::Memory,
                b"# Architecture\nWe use crates.".to_vec(),
                r#"{"name":"architecture"}"#.to_string(),
            )
            .unwrap();

        // Hash should be non-zero
        assert_ne!(hash, Hash::ZERO);

        // Retrieve should work
        let entry = repo
            .vault_retrieve("memory/architecture.md")
            .unwrap()
            .unwrap();
        assert_eq!(Hash::from_bytes(entry.content_hash), hash);
    }

    #[test]
    fn test_vault_store_entry_returns_hash() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let entry = VaultEntry::new(
            VaultEntryType::Memory,
            b"# Architecture\nWe use crates.".to_vec(),
            r#"{"name":"architecture"}"#.to_string(),
            "2025-07-15T12:00:00Z".to_string(),
        );

        let hash = repo
            .vault_store_entry("memory/architecture.md", &entry)
            .unwrap();

        assert_ne!(hash, Hash::ZERO);

        let retrieved = repo
            .vault_retrieve("memory/architecture.md")
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.content_bytes, entry.content_bytes);
        assert_eq!(retrieved.entry_type, VaultEntryType::Memory);
        assert_eq!(Hash::from_bytes(retrieved.content_hash), hash);
    }

    #[test]
    fn test_manifest_auto_updates_on_store() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let m1 = repo.vault_manifest().unwrap();
        // Defaults (skill + memory index) are now installed by init_vault,
        // so the merkle is already non-empty.
        assert!(!m1.merkle.is_empty());

        repo.vault_store(
            "memory/arch.md",
            VaultEntryType::Memory,
            b"content".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let m2 = repo.vault_manifest().unwrap();
        assert!(!m2.merkle.is_empty());
        assert!(m2.memory.contains_key("memory/arch.md"));

        // Second store should change merkle again
        repo.vault_store(
            "memory/conv.md",
            VaultEntryType::Memory,
            b"content2".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let m3 = repo.vault_manifest().unwrap();
        assert_ne!(m2.merkle, m3.merkle);
        // 2 test entries + 1 default (memory/MEMORY.md)
        assert_eq!(m3.memory.len(), 3);
    }

    #[test]
    fn test_manifest_auto_updates_on_delete() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/arch.md",
            VaultEntryType::Memory,
            b"content".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let m1 = repo.vault_manifest().unwrap();

        repo.vault_delete("memory/arch.md").unwrap();

        let m2 = repo.vault_manifest().unwrap();
        assert_ne!(m1.merkle, m2.merkle);
        assert!(!m2.memory.contains_key("memory/arch.md"));
    }

    #[test]
    fn test_manifest_replacement_tracks_current_type_and_totals() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let baseline = repo.vault_manifest().unwrap();
        let path = "memory/reclassified.md";

        repo.vault_store(
            path,
            VaultEntryType::Memory,
            b"old".to_vec(),
            r#"{"name":"old"}"#.to_string(),
        )
        .unwrap();
        let inserted = repo.vault_manifest().unwrap();
        assert_eq!(inserted.file_count, baseline.file_count + 1);
        assert_eq!(inserted.total_bytes, baseline.total_bytes + 3);
        assert!(inserted.memory.contains_key(path));

        repo.vault_store(
            path,
            VaultEntryType::Skill,
            b"new skill".to_vec(),
            r#"{"name":"new"}"#.to_string(),
        )
        .unwrap();
        let replaced = repo.vault_manifest().unwrap();
        assert_eq!(replaced.file_count, baseline.file_count + 1);
        assert_eq!(replaced.total_bytes, baseline.total_bytes + 9);
        assert!(!replaced.memory.contains_key(path));
        assert!(replaced.skills.contains_key(path));

        assert!(repo.vault_delete(path).unwrap());
        let deleted = repo.vault_manifest().unwrap();
        assert_eq!(deleted.file_count, baseline.file_count);
        assert_eq!(deleted.total_bytes, baseline.total_bytes);
        assert!(!deleted.skills.contains_key(path));
    }

    #[test]
    fn test_vault_delete_removes_goal_and_intent_manifest_summaries() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let goal_path = "goals/test-goal/_goal.md";
        let intent_path = "intents/manual/alice/1/intent.md";

        repo.vault_store(
            goal_path,
            VaultEntryType::Session,
            b"# Goal".to_vec(),
            r#"{"goal_id":"test-goal","intent":"TEST-1"}"#.to_string(),
        )
        .unwrap();
        repo.vault_store(
            intent_path,
            VaultEntryType::Intent,
            b"# Intent".to_vec(),
            r#"{"goals":["test-goal"],"id":"TEST-1","title":"Test"}"#.to_string(),
        )
        .unwrap();

        {
            let mut txn = repo.pristine.write_txn().unwrap();
            let mut manifest = txn.get_vault_manifest().unwrap();
            manifest.goals.insert(
                "test-goal".to_string(),
                GoalSummary {
                    developer: "alice".to_string(),
                    intent: Some("TEST-1".to_string()),
                    status: "active".to_string(),
                    started_at: "2025-01-01T00:00:00Z".to_string(),
                    turns: 0,
                    tokens: TokenCounts::default(),
                    tool_results: Vec::new(),
                    findings_hash: None,
                    bytes: 0,
                },
            );
            manifest.intents.insert(
                "TEST-1".to_string(),
                IntentSummary {
                    status: "backlog".to_string(),
                    priority: "medium".to_string(),
                    assignee: None,
                    goals: 1,
                    blocked_by: Vec::new(),
                    title: "Test".to_string(),
                    vault_path: intent_path.to_string(),
                    ..Default::default()
                },
            );
            txn.put_vault_manifest(&manifest).unwrap();
            txn.commit().unwrap();
        }

        assert!(repo.vault_delete(goal_path).unwrap());
        let after_goal = repo.vault_manifest().unwrap();
        assert!(!after_goal.goals.contains_key("test-goal"));
        assert_eq!(after_goal.intents["TEST-1"].goals, 0);
        let intent = repo.vault_retrieve(intent_path).unwrap().unwrap();
        let intent_frontmatter: serde_json::Value =
            serde_json::from_str(&intent.frontmatter_json).unwrap();
        assert_eq!(intent_frontmatter["goals"], serde_json::json!([]));
        assert!(repo.vault_dir().join(intent_path).exists());

        // Reclassifying a Goal path follows the same unlink lifecycle.
        let reclassified_goal_path = "goals/reclassified/_goal.md";
        repo.vault_store(
            reclassified_goal_path,
            VaultEntryType::Session,
            b"# Reclassified Goal".to_vec(),
            r#"{"goal_id":"reclassified"}"#.to_string(),
        )
        .unwrap();
        repo.vault_store(
            intent_path,
            VaultEntryType::Intent,
            b"# Intent".to_vec(),
            r#"{"goals":["reclassified"],"id":"TEST-1","title":"Test"}"#.to_string(),
        )
        .unwrap();
        {
            let mut txn = repo.pristine.write_txn().unwrap();
            let mut manifest = txn.get_vault_manifest().unwrap();
            manifest.goals.insert(
                "reclassified".to_string(),
                GoalSummary {
                    developer: "alice".to_string(),
                    intent: Some("TEST-1".to_string()),
                    status: "active".to_string(),
                    started_at: "2025-01-01T00:00:00Z".to_string(),
                    turns: 0,
                    tokens: TokenCounts::default(),
                    tool_results: Vec::new(),
                    findings_hash: None,
                    bytes: 0,
                },
            );
            manifest.intents.get_mut("TEST-1").unwrap().goals = 1;
            txn.put_vault_manifest(&manifest).unwrap();
            txn.commit().unwrap();
        }
        repo.vault_store(
            reclassified_goal_path,
            VaultEntryType::Scratch,
            b"No longer a Goal".to_vec(),
            "{}".to_string(),
        )
        .unwrap();
        let after_reclassification = repo.vault_manifest().unwrap();
        assert!(!after_reclassification.goals.contains_key("reclassified"));
        assert_eq!(after_reclassification.intents["TEST-1"].goals, 0);
        let intent = repo.vault_retrieve(intent_path).unwrap().unwrap();
        let intent_frontmatter: serde_json::Value =
            serde_json::from_str(&intent.frontmatter_json).unwrap();
        assert_eq!(intent_frontmatter["goals"], serde_json::json!([]));

        assert!(repo.vault_delete(intent_path).unwrap());
        let after_intent = repo.vault_manifest().unwrap();
        assert!(!after_intent.intents.contains_key("TEST-1"));
    }

    #[test]
    fn test_vault_manifest_merkle() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // init_vault installs defaults so merkle is already non-zero
        let merkle_after_init = repo.vault_manifest_merkle().unwrap();
        assert_ne!(merkle_after_init, Hash::ZERO);

        // After an additional store, merkle changes again
        repo.vault_store(
            "memory/test.md",
            VaultEntryType::Memory,
            b"test".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let merkle = repo.vault_manifest_merkle().unwrap();
        assert_ne!(merkle, Hash::ZERO);
        assert_ne!(merkle, merkle_after_init);
    }

    #[test]
    fn test_manifest_unchanged_on_read() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/arch.md",
            VaultEntryType::Memory,
            b"content".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let m1 = repo.vault_manifest().unwrap();

        // Read operations should not change the manifest
        repo.vault_retrieve("memory/arch.md").unwrap();
        repo.vault_list("", None).unwrap();
        repo.vault_manifest().unwrap();

        let m2 = repo.vault_manifest().unwrap();
        assert_eq!(m1.merkle, m2.merkle);
    }

    #[test]
    fn test_vault_list_and_delete() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/arch.md",
            VaultEntryType::Memory,
            b"content1".to_vec(),
            "{}".to_string(),
        )
        .unwrap();
        repo.vault_store(
            "goals/abc/_goal.md",
            VaultEntryType::Session,
            b"content2".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        // List all (4 defaults from init_vault + 2 test entries)
        let all = repo.vault_list("", None).unwrap();
        assert_eq!(all.len(), 6);

        // Filter by type
        let sessions = repo.vault_list("", Some(VaultEntryType::Session)).unwrap();
        assert_eq!(sessions.len(), 1);

        // Delete
        assert!(repo.vault_delete("memory/arch.md").unwrap());
        assert!(!repo.vault_delete("memory/arch.md").unwrap());

        let all = repo.vault_list("", None).unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn test_vault_delete_cleans_embeddings() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let path = "memory/delete-embedding.md";

        repo.vault_store(
            path,
            VaultEntryType::Memory,
            b"Unique semantic memory content".to_vec(),
            r#"{"name":"delete-embedding"}"#.to_string(),
        )
        .unwrap();

        {
            let txn = repo.pristine.read_txn().unwrap();
            assert!(txn.get_embedding(path, 0).unwrap().is_some());
        }

        assert!(repo.vault_delete(path).unwrap());
        let txn = repo.pristine.read_txn().unwrap();
        assert!(txn.get_embedding(path, 0).unwrap().is_none());
    }

    #[test]
    fn test_vault_manifest() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let manifest = repo.vault_manifest().unwrap();
        assert_eq!(manifest.version, 1);
        assert!(manifest.goals.is_empty());
    }

    #[test]
    fn test_vault_recompute_totals() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        for i in 0..5 {
            repo.vault_store(
                &format!("memory/file{}.md", i),
                VaultEntryType::Memory,
                format!("content {}", i).into_bytes(),
                "{}".to_string(),
            )
            .unwrap();
        }

        repo.vault_recompute_manifest_totals().unwrap();

        let manifest = repo.vault_manifest().unwrap();
        // 5 test entries + 4 defaults (2 skills + system prompt + memory index)
        assert_eq!(manifest.file_count, 9);
        assert!(manifest.total_bytes > 0);
    }

    #[test]
    fn test_vault_skill_in_manifest() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "skills/rust-patterns.md",
            VaultEntryType::Skill,
            b"# Rust Patterns\nUse enums for state.".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let manifest = repo.vault_manifest().unwrap();
        assert!(manifest.skills.contains_key("skills/rust-patterns.md"));
        let skill = &manifest.skills["skills/rust-patterns.md"];
        assert!(!skill.hash.is_empty());
        assert!(skill.bytes > 0);
    }

    #[test]
    fn test_vault_delete_nonexistent_does_not_change_manifest() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/arch.md",
            VaultEntryType::Memory,
            b"content".to_vec(),
            "{}".to_string(),
        )
        .unwrap();

        let m1 = repo.vault_manifest().unwrap();

        // Delete something that doesn't exist
        let existed = repo.vault_delete("memory/nonexistent.md").unwrap();
        assert!(!existed);

        let m2 = repo.vault_manifest().unwrap();
        assert_eq!(m1.merkle, m2.merkle);
    }

    #[test]
    fn test_vault_materialize_single() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/architecture.md",
            VaultEntryType::Memory,
            b"# Architecture\n\nWe use crates for modularity.".to_vec(),
            r#"{"name":"architecture","type":"project"}"#.to_string(),
        )
        .unwrap();

        repo.vault_materialize("memory/architecture.md").unwrap();

        let disk_path = repo.vault_dir().join("memory/architecture.md");
        assert!(disk_path.exists());

        let content = std::fs::read_to_string(&disk_path).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("entry_type: memory"));
        assert!(content.contains("name: architecture"));
        assert!(content.contains("# Architecture"));
        assert!(content.contains("We use crates for modularity."));
    }

    #[test]
    fn test_vault_materialize_all() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "memory/arch.md",
            VaultEntryType::Memory,
            b"content1".to_vec(),
            "{}".to_string(),
        )
        .unwrap();
        repo.vault_store(
            "skills/run-tests.md",
            VaultEntryType::Skill,
            b"content2".to_vec(),
            r#"{"name":"run-tests"}"#.to_string(),
        )
        .unwrap();

        // 2 test entries + 4 defaults from init_vault
        let count = repo.vault_materialize_all().unwrap();
        assert_eq!(count, 6);

        assert!(repo.vault_dir().join("memory/arch.md").exists());
        assert!(repo.vault_dir().join("skills/run-tests.md").exists());
        assert!(repo.vault_dir().join("_manifest.json").exists());

        // Verify manifest JSON is valid
        let manifest_content =
            std::fs::read_to_string(repo.vault_dir().join("_manifest.json")).unwrap();
        let manifest: VaultManifest = serde_json::from_str(&manifest_content).unwrap();
        assert_eq!(manifest.version, 1);
    }

    #[test]
    fn test_vault_materialize_creates_subdirs() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        repo.vault_store(
            "goals/2025-07-15-abc123/_goal.md",
            VaultEntryType::Session,
            b"# Goal transcript".to_vec(),
            r#"{"goal_id":"abc123"}"#.to_string(),
        )
        .unwrap();

        repo.vault_materialize("goals/2025-07-15-abc123/_goal.md")
            .unwrap();

        assert!(repo
            .vault_dir()
            .join("goals/2025-07-15-abc123/_goal.md")
            .exists());
    }

    #[test]
    fn test_vault_materialize_nonexistent_entry() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let result = repo.vault_materialize("nonexistent.md");
        assert!(result.is_err());
    }

    #[test]
    fn test_render_entry_to_markdown_format() {
        let entry = VaultEntry::new(
            VaultEntryType::Memory,
            b"# Hello\n\nWorld\n".to_vec(),
            r#"{"name":"test","description":"a test file"}"#.to_string(),
            "2025-07-15T12:00:00Z".to_string(),
        );

        let md = render_entry_to_markdown(&entry);

        // Should start and end with frontmatter delimiters
        assert!(md.starts_with("---\n"));
        assert!(md.contains("\n---\n"));

        // User fields should be present
        assert!(md.contains("name: test"));
        assert!(md.contains("description: a test file"));

        // System fields should be present
        assert!(md.contains("entry_type: memory"));
        assert!(md.contains("content_hash:"));
        assert!(md.contains("created_at: 2025-07-15T12:00:00Z"));
        assert!(md.contains("updated_at: 2025-07-15T12:00:00Z"));

        // Content should follow frontmatter
        assert!(md.contains("# Hello\n\nWorld\n"));
    }

    #[test]
    fn test_parse_markdown_frontmatter() {
        let content =
            "---\nname: test\ntype: memory\nentry_type: memory\ncontent_hash: abc\n---\n# Hello\n\nWorld\n";
        let (fm, body) = parse_markdown_frontmatter(content);

        // entry_type and content_hash should be stripped from frontmatter JSON
        let fm_parsed: serde_json::Value = serde_json::from_str(&fm).unwrap();
        assert_eq!(fm_parsed["name"], "test");
        assert_eq!(fm_parsed["type"], "memory");
        assert!(fm_parsed.get("entry_type").is_none()); // system field stripped
        assert!(fm_parsed.get("content_hash").is_none()); // system field stripped

        assert_eq!(body, "# Hello\n\nWorld\n");
    }

    #[test]
    fn test_parse_markdown_no_frontmatter() {
        let content = "# Just content\n\nNo frontmatter here.";
        let (fm, body) = parse_markdown_frontmatter(content);
        assert_eq!(fm, "{}");
        assert_eq!(body, content);
    }

    #[test]
    fn test_infer_entry_type() {
        assert_eq!(
            infer_entry_type("goals/abc/_goal.md"),
            VaultEntryType::Session
        );
        assert_eq!(
            infer_entry_type("goals/abc/toolu_xyz.md"),
            VaultEntryType::ToolResult
        );
        assert_eq!(infer_entry_type("memory/arch.md"), VaultEntryType::Memory);
        assert_eq!(
            infer_entry_type("skills/run-tests.md"),
            VaultEntryType::Skill
        );
        assert_eq!(
            infer_entry_type("intents/fix-bug/intent.md"),
            VaultEntryType::Intent
        );
        assert_eq!(infer_entry_type("scratch/idea.md"), VaultEntryType::Scratch);
        assert_eq!(infer_entry_type("unknown/file.md"), VaultEntryType::Scratch);
        assert_eq!(
            infer_entry_type("attestations/PIMO-1/attested.md"),
            VaultEntryType::Attestation
        );
    }

    #[test]
    fn test_attestation_entry_scan_unchanged() {
        // The trailing-newline byte-identity invariant (critic #1): the stored
        // content_bytes end in exactly one '\n', so render_entry_to_markdown
        // appends NO extra newline and the first vault_scan sees the
        // materialized body's hash == the stored blake3(content) == Unchanged.
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Body = pretty JSON + a single trailing '\n' (exactly how attest builds it).
        let node_json =
            serde_json::to_string_pretty(&serde_json::json!({"@id": "atomic:intent:PIMO-1"}))
                .unwrap();
        let mut body = node_json;
        body.push('\n');
        let body_bytes = body.clone().into_bytes();

        let path = "attestations/PIMO-1/attested.md";
        let stored_hash = repo
            .vault_store(
                path,
                VaultEntryType::Attestation,
                body_bytes.clone(),
                r#"{"intentId":"PIMO-1"}"#.to_string(),
            )
            .unwrap();

        // The stored content hash is blake3 of the body-with-newline (unchanged
        // storage-hash semantics).
        assert_eq!(stored_hash, Hash::of(&body_bytes));
        let retrieved = repo.vault_retrieve(path).unwrap().unwrap();
        assert_eq!(
            Hash::from_bytes(retrieved.content_hash),
            Hash::of(&body_bytes)
        );

        // Materialize to disk, then scan: the materialized body must hash back to
        // the exact same value, so the entry is Unchanged (no change reported).
        repo.vault_materialize(path).unwrap();
        let changes = repo.vault_scan_working_copy().unwrap();
        assert!(
            !changes.iter().any(|c| c.path == path),
            "attestation entry must scan as Unchanged; got changes: {changes:?}"
        );
    }

    #[test]
    fn test_attestation_not_embedded() {
        // The guard in vault_auto_index must skip embedding for Attestation
        // entries (signed blobs must not pollute semantic search).
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        let before = repo.vault_embedding_count().unwrap();
        repo.vault_store(
            "attestations/PIMO-1/attested.md",
            VaultEntryType::Attestation,
            b"{\n  \"@id\": \"atomic:intent:PIMO-1\"\n}\n".to_vec(),
            r#"{"intentId":"PIMO-1"}"#.to_string(),
        )
        .unwrap();
        let after = repo.vault_embedding_count().unwrap();
        assert_eq!(before, after, "attestation entry must not be embedded");
    }

    #[test]
    fn test_vault_deflation_roundtrip() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Store and materialize an entry
        repo.vault_store(
            "memory/architecture.md",
            VaultEntryType::Memory,
            b"# Architecture\n\nOriginal content.\n".to_vec(),
            r#"{"name":"architecture","type":"project"}"#.to_string(),
        )
        .unwrap();
        repo.vault_materialize("memory/architecture.md").unwrap();

        // Edit the file on disk
        let disk_path = repo.vault_dir().join("memory/architecture.md");
        let original = std::fs::read_to_string(&disk_path).unwrap();
        let modified = original.replace("Original content.", "Modified content.");
        std::fs::write(&disk_path, &modified).unwrap();

        // Scan should detect the change
        let changes = repo.vault_scan_working_copy().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, "memory/architecture.md");
        assert_eq!(changes[0].change_type, VaultChangeType::Modified);

        // Record the change
        let updated = repo.vault_record_working_copy().unwrap();
        assert_eq!(updated.len(), 1);

        // Verify redb has the new content
        let entry = repo
            .vault_retrieve("memory/architecture.md")
            .unwrap()
            .unwrap();
        let content = String::from_utf8_lossy(&entry.content_bytes);
        assert!(content.contains("Modified content."));
    }

    #[test]
    fn test_vault_deflation_detects_frontmatter_only_change() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let path = "memory/status-change.md";
        repo.vault_store(
            path,
            VaultEntryType::Memory,
            b"Same body.\n".to_vec(),
            r#"{"name":"status-change","status":"active"}"#.to_string(),
        )
        .unwrap();
        repo.vault_materialize(path).unwrap();

        // Materializing without edits must round-trip as unchanged.
        assert!(repo.vault_scan_working_copy().unwrap().is_empty());

        let disk_path = repo.vault_dir().join(path);
        let original = std::fs::read_to_string(&disk_path).unwrap();
        let modified = original.replace("status: active", "status: retracted");
        std::fs::write(&disk_path, modified).unwrap();

        let changes = repo.vault_scan_working_copy().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, VaultChangeType::Modified);
        repo.vault_record_working_copy().unwrap();

        let entry = repo.vault_retrieve(path).unwrap().unwrap();
        let frontmatter: serde_json::Value = serde_json::from_str(&entry.frontmatter_json).unwrap();
        assert_eq!(frontmatter["status"], "retracted");
    }

    #[test]
    fn test_vault_frontmatter_materialization_preserves_json_types() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();
        let path = "memory/frontmatter-types.md";
        let frontmatter = serde_json::json!({
            "boolean": true,
            "boolean_string": "true",
            "number": 123,
            "number_string": "123",
            "null_string": "null",
            "array_string": "[draft]",
            "labels": ["true", "a,b", 123, false],
            "nested": {"status": "null"}
        });

        repo.vault_store(
            path,
            VaultEntryType::Memory,
            b"Same body.\n".to_vec(),
            frontmatter.to_string(),
        )
        .unwrap();
        repo.vault_materialize(path).unwrap();

        assert!(
            repo.vault_scan_working_copy().unwrap().is_empty(),
            "materializing an entry must not create a semantic frontmatter change"
        );
        let markdown = std::fs::read_to_string(repo.vault_dir().join(path)).unwrap();
        let (parsed, _) = parse_markdown_frontmatter(&markdown);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&parsed).unwrap(),
            frontmatter
        );
    }

    #[test]
    fn test_vault_deflation_new_file() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Create a new file on disk (not in redb)
        let disk_path = repo.vault_dir().join("memory/new-file.md");
        std::fs::create_dir_all(disk_path.parent().unwrap()).unwrap();
        std::fs::write(&disk_path, "---\nname: new\n---\n# New File\n").unwrap();

        let changes = repo.vault_scan_working_copy().unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change_type, VaultChangeType::New);

        let updated = repo.vault_record_working_copy().unwrap();
        assert_eq!(updated.len(), 1);

        // Should now be in redb
        let entry = repo.vault_retrieve("memory/new-file.md").unwrap().unwrap();
        assert_eq!(entry.entry_type, VaultEntryType::Memory);
    }

    #[test]
    fn test_vault_deflation_deleted_file() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Store and materialize
        repo.vault_store(
            "memory/to-delete.md",
            VaultEntryType::Memory,
            b"# Delete me\n".to_vec(),
            "{}".to_string(),
        )
        .unwrap();
        repo.vault_materialize("memory/to-delete.md").unwrap();

        // Delete from disk
        std::fs::remove_file(repo.vault_dir().join("memory/to-delete.md")).unwrap();

        let changes = repo.vault_scan_working_copy().unwrap();
        let deleted: Vec<_> = changes
            .iter()
            .filter(|c| c.change_type == VaultChangeType::Deleted)
            .collect();
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].path, "memory/to-delete.md");

        let updated = repo.vault_record_working_copy().unwrap();
        assert_eq!(updated.len(), 1);

        // Should be gone from redb
        assert!(repo
            .vault_retrieve("memory/to-delete.md")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_vault_scan_no_vault() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        // Don't init vault
        let changes = repo.vault_scan_working_copy().unwrap();
        assert!(changes.is_empty());
    }

    #[test]
    fn test_record_picks_up_vault_changes() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        repo.init_vault().unwrap();

        // Store and materialize a vault entry
        repo.vault_store(
            "memory/test.md",
            VaultEntryType::Memory,
            b"# Original\n".to_vec(),
            "{}".to_string(),
        )
        .unwrap();
        repo.vault_materialize("memory/test.md").unwrap();

        // Edit the vault file on disk
        let disk_path = repo.vault_dir().join("memory/test.md");
        let original = std::fs::read_to_string(&disk_path).unwrap();
        let modified = original.replace("# Original", "# Modified");
        std::fs::write(&disk_path, &modified).unwrap();

        // The vault scan should detect it
        let changes = repo.vault_scan_working_copy().unwrap();
        assert!(!changes.is_empty());

        // vault_record_working_copy should update redb
        let updated = repo.vault_record_working_copy().unwrap();
        assert!(!updated.is_empty());

        // Verify redb has the new content
        let entry = repo.vault_retrieve("memory/test.md").unwrap().unwrap();
        let content = String::from_utf8_lossy(&entry.content_bytes);
        assert!(content.contains("# Modified"));
    }

    #[test]
    fn test_bootstrap_vault_from_working_copy() {
        // ── Repo 1: the original author who inited the vault ────────
        let dir1 = tempdir().unwrap();
        let repo1 = Repository::init(dir1.path()).unwrap();
        repo1.init_vault().unwrap();

        // Store some custom content in repo1's vault
        repo1
            .vault_store(
                "memory/architecture.md",
                VaultEntryType::Memory,
                b"# Architecture\nWe use patch theory.".to_vec(),
                r#"{"name":"architecture"}"#.to_string(),
            )
            .unwrap();
        repo1
            .vault_store(
                "skills/rust.md",
                VaultEntryType::Skill,
                b"# Rust\nUse cargo build.".to_vec(),
                r#"{"name":"rust"}"#.to_string(),
            )
            .unwrap();

        // Materialize so we have on-disk markdown files to copy
        repo1.vault_materialize("memory/architecture.md").unwrap();
        repo1.vault_materialize("skills/rust.md").unwrap();

        // ── Repo 2: simulates a collaborator after pull/clone ──────
        // The repo is initialized but vault tables are NOT created.
        // The .vault/ directory with markdown files exists on disk
        // (as if materialized from the graph by the apply/pull path).
        let dir2 = tempdir().unwrap();
        let repo2 = Repository::init(dir2.path()).unwrap();

        // Manually create .vault/ subdirs and copy markdown files
        let vault2 = repo2.vault_dir();
        std::fs::create_dir_all(vault2.join("memory")).unwrap();
        std::fs::create_dir_all(vault2.join("skills")).unwrap();

        let src_vault = repo1.vault_dir();
        std::fs::copy(
            src_vault.join("memory/architecture.md"),
            vault2.join("memory/architecture.md"),
        )
        .unwrap();
        std::fs::copy(
            src_vault.join("skills/rust.md"),
            vault2.join("skills/rust.md"),
        )
        .unwrap();

        // Precondition: vault tables don't exist yet
        assert!(!repo2.has_vault().unwrap());

        // ── Bootstrap ──────────────────────────────────────────────
        repo2.bootstrap_vault_from_working_copy().unwrap();

        // ── Assertions ─────────────────────────────────────────────
        assert!(repo2.has_vault().unwrap());

        // Both entries should now be in repo2's redb
        let arch = repo2
            .vault_retrieve("memory/architecture.md")
            .unwrap()
            .expect("architecture entry should exist");
        let arch_content = String::from_utf8_lossy(&arch.content_bytes);
        assert!(
            arch_content.contains("# Architecture") && arch_content.contains("patch theory"),
            "architecture content mismatch: {arch_content:?}"
        );

        let rust = repo2
            .vault_retrieve("skills/rust.md")
            .unwrap()
            .expect("rust entry should exist");
        let rust_content = String::from_utf8_lossy(&rust.content_bytes);
        assert!(
            rust_content.contains("# Rust") && rust_content.contains("cargo build"),
            "rust content mismatch: {rust_content:?}"
        );

        // vault_list should return both entries
        let all = repo2.vault_list("", None).unwrap();
        assert_eq!(all.len(), 2);
    }
}

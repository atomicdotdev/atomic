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
        let subdirs = ["goals", "memory", "skills", "intents", "scratch"];
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

        // Install default vault content (skills, memory index)
        self.vault_install_defaults()?;

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
        use atomic_core::pristine::{VaultMutTxnT, VaultTxnT};

        txn.put_vault_entry(path, &entry)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        // Update manifest
        let mut manifest = txn
            .get_vault_manifest()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        update_manifest_for_store(&mut manifest, path, &entry, &now);
        txn.put_vault_manifest(&manifest)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        self.vault_auto_index(path);

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

        let mut txn = self
            .pristine
            .write_txn()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        use atomic_core::pristine::{VaultMutTxnT, VaultTxnT};

        txn.put_vault_entry(path, entry)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        let now = chrono::Utc::now().to_rfc3339();
        let mut manifest = txn
            .get_vault_manifest()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
        update_manifest_for_store(&mut manifest, path, entry, &now);
        txn.put_vault_manifest(&manifest)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        self.vault_auto_index(path);

        Ok(content_hash)
    }

    /// Automatically index a vault entry into the KG and embed it.
    ///
    /// Called as a side effect of `vault_store` and `vault_store_entry`.
    /// Best-effort — logs on failure but never fails the parent operation.
    fn vault_auto_index(&self, path: &str) {
        // KG indexing
        if let Err(e) = self.vault_index_kg(path) {
            log::debug!("Auto KG index for {}: {}", path, e);
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
        use atomic_core::pristine::{VaultMutTxnT, VaultTxnT};

        let existed = txn
            .del_vault_entry(path)
            .map_err(|e| RepositoryError::Database(e.to_string()))?;

        if existed {
            let now = chrono::Utc::now().to_rfc3339();
            let mut manifest = txn
                .get_vault_manifest()
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
            update_manifest_for_delete(&mut manifest, path, &now);
            txn.put_vault_manifest(&manifest)
                .map_err(|e| RepositoryError::Database(e.to_string()))?;
        }

        txn.commit()
            .map_err(|e| RepositoryError::Database(e.to_string()))?;
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
    /// - Their content hash differs from the stored entry
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

            // Compute hash of the body (not frontmatter)
            let body_bytes = body.as_bytes();
            let new_hash = Hash::of(body_bytes);

            // Check if this is new or changed
            let existing = self.vault_retrieve(&rel_path_str)?;
            let change_type = match &existing {
                Some(entry) if Hash::from_bytes(entry.content_hash) == new_hash => {
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

        if changes.is_empty() {
            return Ok(Vec::new());
        }

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

        Ok(updated_paths)
    }
}

// ── Manifest helpers ────────────────────────────────────────────────────

/// Update the manifest when a vault entry is stored.
fn update_manifest_for_store(
    manifest: &mut VaultManifest,
    path: &str,
    entry: &VaultEntry,
    now: &str,
) {
    let hash_str = Hash::from_bytes(entry.content_hash).to_base32();
    let bytes = entry.content_size() as u64;

    // Update type-specific index
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

    // Recompute totals by iterating all entries would be expensive,
    // so we do incremental updates.
    manifest.file_count = manifest.file_count.saturating_add(1);
    manifest.total_bytes = manifest.total_bytes.saturating_add(bytes);

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
fn update_manifest_for_delete(manifest: &mut VaultManifest, path: &str, now: &str) {
    // Remove from type-specific indexes
    manifest.memory.remove(path);
    manifest.skills.remove(path);
    manifest.goals.remove(path);
    manifest.intents.remove(path);

    // Decrement file count (floor at 0)
    manifest.file_count = manifest.file_count.saturating_sub(1);

    // We can't easily adjust total_bytes without knowing the old entry's size,
    // so we leave it as-is. Use vault_recompute_manifest_totals() for accuracy.
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
            // Use bare value for simple strings, quoted for strings with special chars.
            // Note: bare colons are fine in YAML values (e.g. timestamps "12:00:00");
            // only ": " (colon-space) is ambiguous as a nested key-value separator.
            if s.contains(": ")
                || s.contains('#')
                || s.contains('\n')
                || s.starts_with('{')
                || s.starts_with('[')
            {
                let _ = writeln!(output, "{}: \"{}\"", key, s.replace('\"', "\\\""));
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
            let items: Vec<String> = arr
                .iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            let _ = writeln!(output, "{}: [{}]", key, items.join(", "));
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
        if key == "entry_type" || key == "content_hash" {
            continue;
        }

        let value = parse_yaml_value(value_str);
        map.insert(key, value);
    }

    serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
}

/// Parse a YAML value string into a serde_json::Value.
fn parse_yaml_value(s: &str) -> serde_json::Value {
    // Remove surrounding quotes if present
    let s =
        if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
            &s[1..s.len() - 1]
        } else {
            s
        };

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

    // Try as array [a, b, c]
    if s.starts_with('[') && s.ends_with(']') {
        let inner = &s[1..s.len() - 1];
        let items: Vec<serde_json::Value> = inner
            .split(',')
            .map(|item| parse_yaml_value(item.trim()))
            .collect();
        return serde_json::Value::Array(items);
    }

    // Try as inline JSON object
    if s.starts_with('{') && s.ends_with('}') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
            return v;
        }
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
    } else if path.starts_with("scratch/") {
        VaultEntryType::Scratch
    } else {
        VaultEntryType::Scratch // default for unknown paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::pristine::VaultEntryType;
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

        // List all (3 defaults from init_vault + 2 test entries)
        let all = repo.vault_list("", None).unwrap();
        assert_eq!(all.len(), 5);

        // Filter by type
        let sessions = repo.vault_list("", Some(VaultEntryType::Session)).unwrap();
        assert_eq!(sessions.len(), 1);

        // Delete
        assert!(repo.vault_delete("memory/arch.md").unwrap());
        assert!(!repo.vault_delete("memory/arch.md").unwrap());

        let all = repo.vault_list("", None).unwrap();
        assert_eq!(all.len(), 4);
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
        // 5 test entries + 3 defaults (2 skills + memory index)
        assert_eq!(manifest.file_count, 8);
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

        // 2 test entries + 3 defaults from init_vault
        let count = repo.vault_materialize_all().unwrap();
        assert_eq!(count, 5);

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
}

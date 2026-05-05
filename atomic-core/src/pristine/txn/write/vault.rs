//! Vault trait implementations for WriteTxn.
//!
//! Implements both `VaultTxnT` (read) and `VaultMutTxnT` (write) for `WriteTxn`.

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::tables::{VAULT_ENTRIES, VAULT_MANIFEST};
use crate::pristine::traits::{VaultEntryMeta, VaultMutTxnT, VaultTxnT};
use crate::pristine::{VaultEntry, VaultEntryType, VaultManifest};

use redb::ReadableTable;

use super::WriteTxn;

impl<'a> VaultTxnT for WriteTxn<'a> {
    fn get_vault_entry(&self, path: &str) -> PristineResult<Option<VaultEntry>> {
        let table = match self.txn.open_table(VAULT_ENTRIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(PristineError::from(e)),
        };

        let result = match table.get(path)? {
            Some(guard) => {
                let bytes = guard.value();
                let entry: VaultEntry =
                    postcard::from_bytes(bytes).map_err(|e| PristineError::Serialization {
                        message: format!("failed to deserialize VaultEntry at '{}': {}", path, e),
                    })?;
                Ok(Some(entry))
            }
            None => Ok(None),
        };
        result
    }

    fn list_vault_entries(
        &self,
        prefix: &str,
        entry_type_filter: Option<VaultEntryType>,
    ) -> PristineResult<Vec<VaultEntryMeta>> {
        let table = match self.txn.open_table(VAULT_ENTRIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let mut results = Vec::new();

        let iter = if prefix.is_empty() {
            table.iter()?
        } else {
            table.range(prefix..)?
        };

        for item in iter {
            let (key, value) = item?;
            let key_str = key.value();

            // Stop iterating once we pass the prefix range
            if !prefix.is_empty() && !key_str.starts_with(prefix) {
                break;
            }

            let entry: VaultEntry =
                postcard::from_bytes(value.value()).map_err(|e| PristineError::Serialization {
                    message: format!("failed to deserialize VaultEntry at '{}': {}", key_str, e),
                })?;

            // Apply type filter
            if let Some(ref filter) = entry_type_filter {
                if entry.entry_type != *filter {
                    continue;
                }
            }

            results.push(VaultEntryMeta {
                path: key_str.to_string(),
                entry_type: entry.entry_type,
                content_hash: entry.content_hash,
                content_size: entry.content_bytes.len(),
                updated_at: entry.updated_at,
            });
        }

        Ok(results)
    }

    fn get_vault_manifest(&self) -> PristineResult<VaultManifest> {
        let table = match self.txn.open_table(VAULT_MANIFEST) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(VaultManifest::default()),
            Err(e) => return Err(PristineError::from(e)),
        };

        let result = match table.get("manifest")? {
            Some(guard) => {
                let bytes = guard.value();
                let manifest: VaultManifest =
                    serde_json::from_slice(bytes).map_err(|e| PristineError::Serialization {
                        message: format!("failed to deserialize VaultManifest: {}", e),
                    })?;
                Ok(manifest)
            }
            None => Ok(VaultManifest::default()),
        };
        result
    }

    fn has_vault(&self) -> PristineResult<bool> {
        let table = match self.txn.open_table(VAULT_MANIFEST) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(e) => return Err(PristineError::from(e)),
        };

        let result = match table.get("manifest")? {
            Some(_) => Ok(true),
            None => Ok(false),
        };
        result
    }
}

impl<'a> VaultMutTxnT for WriteTxn<'a> {
    fn put_vault_entry(&mut self, path: &str, entry: &VaultEntry) -> PristineResult<()> {
        let bytes = postcard::to_allocvec(entry).map_err(|e| PristineError::Serialization {
            message: format!("failed to serialize VaultEntry for '{}': {}", path, e),
        })?;

        let mut table = self.txn.open_table(VAULT_ENTRIES)?;
        table.insert(path, bytes.as_slice())?;
        Ok(())
    }

    fn del_vault_entry(&mut self, path: &str) -> PristineResult<bool> {
        let mut table = self.txn.open_table(VAULT_ENTRIES)?;
        let existed = table.remove(path)?.is_some();
        Ok(existed)
    }

    fn put_vault_manifest(&mut self, manifest: &VaultManifest) -> PristineResult<()> {
        let bytes = serde_json::to_vec(manifest).map_err(|e| PristineError::Serialization {
            message: format!("failed to serialize VaultManifest: {}", e),
        })?;

        let mut table = self.txn.open_table(VAULT_MANIFEST)?;
        table.insert("manifest", bytes.as_slice())?;
        Ok(())
    }

    fn init_vault(&mut self) -> PristineResult<()> {
        // Opening tables with a WriteTransaction creates them if they don't exist
        let _ = self.txn.open_table(VAULT_ENTRIES)?;
        let _ = self.txn.open_table(VAULT_MANIFEST)?;

        // Store a default manifest if none exists yet
        let table = self.txn.open_table(VAULT_MANIFEST)?;
        let needs_default = table.get("manifest")?.is_none();
        drop(table);

        if needs_default {
            self.put_vault_manifest(&VaultManifest::default())?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::traits::MutTxnT;
    use crate::pristine::Pristine;
    use tempfile::tempdir;

    #[test]
    fn test_vault_init_and_has_vault() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();

        // Before init, has_vault should be false
        let txn = pristine.read_txn().unwrap();
        assert!(!txn.has_vault().unwrap());
        drop(txn);

        // Init vault
        let mut txn = pristine.write_txn().unwrap();
        txn.init_vault().unwrap();
        txn.commit().unwrap();

        // After init, has_vault should be true
        let txn = pristine.read_txn().unwrap();
        assert!(txn.has_vault().unwrap());
    }

    #[test]
    fn test_vault_entry_crud() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        txn.init_vault().unwrap();

        let entry = VaultEntry::new(
            VaultEntryType::Memory,
            b"# Architecture\nWe use crates.".to_vec(),
            r#"{"name":"architecture"}"#.to_string(),
            "2025-07-15T12:00:00Z".to_string(),
        );

        // Store
        txn.put_vault_entry("memory/architecture.md", &entry)
            .unwrap();

        // Retrieve
        let retrieved = txn
            .get_vault_entry("memory/architecture.md")
            .unwrap()
            .unwrap();
        assert_eq!(retrieved.entry_type, VaultEntryType::Memory);
        assert_eq!(retrieved.content_bytes, entry.content_bytes);
        assert_eq!(retrieved.content_hash, entry.content_hash);

        // Not found
        assert!(txn.get_vault_entry("nonexistent").unwrap().is_none());

        // Delete
        assert!(txn.del_vault_entry("memory/architecture.md").unwrap());
        assert!(!txn.del_vault_entry("memory/architecture.md").unwrap());
        assert!(txn
            .get_vault_entry("memory/architecture.md")
            .unwrap()
            .is_none());

        txn.commit().unwrap();
    }

    #[test]
    fn test_vault_list_entries() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        txn.init_vault().unwrap();

        // Store multiple entries
        let mem1 = VaultEntry::new(
            VaultEntryType::Memory,
            b"content1".to_vec(),
            "{}".to_string(),
            "2025-07-15T12:00:00Z".to_string(),
        );
        let mem2 = VaultEntry::new(
            VaultEntryType::Memory,
            b"content2".to_vec(),
            "{}".to_string(),
            "2025-07-15T12:00:00Z".to_string(),
        );
        let session = VaultEntry::new(
            VaultEntryType::Session,
            b"session data".to_vec(),
            "{}".to_string(),
            "2025-07-15T12:00:00Z".to_string(),
        );

        txn.put_vault_entry("memory/arch.md", &mem1).unwrap();
        txn.put_vault_entry("memory/conv.md", &mem2).unwrap();
        txn.put_vault_entry("sessions/abc/_session.md", &session)
            .unwrap();

        // List all
        let all = txn.list_vault_entries("", None).unwrap();
        assert_eq!(all.len(), 3);

        // List by prefix
        let mem_entries = txn.list_vault_entries("memory/", None).unwrap();
        assert_eq!(mem_entries.len(), 2);

        // List by type
        let sessions = txn
            .list_vault_entries("", Some(VaultEntryType::Session))
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].path, "sessions/abc/_session.md");

        // List by prefix + type
        let mem_only = txn
            .list_vault_entries("memory/", Some(VaultEntryType::Memory))
            .unwrap();
        assert_eq!(mem_only.len(), 2);

        txn.commit().unwrap();
    }

    #[test]
    fn test_vault_manifest_crud() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        txn.init_vault().unwrap();

        // Default manifest after init
        let manifest = txn.get_vault_manifest().unwrap();
        assert_eq!(manifest.version, 1);
        assert!(manifest.goals.is_empty());

        // Update manifest
        let mut new_manifest = VaultManifest::new("2025-07-15T12:00:00Z".to_string());
        new_manifest.file_count = 42;
        new_manifest.total_bytes = 100_000;
        txn.put_vault_manifest(&new_manifest).unwrap();

        // Retrieve updated manifest
        let retrieved = txn.get_vault_manifest().unwrap();
        assert_eq!(retrieved.file_count, 42);
        assert_eq!(retrieved.total_bytes, 100_000);

        txn.commit().unwrap();

        // Verify persistence via read txn
        let txn = pristine.read_txn().unwrap();
        let manifest = txn.get_vault_manifest().unwrap();
        assert_eq!(manifest.file_count, 42);
    }

    #[test]
    fn test_vault_100_entries_iterate() {
        let dir = tempdir().unwrap();
        let pristine = Pristine::open(dir.path().join("db")).unwrap();

        let mut txn = pristine.write_txn().unwrap();
        txn.init_vault().unwrap();

        // Store 100 entries
        for i in 0..100 {
            let entry = VaultEntry::new(
                if i % 3 == 0 {
                    VaultEntryType::Session
                } else if i % 3 == 1 {
                    VaultEntryType::Memory
                } else {
                    VaultEntryType::Skill
                },
                format!("content {}", i).into_bytes(),
                "{}".to_string(),
                format!("2025-07-15T12:{:02}:00Z", i % 60),
            );
            txn.put_vault_entry(&format!("entry/{:03}.md", i), &entry)
                .unwrap();
        }

        // List all
        let all = txn.list_vault_entries("", None).unwrap();
        assert_eq!(all.len(), 100);

        // Filter by type
        let sessions = txn
            .list_vault_entries("", Some(VaultEntryType::Session))
            .unwrap();
        assert_eq!(sessions.len(), 34); // 0, 3, 6, ..., 99 → 34 entries

        let memories = txn
            .list_vault_entries("", Some(VaultEntryType::Memory))
            .unwrap();
        assert_eq!(memories.len(), 33); // 1, 4, 7, ..., 97 → 33 entries

        txn.commit().unwrap();
    }
}

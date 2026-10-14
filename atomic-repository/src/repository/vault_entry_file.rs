//! Complete entry files for explicit Vault reconstruction, separate from the
//! editable Markdown projection and from the RPC entry/attestation bundle.

use atomic_core::pristine::{GraphTxnT, MutTxnT, VaultEntry, VaultMutTxnT, VaultTxnT};
use atomic_core::types::{Hash, NodeId};
use serde::{Deserialize, Serialize};

use super::{Repository, RepositoryError};

const SCHEMA: &str = "atomic.vault.entry.file.v1";

/// A complete entry revision with a portable introducing-change reference.
///
/// `entry()` retains the source's exact stored fields, including its local
/// NodeId. The origin names that ID's namespace; `introduced_by_hash()` is the
/// reference used to restore it in a different repository. The source ID must
/// never be used directly in the receiver's graph.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultEntryFile {
    schema: String,
    origin: String,
    path: String,
    entry: VaultEntry,
    introduced_by_hash: Option<Hash>,
}

impl VaultEntryFile {
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn entry(&self) -> &VaultEntry {
        &self.entry
    }
    pub fn introduced_by_hash(&self) -> Option<Hash> {
        self.introduced_by_hash
    }

    /// Encode all fields without parsing frontmatter or interpreting body bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, RepositoryError> {
        self.validate()?;
        Ok(serde_json::to_vec(self)?)
    }

    /// Decode a V1 file, refusing unsupported or noncanonical fields/encoding.
    /// The strict round trip also catches unknown fields inside `VaultEntry`
    /// rather than silently dropping them through its general serde decoder.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, RepositoryError> {
        let file: Self = serde_json::from_slice(bytes)?;
        if file.to_bytes()? != bytes {
            return Err(invalid(
                "unsupported fields or noncanonical Vault entry file",
            ));
        }
        Ok(file)
    }

    /// Hash the complete file, including metadata and source reference mapping.
    /// This differs from `entry.content_hash`, which covers only the body.
    pub fn revision(&self) -> Result<Hash, RepositoryError> {
        let mut hasher = blake3::Hasher::new_derive_key(SCHEMA);
        hasher.update(&self.to_bytes()?);
        Ok(Hash::from_bytes(*hasher.finalize().as_bytes()))
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.schema != SCHEMA {
            return Err(invalid("unsupported Vault entry file schema"));
        }
        if self.origin.trim().is_empty() {
            return Err(invalid("Vault entry file needs a source namespace"));
        }
        if self.path.is_empty()
            || self.path.contains(['\\', '\0', ':'])
            || self
                .path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(invalid("Vault entry file needs a vault-relative path"));
        }
        if self.entry.content_hash != *blake3::hash(&self.entry.content_bytes).as_bytes() {
            return Err(invalid("Vault entry body hash mismatch"));
        }
        if (self.entry.introduced_by == 0) != self.introduced_by_hash.is_none() {
            return Err(invalid("inconsistent Vault introducing-change reference"));
        }
        Ok(())
    }
}

fn invalid(message: &str) -> RepositoryError {
    RepositoryError::Serialization(message.into())
}

impl Repository {
    /// Export one complete entry from a consistent database snapshot.
    ///
    /// `origin` is the caller's stable source namespace for repository-local
    /// IDs (for example the replication origin ID). It is preserved verbatim.
    /// The result can be written directly to a file using `to_bytes()`.
    pub fn vault_export_entry_file(
        &self,
        path: &str,
        origin: &str,
    ) -> Result<VaultEntryFile, RepositoryError> {
        let txn = self.pristine.read_txn()?;
        let entry = txn
            .get_vault_entry(path)?
            .ok_or_else(|| invalid("Vault entry to export was not found"))?;
        let introduced_by_hash = if entry.introduced_by == 0 {
            None
        } else {
            Some(
                txn.get_external(NodeId::new(entry.introduced_by))?
                    .ok_or_else(|| invalid("Vault introducing change has no canonical hash"))?,
            )
        };
        let file = VaultEntryFile {
            schema: SCHEMA.into(),
            origin: origin.into(),
            path: path.into(),
            entry,
            introduced_by_hash,
        };
        file.validate()?;
        Ok(file)
    }

    /// Restore one complete entry file, retaining its original bytes and mapping.
    ///
    /// Import its introducing change first. Restoration maps that hash to the
    /// receiver's local NodeId and commits the entry, manifest and original file
    /// together. Repeating an identical restore is safe. A different existing
    /// entry is refused: this is a reconstruction API, not conflict resolution
    /// or an implicit overwrite of local work.
    pub fn vault_restore_entry_file(&self, bytes: &[u8]) -> Result<Hash, RepositoryError> {
        let file = VaultEntryFile::from_bytes(bytes)?;
        let revision = file.revision()?;
        let mut txn = self.pristine.write_txn()?;
        let mut entry = file.entry.clone();
        if let Some(hash) = file.introduced_by_hash {
            entry.introduced_by = txn
                .get_internal(&hash)?
                .ok_or_else(|| {
                    invalid("import the Vault introducing change before restoring its entry")
                })?
                .get();
        }
        let previous = txn.get_vault_entry(&file.path)?;
        if let Some(previous) = &previous {
            if serde_json::to_vec(previous)? != serde_json::to_vec(&entry)? {
                return Err(invalid(
                    "Vault entry differs from local state; restore into a fresh repository",
                ));
            }
        } else {
            txn.init_vault()?;
            txn.put_vault_entry(&file.path, &entry)?;
            let mut manifest = txn.get_vault_manifest()?;
            super::vault::update_manifest_for_store(
                &mut manifest,
                &file.path,
                &entry,
                None,
                &entry.updated_at,
            );
            txn.put_vault_manifest(&manifest)?;
        }
        txn.put_vault_entry_revision(&revision, bytes)?;
        txn.commit()?;
        self.vault_auto_index(&file.path);
        Ok(revision)
    }

    /// Retrieve a retained original entry file, including its source-local ID.
    /// Retained revisions survive current entry updates and deletion.
    pub fn vault_retained_entry_file(
        &self,
        revision: &Hash,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let bytes = self
            .pristine
            .read_txn()?
            .get_vault_entry_revision(revision)?;
        if let Some(bytes) = &bytes {
            if VaultEntryFile::from_bytes(bytes)?.revision()? != *revision {
                return Err(invalid("retained Vault entry revision hash mismatch"));
            }
        }
        Ok(bytes)
    }
}

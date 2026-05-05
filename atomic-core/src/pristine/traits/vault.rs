//! Vault storage trait for reading and writing vault entries.
//!
//! `VaultTxnT` provides CRUD operations on the vault knowledge store.
//! It is implemented independently by both `ReadTxn` (read-only subset)
//! and `WriteTxn` (full read-write access).

use crate::pristine::error::PristineError;
use crate::pristine::vault::{VaultEntry, VaultEntryType, VaultManifest};

/// Metadata for a vault entry listing (without the full content bytes).
#[derive(Clone, Debug)]
pub struct VaultEntryMeta {
    /// Vault-relative path (e.g. "sessions/abc123/_session.md").
    pub path: String,
    /// Entry type.
    pub entry_type: VaultEntryType,
    /// Blake3 hash of content.
    pub content_hash: [u8; 32],
    /// Size of content in bytes.
    pub content_size: usize,
    /// Last updated timestamp (RFC 3339).
    pub updated_at: String,
}

/// Read-only vault operations.
///
/// Both `ReadTxn` and `WriteTxn` implement this trait. For write operations,
/// use `VaultMutTxnT`.
pub trait VaultTxnT {
    /// Retrieve a vault entry by its path.
    ///
    /// Returns `None` if no entry exists at this path.
    fn get_vault_entry(&self, path: &str) -> Result<Option<VaultEntry>, PristineError>;

    /// List vault entries matching a path prefix.
    ///
    /// If `prefix` is empty, lists all entries.
    /// If `entry_type_filter` is `Some`, only returns entries of that type.
    fn list_vault_entries(
        &self,
        prefix: &str,
        entry_type_filter: Option<VaultEntryType>,
    ) -> Result<Vec<VaultEntryMeta>, PristineError>;

    /// Get the vault manifest.
    ///
    /// Returns the default (empty) manifest if none has been stored yet.
    fn get_vault_manifest(&self) -> Result<VaultManifest, PristineError>;

    /// Check whether the vault tables have been initialized.
    ///
    /// Returns `true` if at least one vault entry or a manifest exists.
    fn has_vault(&self) -> Result<bool, PristineError>;
}

/// Write operations on the vault.
///
/// Only `WriteTxn` implements this. Extends `VaultTxnT` for read access.
pub trait VaultMutTxnT: VaultTxnT {
    /// Store a vault entry at the given path.
    ///
    /// If an entry already exists at this path, it is replaced.
    /// The content hash is verified against the entry's content_bytes.
    fn put_vault_entry(&mut self, path: &str, entry: &VaultEntry) -> Result<(), PristineError>;

    /// Delete a vault entry by path.
    ///
    /// Returns `true` if the entry existed and was deleted, `false` if not found.
    fn del_vault_entry(&mut self, path: &str) -> Result<bool, PristineError>;

    /// Store the vault manifest.
    fn put_vault_manifest(&mut self, manifest: &VaultManifest) -> Result<(), PristineError>;

    /// Initialize the vault tables.
    ///
    /// Creates the vault tables in the database if they don't exist.
    /// This is idempotent — calling it multiple times is safe.
    /// Also stores a default empty manifest if none exists.
    fn init_vault(&mut self) -> Result<(), PristineError>;
}

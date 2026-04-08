//! Attestation storage methods for the change store.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use atomic_core::types::{Base32, Hash};

use super::{ChangeStore, ChangeStoreError, ChangeStoreResult, HASH_PREFIX_LEN};

impl ChangeStore {
    // =========================================================================
    // Attestation Storage
    // =========================================================================

    /// Get the filesystem path for an attestation with the given hash.
    ///
    /// Attestations use the same two-level directory structure as changes
    /// but with the `.attest` extension.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let hash = Hash::of(b"test");
    /// let path = store.attest_path(&hash);
    /// // e.g., ".atomic/changes/AB/ABCDEF1234567890....attest"
    /// ```
    pub fn attest_path(&self, hash: &Hash) -> PathBuf {
        let hash_str = hash.to_base32();
        let (prefix, _) = hash_str.split_at(HASH_PREFIX_LEN.min(hash_str.len()));
        self.changes_dir.join(prefix).join(format!(
            "{}.{}",
            hash_str,
            atomic_core::change::ATTESTATION_EXTENSION
        ))
    }

    /// Check if an attestation with the given hash exists on disk.
    pub fn has_attestation(&self, hash: &Hash) -> bool {
        self.attest_path(hash).exists()
    }

    /// Save an attestation to disk.
    ///
    /// Serializes the attestation, computes its hash, and writes it to
    /// the two-level directory structure with an `.attest` extension.
    ///
    /// # Returns
    ///
    /// The content hash of the attestation.
    pub fn save_attestation(
        &self,
        attestation: &atomic_core::change::Attestation,
    ) -> ChangeStoreResult<Hash> {
        let data = attestation.serialize().map_err(|e| {
            ChangeStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to serialize attestation: {}", e),
            ))
        })?;

        let hash = Hash::of(&data);
        let path = self.attest_path(&hash);

        // Create parent directory
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Don't overwrite if already exists (content-addressed = idempotent)
        if path.exists() {
            return Ok(hash);
        }

        // Atomic write via temp file
        let temp_file = tempfile::NamedTempFile::new_in(&self.changes_dir)?;
        temp_file.as_file().write_all(&data)?;
        temp_file.persist(&path).map_err(|e| {
            ChangeStoreError::Io(std::io::Error::other(format!(
                "Failed to persist attestation: {}",
                e
            )))
        })?;

        log::debug!(
            "Saved attestation {} ({} bytes) to {}",
            hash.to_base32(),
            data.len(),
            path.display()
        );

        Ok(hash)
    }

    /// Load an attestation from disk by hash.
    ///
    /// # Returns
    ///
    /// The deserialized `Attestation`.
    ///
    /// # Errors
    ///
    /// Returns `ChangeStoreError::NotFound` if the file doesn't exist.
    pub fn load_attestation(
        &self,
        hash: &Hash,
    ) -> ChangeStoreResult<atomic_core::change::Attestation> {
        let path = self.attest_path(hash);

        if !path.exists() {
            return Err(ChangeStoreError::NotFound {
                hash: hash.to_base32(),
            });
        }

        let data = fs::read(&path)?;
        let (attestation, computed_hash) = atomic_core::change::Attestation::deserialize(&data)
            .map_err(|e| {
                ChangeStoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Failed to deserialize attestation: {}", e),
                ))
            })?;

        // Verify hash integrity
        if computed_hash != *hash {
            return Err(ChangeStoreError::HashMismatch {
                expected: hash.to_base32(),
                computed: computed_hash.to_base32(),
            });
        }

        Ok(attestation)
    }

    /// Iterate over all attestation hashes on disk.
    pub fn iter_attestations(&self) -> impl Iterator<Item = ChangeStoreResult<Hash>> + '_ {
        super::iterators::AttestationIterator::new(&self.changes_dir)
    }

    /// Count attestations on disk.
    pub fn count_attestations(&self) -> ChangeStoreResult<usize> {
        let mut count = 0;
        for result in self.iter_attestations() {
            result?;
            count += 1;
        }
        Ok(count)
    }
}

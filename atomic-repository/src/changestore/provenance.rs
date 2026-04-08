//! Provenance graph storage methods for the change store.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use atomic_core::types::{Base32, Hash};

use super::{ChangeStore, ChangeStoreError, ChangeStoreResult, HASH_PREFIX_LEN};

impl ChangeStore {
    // =========================================================================
    // Provenance Graph Storage
    // =========================================================================

    /// Get the filesystem path for a provenance graph with the given hash.
    ///
    /// Provenance graphs use the same two-level directory structure as changes
    /// and attestations but with the `.provenance` extension.
    pub fn provenance_path(&self, hash: &Hash) -> PathBuf {
        let hash_str = hash.to_base32();
        let (prefix, _) = hash_str.split_at(HASH_PREFIX_LEN.min(hash_str.len()));
        self.changes_dir.join(prefix).join(format!(
            "{}.{}",
            hash_str,
            atomic_core::change::PROVENANCE_GRAPH_EXTENSION
        ))
    }

    /// Check if a provenance graph with the given hash exists on disk.
    pub fn has_provenance_graph(&self, hash: &Hash) -> bool {
        self.provenance_path(hash).exists()
    }

    /// Save a provenance graph to disk.
    ///
    /// Serializes the graph, computes its hash, and writes it to the
    /// two-level directory structure with a `.provenance` extension.
    ///
    /// # Returns
    ///
    /// The content hash of the provenance graph.
    pub fn save_provenance_graph(
        &self,
        graph: &atomic_core::change::ProvenanceGraph,
    ) -> ChangeStoreResult<Hash> {
        let data = graph.serialize().map_err(|e| {
            ChangeStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Failed to serialize provenance graph: {}", e),
            ))
        })?;

        let hash = Hash::of(&data);
        let path = self.provenance_path(&hash);

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
                "Failed to persist provenance graph: {}",
                e
            )))
        })?;

        log::debug!(
            "Saved provenance graph {} ({} bytes, {} nodes) to {}",
            hash.to_base32(),
            data.len(),
            graph.node_count(),
            path.display()
        );

        Ok(hash)
    }

    /// Load a provenance graph from disk by hash.
    ///
    /// # Errors
    ///
    /// Returns `ChangeStoreError::NotFound` if the file doesn't exist.
    pub fn load_provenance_graph(
        &self,
        hash: &Hash,
    ) -> ChangeStoreResult<atomic_core::change::ProvenanceGraph> {
        let path = self.provenance_path(hash);

        if !path.exists() {
            return Err(ChangeStoreError::NotFound {
                hash: hash.to_base32(),
            });
        }

        let data = fs::read(&path)?;
        let (graph, computed_hash) = atomic_core::change::ProvenanceGraph::deserialize(&data)
            .map_err(|e| {
                ChangeStoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Failed to deserialize provenance graph: {}", e),
                ))
            })?;

        // Verify hash integrity
        if computed_hash != *hash {
            return Err(ChangeStoreError::HashMismatch {
                expected: hash.to_base32(),
                computed: computed_hash.to_base32(),
            });
        }

        Ok(graph)
    }

    /// Iterate over all provenance graph hashes on disk.
    pub fn iter_provenance_graphs(&self) -> impl Iterator<Item = ChangeStoreResult<Hash>> + '_ {
        super::iterators::ProvenanceIterator::new(&self.changes_dir)
    }

    /// Count provenance graphs on disk.
    pub fn count_provenance_graphs(&self) -> ChangeStoreResult<usize> {
        let mut count = 0;
        for result in self.iter_provenance_graphs() {
            result?;
            count += 1;
        }
        Ok(count)
    }
}

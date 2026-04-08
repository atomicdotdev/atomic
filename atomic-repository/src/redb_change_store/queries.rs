//! Query, statistics, and export operations for the redb change store.

use atomic_core::change::format_v3::{
    ChangeWriter, FileHeader, HashDedupTable, SectionType, WriterOptions,
};
use atomic_core::change::Change;
use atomic_core::pristine::tables;
use std::fmt;
use std::io::Cursor;
use std::path::Path;

use super::{
    count_table_entries, extract_path_from_payload, RedbChangeStore, RedbStoreError,
    RedbStoreResult,
};

use super::batch::StoredSection;

// ═══════════════════════════════════════════════════════════════════════
// StoredContentChunk — a content chunk from CONTENT_CHUNKS
// ═══════════════════════════════════════════════════════════════════════

/// A content chunk loaded from the CONTENT_CHUNKS table.
#[derive(Clone, Debug)]
pub struct StoredContentChunk {
    /// Sequential index within the change.
    pub index: u32,

    /// Blake3 hash of the uncompressed content.
    pub hash: [u8; 32],

    /// The decompressed content bytes.
    pub data: Vec<u8>,
}

// ═══════════════════════════════════════════════════════════════════════
// StoreStats — statistics about the store
// ═══════════════════════════════════════════════════════════════════════

/// Statistics about the redb change store.
///
/// Useful for monitoring, debugging, and capacity planning.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Number of changes stored.
    pub change_count: u64,

    /// Number of graph section entries.
    pub graph_section_count: u64,

    /// Number of semantic section entries.
    pub semantic_section_count: u64,

    /// Number of unique content chunks (deduplicated).
    pub content_chunk_count: u64,

    /// Number of change-to-chunk mappings (not deduplicated).
    pub change_chunk_mappings: u64,

    /// Number of unhashed entries.
    pub unhashed_count: u64,
}

impl fmt::Display for StoreStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} changes, {} graph sections, {} semantic sections, {} unique chunks ({} mappings), {} unhashed",
            self.change_count,
            self.graph_section_count,
            self.semantic_section_count,
            self.content_chunk_count,
            self.change_chunk_mappings,
            self.unhashed_count,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Query methods on RedbChangeStore
// ═══════════════════════════════════════════════════════════════════════

impl RedbChangeStore {
    /// Load all graph sections for a change.
    ///
    /// Reads from CHANGE_GRAPH only. This is the "thin apply" path —
    /// the minimum data needed to apply a change to the repository DAG.
    ///
    /// # Returns
    ///
    /// A vector of decompressed graph section payloads, ordered by file index.
    pub fn load_graph_sections(&self, hash: &[u8; 32]) -> RedbStoreResult<Vec<StoredSection>> {
        let meta = self.load_meta(hash)?;
        let txn = self.db().begin_read()?;
        let table = txn.open_table(tables::CHANGE_GRAPH)?;

        let mut sections = Vec::with_capacity(meta.graph_section_count as usize);

        for idx in 0..meta.graph_section_count {
            let key = tables::encode_change_file_key(hash, idx);
            if let Some(value) = table.get(&key)? {
                let compressed = value.value();
                let decompressed = zstd::decode_all(compressed).map_err(|e| {
                    RedbStoreError::Corrupt(format!(
                        "graph section {} decompression failed: {}",
                        idx, e
                    ))
                })?;

                // Extract path from the payload (it's a postcard-encoded GraphSectionPayload)
                let path = extract_path_from_payload(&decompressed);

                sections.push(StoredSection {
                    section_type: SectionType::Graph,
                    payload: decompressed,
                    path,
                    file_index: idx,
                });
            }
        }

        Ok(sections)
    }

    /// Load all semantic sections for a change.
    ///
    /// Reads from CHANGE_SEMANTIC only. This is the "code review" path —
    /// the data needed for diffs, blame, and code review UI without
    /// loading graph operations.
    ///
    /// # Returns
    ///
    /// A vector of decompressed semantic section payloads, ordered by file index.
    pub fn load_semantic_sections(&self, hash: &[u8; 32]) -> RedbStoreResult<Vec<StoredSection>> {
        let meta = self.load_meta(hash)?;
        let txn = self.db().begin_read()?;
        let table = txn.open_table(tables::CHANGE_SEMANTIC)?;

        let mut sections = Vec::with_capacity(meta.semantic_section_count as usize);

        for idx in 0..meta.semantic_section_count {
            let key = tables::encode_change_file_key(hash, idx);
            if let Some(value) = table.get(&key)? {
                let compressed = value.value();
                let decompressed = zstd::decode_all(compressed).map_err(|e| {
                    RedbStoreError::Corrupt(format!(
                        "semantic section {} decompression failed: {}",
                        idx, e
                    ))
                })?;

                sections.push(StoredSection {
                    section_type: SectionType::Semantic,
                    payload: decompressed,
                    path: String::new(), // semantic sections don't embed path in the same way
                    file_index: idx,
                });
            }
        }

        Ok(sections)
    }

    /// Load all content chunks for a change, in order.
    ///
    /// Reads from CHANGE_CHUNKS (manifest) + CONTENT_CHUNKS (data).
    /// Decompresses each chunk and returns the raw content.
    ///
    /// # Returns
    ///
    /// A vector of decompressed content chunks, ordered by chunk index.
    pub fn load_content_chunks(&self, hash: &[u8; 32]) -> RedbStoreResult<Vec<StoredContentChunk>> {
        let meta = self.load_meta(hash)?;
        let txn = self.db().begin_read()?;
        let manifest_table = txn.open_table(tables::CHANGE_CHUNKS)?;
        let content_table = txn.open_table(tables::CONTENT_CHUNKS)?;

        let mut chunks = Vec::with_capacity(meta.content_chunk_count as usize);

        for idx in 0..meta.content_chunk_count {
            let manifest_key = tables::encode_change_file_key(hash, idx);

            if let Some(chunk_hash_value) = manifest_table.get(&manifest_key)? {
                let chunk_hash = *chunk_hash_value.value();

                if let Some(chunk_data_value) = content_table.get(&chunk_hash)? {
                    let compressed = chunk_data_value.value();
                    let decompressed = zstd::decode_all(compressed).map_err(|e| {
                        RedbStoreError::Corrupt(format!(
                            "content chunk {} decompression failed: {}",
                            idx, e
                        ))
                    })?;

                    chunks.push(StoredContentChunk {
                        index: idx,
                        hash: chunk_hash,
                        data: decompressed,
                    });
                }
            }
        }

        Ok(chunks)
    }

    /// Load the full content blob for a change by concatenating all chunks.
    ///
    /// This is equivalent to reading all CONTENT chunks and joining them.
    ///
    /// # Returns
    ///
    /// The full, decompressed content blob.
    pub fn load_full_content(&self, hash: &[u8; 32]) -> RedbStoreResult<Vec<u8>> {
        let chunks = self.load_content_chunks(hash)?;
        let mut content = Vec::new();
        for chunk in &chunks {
            content.extend_from_slice(&chunk.data);
        }
        Ok(content)
    }

    /// Load the unhashed data for a change (if any).
    ///
    /// Returns `None` if no unhashed data is stored.
    pub fn load_unhashed(&self, hash: &[u8; 32]) -> RedbStoreResult<Option<serde_json::Value>> {
        let txn = self.db().begin_read()?;
        let table = txn.open_table(tables::CHANGE_UNHASHED)?;

        match table.get(hash)? {
            Some(value) => {
                let compressed = value.value();
                let decompressed = zstd::decode_all(compressed).map_err(|e| {
                    RedbStoreError::Corrupt(format!("unhashed decompression failed: {}", e))
                })?;
                let json: serde_json::Value = serde_json::from_slice(&decompressed)?;
                Ok(Some(json))
            }
            None => Ok(None),
        }
    }

    // ── Statistics ──────────────────────────────────────────────────

    /// Get statistics about the store.
    pub fn stats(&self) -> RedbStoreResult<StoreStats> {
        let txn = self.db().begin_read()?;

        let change_count = count_table_entries(&txn, tables::CHANGE_META)?;
        let graph_section_count = count_table_entries(&txn, tables::CHANGE_GRAPH)?;
        let semantic_section_count = count_table_entries(&txn, tables::CHANGE_SEMANTIC)?;
        let content_chunk_count = count_table_entries(&txn, tables::CONTENT_CHUNKS)?;
        let change_chunk_mappings = count_table_entries(&txn, tables::CHANGE_CHUNKS)?;
        let unhashed_count = count_table_entries(&txn, tables::CHANGE_UNHASHED)?;

        Ok(StoreStats {
            change_count,
            graph_section_count,
            semantic_section_count,
            content_chunk_count,
            change_chunk_mappings,
            unhashed_count,
        })
    }

    /// Check if a content chunk exists in the store (by its content hash).
    ///
    /// This is used during delta push to determine which chunks the server
    /// already has without transferring them.
    pub fn has_content_chunk(&self, chunk_hash: &[u8; 32]) -> RedbStoreResult<bool> {
        let txn = self.db().begin_read()?;
        let table = txn.open_table(tables::CONTENT_CHUNKS)?;
        Ok(table.get(chunk_hash)?.is_some())
    }

    /// Get the chunk manifest for a change.
    ///
    /// Returns the ordered list of (chunk_index, chunk_hash) pairs.
    /// This is used for delta transfer negotiation.
    pub fn get_chunk_manifest(&self, hash: &[u8; 32]) -> RedbStoreResult<Vec<(u32, [u8; 32])>> {
        let meta = self.load_meta(hash)?;
        let txn = self.db().begin_read()?;
        let table = txn.open_table(tables::CHANGE_CHUNKS)?;

        let mut manifest = Vec::with_capacity(meta.content_chunk_count as usize);

        for idx in 0..meta.content_chunk_count {
            let key = tables::encode_change_file_key(hash, idx);
            if let Some(value) = table.get(&key)? {
                manifest.push((idx, *value.value()));
            }
        }

        Ok(manifest)
    }

    // ── Load / Delete Operations ───────────────────────────────────

    /// Load a full `Change` from the redb store.
    ///
    /// This reads all tables for the change and reassembles a `Change`
    /// object. Equivalent to deserializing a complete `.change` file.
    ///
    /// For layer-selective reads (e.g., graph-only), use `load_graph_sections()`
    /// or `load_semantic_sections()` instead.
    pub fn load_change(&self, hash: &[u8; 32]) -> RedbStoreResult<Change> {
        let v3_bytes = self.export_v3_bytes(hash)?;
        let mut cursor = Cursor::new(&v3_bytes);
        let (change, _verified_hash) = Change::deserialize(&mut cursor).map_err(|e| {
            RedbStoreError::Corrupt(format!("change deserialization failed: {}", e))
        })?;
        Ok(change)
    }

    /// Delete a change from the store.
    ///
    /// Removes all entries for this change from CHANGE_META, CHANGE_GRAPH,
    /// CHANGE_SEMANTIC, CHANGE_CHUNKS, and CHANGE_UNHASHED.
    ///
    /// **Note**: Content chunks in CONTENT_CHUNKS are NOT deleted because
    /// they may be shared with other changes.
    pub fn delete_change(&self, hash: &[u8; 32]) -> RedbStoreResult<bool> {
        let meta = match self.load_meta(hash) {
            Ok(m) => m,
            Err(RedbStoreError::NotFound { .. }) => return Ok(false),
            Err(e) => return Err(e),
        };

        let txn = self.db().begin_write()?;
        {
            let mut meta_table = txn.open_table(tables::CHANGE_META)?;
            meta_table.remove(hash)?;

            let mut graph_table = txn.open_table(tables::CHANGE_GRAPH)?;
            for idx in 0..meta.graph_section_count {
                let key = tables::encode_change_file_key(hash, idx);
                graph_table.remove(&key)?;
            }

            let mut semantic_table = txn.open_table(tables::CHANGE_SEMANTIC)?;
            for idx in 0..meta.semantic_section_count {
                let key = tables::encode_change_file_key(hash, idx);
                semantic_table.remove(&key)?;
            }

            let mut change_chunks_table = txn.open_table(tables::CHANGE_CHUNKS)?;
            for idx in 0..meta.content_chunk_count {
                let key = tables::encode_change_file_key(hash, idx);
                change_chunks_table.remove(&key)?;
            }

            let mut unhashed_table = txn.open_table(tables::CHANGE_UNHASHED)?;
            unhashed_table.remove(hash)?;
        }
        txn.commit()?;

        Ok(true)
    }

    // ── Export Operations ──────────────────────────────────────────

    /// Export a change from redb to V3 `.change` file format.
    ///
    /// Reads all sections from redb and assembles a complete V3 change
    /// file. This is used for push/pull (network transfer) and
    /// `atomic export` (offline sharing).
    pub fn export_v3_file<P: AsRef<Path>>(&self, hash: &[u8; 32], dest: P) -> RedbStoreResult<()> {
        let bytes = self.export_v3_bytes(hash)?;
        std::fs::write(dest, &bytes)?;
        Ok(())
    }

    /// Export a change from redb to V3 bytes in memory.
    ///
    /// This is the core export method used by both file export and
    /// network transfer.
    pub fn export_v3_bytes(&self, hash: &[u8; 32]) -> RedbStoreResult<Vec<u8>> {
        let meta = self.load_meta(hash)?;

        // Reconstruct the hash dedup table
        let hash_table = if meta.hash_table.is_empty() {
            HashDedupTable::empty()
        } else {
            HashDedupTable::from_hashes(meta.hash_table.clone()).map_err(|e| {
                RedbStoreError::Corrupt(format!("hash table reconstruction failed: {}", e))
            })?
        };

        // Build file header
        let mut file_header_builder = FileHeader::builder()
            .hash_table_entries(hash_table.len() as u32)
            .graph_section_count(meta.graph_section_count)
            .semantic_section_count(meta.semantic_section_count)
            .contents_chunks(meta.content_chunk_count);

        if meta.has_provenance {
            file_header_builder = file_header_builder.with_provenance();
        }
        if meta.has_unhashed {
            file_header_builder = file_header_builder.with_unhashed();
        }

        let file_header = file_header_builder.build();

        // Write V3 format
        let mut output = Vec::new();
        let mut writer = ChangeWriter::new(&mut output, WriterOptions::default());

        writer.write_file_header(&file_header)?;
        writer.write_hash_table(&hash_table)?;

        // Write HEADER section
        writer.write_change_header(&meta.header)?;

        // Write DEPS section
        writer.write_dependencies(&meta.dependency_indices)?;

        // Write GRAPH sections
        let txn = self.db().begin_read()?;
        {
            let graph_table = txn.open_table(tables::CHANGE_GRAPH)?;
            for idx in 0..meta.graph_section_count {
                let key = tables::encode_change_file_key(hash, idx);
                if let Some(value) = graph_table.get(&key)? {
                    let compressed = value.value();
                    let decompressed = zstd::decode_all(compressed).map_err(|e| {
                        RedbStoreError::Corrupt(format!(
                            "graph section {} decompression failed: {}",
                            idx, e
                        ))
                    })?;
                    writer.write_graph_section(&decompressed)?;
                }
            }
        }

        // Write SEMANTIC sections
        {
            let semantic_table = txn.open_table(tables::CHANGE_SEMANTIC)?;
            for idx in 0..meta.semantic_section_count {
                let key = tables::encode_change_file_key(hash, idx);
                if let Some(value) = semantic_table.get(&key)? {
                    let compressed = value.value();
                    let decompressed = zstd::decode_all(compressed).map_err(|e| {
                        RedbStoreError::Corrupt(format!(
                            "semantic section {} decompression failed: {}",
                            idx, e
                        ))
                    })?;
                    writer.write_semantic_section(&decompressed)?;
                }
            }
        }

        // Write CONTENT chunks
        {
            let manifest_table = txn.open_table(tables::CHANGE_CHUNKS)?;
            let content_table = txn.open_table(tables::CONTENT_CHUNKS)?;

            for idx in 0..meta.content_chunk_count {
                let manifest_key = tables::encode_change_file_key(hash, idx);
                if let Some(chunk_hash_value) = manifest_table.get(&manifest_key)? {
                    let chunk_hash = chunk_hash_value.value();
                    if let Some(chunk_data_value) = content_table.get(chunk_hash)? {
                        let compressed = chunk_data_value.value();
                        let decompressed = zstd::decode_all(compressed).map_err(|e| {
                            RedbStoreError::Corrupt(format!(
                                "content chunk {} decompression failed: {}",
                                idx, e
                            ))
                        })?;
                        writer.write_content_chunk(idx, &decompressed)?;
                    }
                }
            }
        }

        // Write UNHASHED section
        {
            let unhashed_table = txn.open_table(tables::CHANGE_UNHASHED)?;
            if let Some(value) = unhashed_table.get(hash)? {
                let compressed = value.value();
                let decompressed = zstd::decode_all(compressed).map_err(|e| {
                    RedbStoreError::Corrupt(format!("unhashed decompression failed: {}", e))
                })?;
                writer.write_unhashed(&decompressed)?;
            }
        }

        drop(txn);

        writer.finalize()?;

        Ok(output)
    }
}

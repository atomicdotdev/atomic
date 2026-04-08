//! redb-native change storage for Atomic VCS.
//!
//! This module implements [`RedbChangeStore`], which stores change data directly
//! in redb tables instead of `.change` files. This is the primary storage format
//! for Phase 5 of the V3 change format proposal.
//!
//! # Sub-modules
//!
//! - [`batch`]: `StoredSection` type for layer-selective reads
//! - [`queries`]: Read-only query/stats/export operations on the store

mod batch;
mod queries;

pub use batch::StoredSection;
pub use queries::{StoreStats, StoredContentChunk};

use atomic_core::change::format_v3::{self, ChangeReader, FormatError, SectionType};
use atomic_core::change::{Change, ChangeHeader};
use atomic_core::pristine::tables;
use redb::{Database, ReadableTable, TableDefinition};
use std::fmt;
use std::io::Cursor;
use std::path::Path;
use thiserror::Error;

// ═══════════════════════════════════════════════════════════════════════
// Error types
// ═══════════════════════════════════════════════════════════════════════

/// Errors from redb change storage operations.
#[derive(Debug, Error)]
pub enum RedbStoreError {
    /// redb database error.
    #[error("Database error: {0}")]
    Database(#[from] Box<redb::DatabaseError>),

    /// redb transaction error.
    #[error("Transaction error: {0}")]
    Transaction(#[from] Box<redb::TransactionError>),

    /// redb table error.
    #[error("Table error: {0}")]
    Table(#[from] Box<redb::TableError>),

    /// redb storage error.
    #[error("Storage error: {0}")]
    Storage(#[from] Box<redb::StorageError>),

    /// redb commit error.
    #[error("Commit error: {0}")]
    Commit(#[from] Box<redb::CommitError>),

    /// V3 format error (postcard, compression, hash mismatch, etc.).
    #[error("Format error: {0}")]
    Format(#[from] FormatError),

    /// Change not found in the store.
    #[error("Change not found: {hash}")]
    NotFound {
        /// Base32 representation of the missing hash.
        hash: String,
    },

    /// I/O error during file import/export.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON error for unhashed data.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Serialization error.
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// The change data is corrupt or inconsistent.
    #[error("Corrupt change data: {0}")]
    Corrupt(String),
}

/// Convenience result type for redb store operations.
pub type RedbStoreResult<T> = Result<T, RedbStoreError>;

impl From<redb::DatabaseError> for RedbStoreError {
    fn from(e: redb::DatabaseError) -> Self {
        RedbStoreError::Database(Box::new(e))
    }
}

impl From<redb::TransactionError> for RedbStoreError {
    fn from(e: redb::TransactionError) -> Self {
        RedbStoreError::Transaction(Box::new(e))
    }
}

impl From<redb::TableError> for RedbStoreError {
    fn from(e: redb::TableError) -> Self {
        RedbStoreError::Table(Box::new(e))
    }
}

impl From<redb::StorageError> for RedbStoreError {
    fn from(e: redb::StorageError) -> Self {
        RedbStoreError::Storage(Box::new(e))
    }
}

impl From<redb::CommitError> for RedbStoreError {
    fn from(e: redb::CommitError) -> Self {
        RedbStoreError::Commit(Box::new(e))
    }
}

// ═══════════════════════════════════════════════════════════════════════
// StoredChangeMeta — metadata blob contents
// ═══════════════════════════════════════════════════════════════════════

/// Metadata stored in the CHANGE_META table for a single change.
///
/// This is the decoded form of the compressed blob. It contains everything
/// from the V3 metadata sections: header, deps, provenance, hash table.
///
/// Serialized with postcard, compressed with zstd for storage.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredChangeMeta {
    /// The change header (message, authors, timestamp).
    pub header: ChangeHeader,

    /// Dependency hash indices (referencing the hash table).
    pub dependency_indices: Vec<u16>,

    /// The hash dedup table entries (ordered list of 32-byte hashes).
    /// Index 0 = self hash, index 1+ = dependency hashes.
    pub hash_table: Vec<[u8; 32]>,

    /// Number of graph sections stored for this change.
    pub graph_section_count: u32,

    /// Number of semantic sections stored for this change.
    pub semantic_section_count: u32,

    /// Number of content chunks stored for this change.
    pub content_chunk_count: u32,

    /// Whether provenance data is stored.
    pub has_provenance: bool,

    /// Whether unhashed data is stored.
    pub has_unhashed: bool,
}

// ═══════════════════════════════════════════════════════════════════════
// RedbChangeStore — the main store
// ═══════════════════════════════════════════════════════════════════════

/// redb-native change storage.
///
/// Stores change data in redb tables with per-section granularity,
/// enabling layer-selective reads and content deduplication.
///
/// # Table Layout
///
/// | Table | Key | Value | Purpose |
/// |-------|-----|-------|---------|
/// | `CHANGE_META` | `[u8; 32]` (hash) | compressed meta blob | Header + deps + hash table |
/// | `CHANGE_GRAPH` | `[u8; 36]` (hash + idx) | compressed graph ops | Per-file graph sections |
/// | `CHANGE_SEMANTIC` | `[u8; 36]` (hash + idx) | compressed semantic ops | Per-file semantic sections |
/// | `CONTENT_CHUNKS` | `[u8; 32]` (chunk hash) | compressed content | Deduped content chunks |
/// | `CHANGE_CHUNKS` | `[u8; 36]` (hash + idx) | `[u8; 32]` (chunk hash) | Change → chunk manifest |
/// | `CHANGE_UNHASHED` | `[u8; 32]` (hash) | compressed JSON | AI transcripts, etc. |
pub struct RedbChangeStore {
    db: Database,
}

impl RedbChangeStore {
    /// Open or create a redb change store at the given path.
    ///
    /// Creates all required tables on first use. Subsequent opens
    /// use the existing tables.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the redb database file. This can be the same
    ///   database as the pristine (tables are namespaced) or a separate file.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or tables cannot be created.
    pub fn open<P: AsRef<Path>>(path: P) -> RedbStoreResult<Self> {
        let db = Database::create(path)?;

        // Create all tables on first use
        {
            let txn = db.begin_write()?;
            {
                let _ = txn.open_table(tables::CHANGE_META)?;
                let _ = txn.open_table(tables::CHANGE_GRAPH)?;
                let _ = txn.open_table(tables::CHANGE_SEMANTIC)?;
                let _ = txn.open_table(tables::CONTENT_CHUNKS)?;
                let _ = txn.open_table(tables::CHANGE_CHUNKS)?;
                let _ = txn.open_table(tables::CHANGE_UNHASHED)?;
            }
            txn.commit()?;
        }

        Ok(Self { db })
    }

    /// Get a reference to the underlying database.
    pub(crate) fn db(&self) -> &Database {
        &self.db
    }

    // ── Write Operations ───────────────────────────────────────────

    /// Import a V3 `.change` file into the redb store.
    ///
    /// Reads the V3 file section by section using [`ChangeReader`], then
    /// stores each section in the appropriate redb table. Content chunks
    /// are deduplicated by their blake3 hash — if a chunk already exists
    /// in CONTENT_CHUNKS, it's not written again.
    ///
    /// # Arguments
    ///
    /// * `file_path` - Path to the `.change` file to import.
    ///
    /// # Returns
    ///
    /// The blake3 content hash of the imported change (its identity).
    ///
    /// # Errors
    ///
    /// Returns an error if the file can't be read, is not valid V3 format,
    /// or if redb writes fail.
    pub fn import_v3_file<P: AsRef<Path>>(&self, file_path: P) -> RedbStoreResult<[u8; 32]> {
        let file_data = std::fs::read(file_path.as_ref())?;
        self.import_v3_bytes(&file_data)
    }

    /// Import V3 change data from a byte slice.
    ///
    /// This is the core import method — used by both `import_v3_file` and
    /// by the network layer when receiving change data during pull.
    pub fn import_v3_bytes(&self, data: &[u8]) -> RedbStoreResult<[u8; 32]> {
        let mut cursor = Cursor::new(data);
        let mut reader = ChangeReader::open(&mut cursor)?;

        let _file_header = *reader.file_header();
        let hash_table = reader.hash_table().clone();

        // Read all sections
        let mut meta_header: Option<ChangeHeader> = None;
        let mut meta_deps: Vec<u16> = Vec::new();
        let mut provenance_payload: Option<Vec<u8>> = None;
        let mut graph_sections: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut semantic_sections: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut content_chunks: Vec<(u32, [u8; 32], Vec<u8>)> = Vec::new();
        let mut unhashed_payload: Option<Vec<u8>> = None;

        let mut graph_idx = 0u32;
        let mut semantic_idx = 0u32;

        while let Some(section) = reader.next_section()? {
            match section.section_type {
                SectionType::Header => {
                    meta_header = Some(
                        postcard::from_bytes(&section.payload)
                            .map_err(|e| RedbStoreError::Serialization(e.to_string()))?,
                    );
                }
                SectionType::Dependencies => {
                    meta_deps = postcard::from_bytes(&section.payload)
                        .map_err(|e| RedbStoreError::Serialization(e.to_string()))?;
                }
                SectionType::Provenance => {
                    provenance_payload = Some(section.payload.clone());
                }
                SectionType::Graph => {
                    graph_sections.push((graph_idx, section.payload.clone()));
                    graph_idx += 1;
                }
                SectionType::Semantic => {
                    semantic_sections.push((semantic_idx, section.payload.clone()));
                    semantic_idx += 1;
                }
                SectionType::Content => {
                    if let Some(info) = &section.content_chunk_info {
                        content_chunks.push((
                            info.chunk_index,
                            info.chunk_hash,
                            section.payload.clone(),
                        ));
                    }
                }
                SectionType::Unhashed => {
                    unhashed_payload = Some(section.payload.clone());
                }
            }
        }

        // Verify hash
        let content_hash = reader.verify()?;

        // Build the metadata blob
        let header = meta_header
            .ok_or_else(|| RedbStoreError::Corrupt("missing HEADER section".to_string()))?;

        let meta = StoredChangeMeta {
            header,
            dependency_indices: meta_deps,
            hash_table: hash_table.hashes().to_vec(),
            graph_section_count: graph_sections.len() as u32,
            semantic_section_count: semantic_sections.len() as u32,
            content_chunk_count: content_chunks.len() as u32,
            has_provenance: provenance_payload.is_some(),
            has_unhashed: unhashed_payload.is_some(),
        };

        // Serialize and compress metadata
        let meta_bytes = postcard::to_allocvec(&meta)
            .map_err(|e| RedbStoreError::Serialization(e.to_string()))?;
        let meta_compressed = zstd::encode_all(&meta_bytes[..], 3)
            .map_err(|e| RedbStoreError::Serialization(e.to_string()))?;

        // Write everything in a single transaction
        let txn = self.db.begin_write()?;
        {
            // CHANGE_META
            let mut meta_table = txn.open_table(tables::CHANGE_META)?;
            meta_table.insert(&content_hash, meta_compressed.as_slice())?;

            // CHANGE_GRAPH
            let mut graph_table = txn.open_table(tables::CHANGE_GRAPH)?;
            for (idx, payload) in &graph_sections {
                let key = tables::encode_change_file_key(&content_hash, *idx);
                let compressed = zstd::encode_all(payload.as_slice(), 3)
                    .map_err(|e| RedbStoreError::Serialization(e.to_string()))?;
                graph_table.insert(&key, compressed.as_slice())?;
            }

            // CHANGE_SEMANTIC
            let mut semantic_table = txn.open_table(tables::CHANGE_SEMANTIC)?;
            for (idx, payload) in &semantic_sections {
                let key = tables::encode_change_file_key(&content_hash, *idx);
                let compressed = zstd::encode_all(payload.as_slice(), 3)
                    .map_err(|e| RedbStoreError::Serialization(e.to_string()))?;
                semantic_table.insert(&key, compressed.as_slice())?;
            }

            // CONTENT_CHUNKS (content-addressed — skip if already present)
            let mut chunk_table = txn.open_table(tables::CONTENT_CHUNKS)?;
            for (_idx, chunk_hash, chunk_data) in &content_chunks {
                if chunk_table.get(chunk_hash)?.is_none() {
                    let compressed = zstd::encode_all(chunk_data.as_slice(), 3)
                        .map_err(|e| RedbStoreError::Serialization(e.to_string()))?;
                    chunk_table.insert(chunk_hash, compressed.as_slice())?;
                }
            }

            // CHANGE_CHUNKS (change → ordered chunk manifest)
            let mut change_chunks_table = txn.open_table(tables::CHANGE_CHUNKS)?;
            for (idx, chunk_hash, _) in &content_chunks {
                let key = tables::encode_change_file_key(&content_hash, *idx);
                change_chunks_table.insert(&key, chunk_hash)?;
            }

            // CHANGE_UNHASHED
            if let Some(unhashed) = &unhashed_payload {
                let mut unhashed_table = txn.open_table(tables::CHANGE_UNHASHED)?;
                let compressed = zstd::encode_all(unhashed.as_slice(), 3)
                    .map_err(|e| RedbStoreError::Serialization(e.to_string()))?;
                unhashed_table.insert(&content_hash, compressed.as_slice())?;
            }
        }
        txn.commit()?;

        Ok(content_hash)
    }

    /// Store a `Change` object into the redb store.
    ///
    /// This serializes the change to V3 format, then imports the sections
    /// into the redb tables. This is the primary write path during recording.
    ///
    /// # Returns
    ///
    /// The blake3 content hash of the stored change.
    pub fn save_change(&self, change: &Change) -> RedbStoreResult<[u8; 32]> {
        let mut buffer = Vec::new();
        change
            .serialize(&mut buffer)
            .map_err(|e| RedbStoreError::Serialization(e.to_string()))?;
        self.import_v3_bytes(&buffer)
    }

    // ── Read Operations ────────────────────────────────────────────

    /// Check if a change exists in the store.
    pub fn has_change(&self, hash: &[u8; 32]) -> RedbStoreResult<bool> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(tables::CHANGE_META)?;
        Ok(table.get(hash)?.is_some())
    }

    /// Load the metadata for a change.
    ///
    /// This reads only from CHANGE_META — the cheapest possible read.
    /// Useful for `atomic log`, header inspection, dependency resolution.
    pub fn load_meta(&self, hash: &[u8; 32]) -> RedbStoreResult<StoredChangeMeta> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(tables::CHANGE_META)?;

        let value = table.get(hash)?.ok_or_else(|| RedbStoreError::NotFound {
            hash: format!(
                "{:02x}{:02x}{:02x}{:02x}…",
                hash[0], hash[1], hash[2], hash[3]
            ),
        })?;

        let compressed = value.value();
        let decompressed = zstd::decode_all(compressed)
            .map_err(|e| RedbStoreError::Corrupt(format!("meta decompression failed: {}", e)))?;
        let meta: StoredChangeMeta = postcard::from_bytes(&decompressed)
            .map_err(|e| RedbStoreError::Corrupt(format!("meta deserialization failed: {}", e)))?;

        Ok(meta)
    }

    // NOTE: load_change(), export_v3_file(), export_v3_bytes(), and
    // delete_change() are in queries.rs
}

impl fmt::Debug for RedbChangeStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedbChangeStore")
            .field("tables", &"[CHANGE_META, CHANGE_GRAPH, CHANGE_SEMANTIC, CONTENT_CHUNKS, CHANGE_CHUNKS, CHANGE_UNHASHED]")
            .finish()
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

/// Try to extract the file path from a graph section payload.
///
/// The payload is a postcard-encoded `GraphSectionPayload` where the
/// first field is the path string. We do a best-effort extraction
/// without fully deserializing the payload.
pub(crate) fn extract_path_from_payload(payload: &[u8]) -> String {
    // Try to deserialize as GraphSectionPayload to get the path
    if let Ok(graph_payload) = format_v3::GraphSectionPayload::from_postcard_bytes(payload) {
        return graph_payload.path().to_string();
    }
    String::new()
}

/// Count entries in a redb table by iterating (redb tables don't have a len() method).
pub(crate) fn count_table_entries<K: redb::Key + 'static, V: redb::Value + 'static>(
    txn: &redb::ReadTransaction,
    table_def: TableDefinition<K, V>,
) -> RedbStoreResult<u64> {
    let table = txn.open_table(table_def)?;
    let mut count = 0u64;
    let iter = table.iter()?;
    for _ in iter {
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

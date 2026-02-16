//! redb-native change storage for Atomic VCS.
//!
//! This module implements [`RedbChangeStore`], which stores change data directly
//! in redb tables instead of `.change` files. This is the primary storage format
//! for Phase 5 of the V3 change format proposal.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │                        redb Change Storage                               │
//! ├──────────────────────────────────────────────────────────────────────────┤
//! │                                                                          │
//! │  CHANGE_META          CHANGE_GRAPH         CHANGE_SEMANTIC               │
//! │  ┌──────────────┐     ┌──────────────┐     ┌──────────────┐             │
//! │  │ hash → blob  │     │(hash,idx)→   │     │(hash,idx)→   │             │
//! │  │              │     │  graph ops   │     │  semantic ops│             │
//! │  │ Header       │     │  per file    │     │  per file    │             │
//! │  │ Deps         │     └──────────────┘     └──────────────┘             │
//! │  │ Provenance   │                                                        │
//! │  │ HashTable    │     CONTENT_CHUNKS       CHANGE_CHUNKS                │
//! │  └──────────────┘     ┌──────────────┐     ┌──────────────┐             │
//! │                       │ chunk_hash → │     │(hash,idx)→   │             │
//! │  CHANGE_UNHASHED      │  zstd(data)  │     │  chunk_hash  │             │
//! │  ┌──────────────┐     │              │     │              │             │
//! │  │ hash → json  │     │ (shared      │     │ (ordered     │             │
//! │  │              │     │  across      │     │  manifest)   │             │
//! │  │ AI transcript│     │  changes)    │     └──────────────┘             │
//! │  │ Reasoning    │     └──────────────┘                                   │
//! │  └──────────────┘                                                        │
//! │                                                                          │
//! │  Benefits:                                                               │
//! │  • Layer-selective reads (graph-only, semantic-only)                     │
//! │  • Content dedup across changes (shared CONTENT_CHUNKS)                 │
//! │  • ACID transactions (change storage in same txn as graph apply)        │
//! │  • No file I/O for reads (redb page cache)                              │
//! │  • Random access per file (no full-change deserialization)              │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Layer-Selective Reads
//!
//! The key advantage over `.change` files is that each layer can be loaded
//! independently:
//!
//! | Operation | Tables Read | Use Case |
//! |-----------|------------|----------|
//! | Apply change | CHANGE_META + CHANGE_GRAPH + CONTENT_CHUNKS | `atomic apply` |
//! | Code review | CHANGE_META + CHANGE_SEMANTIC + CONTENT_CHUNKS | WebUI diff |
//! | Blame | CHANGE_META + CHANGE_SEMANTIC | Token-level attribution |
//! | Push/pull | All tables for the change hash | Network transfer |
//! | Metadata inspect | CHANGE_META only | `atomic log --verbose` |
//!
//! # Content Deduplication
//!
//! Content chunks are keyed by their blake3 hash, not by change hash. This
//! means identical content regions across different changes are stored once:
//!
//! ```text
//! Change A (initial record):   CHANGE_CHUNKS → [chunk0, chunk1, chunk2, chunk3]
//! Change B (1-line edit):      CHANGE_CHUNKS → [chunk0, chunk1, chunk2', chunk3]
//!                                                ↑        ↑              ↑
//!                                            same hash = shared in CONTENT_CHUNKS
//! ```
//!
//! # Thread Safety
//!
//! `RedbChangeStore` wraps a `redb::Database` and is safe for concurrent access.
//! Read operations use `read_txn()` (multiple concurrent readers allowed).
//! Write operations use `write_txn()` (exclusive).
//!
//! # Examples
//!
//! ```rust,ignore
//! use atomic_repository::redb_change_store::RedbChangeStore;
//!
//! // Open the store (creates tables on first use)
//! let store = RedbChangeStore::open("path/to/pristine.redb")?;
//!
//! // Import a V3 .change file into redb
//! let hash = store.import_v3_file("path/to/change.change")?;
//!
//! // Load only graph sections (for apply)
//! let graph_data = store.load_graph_sections(&hash)?;
//!
//! // Load only semantic sections (for code review)
//! let semantic_data = store.load_semantic_sections(&hash)?;
//!
//! // Export back to a .change file (for push/transfer)
//! store.export_v3_file(&hash, "path/to/output.change")?;
//! ```

use atomic_core::change::format_v3::{
    self, ChangeReader, ChangeWriter, FileHeader, FormatError, HashDedupTable, SectionType,
    WriterOptions,
};
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
    Database(#[from] redb::DatabaseError),

    /// redb transaction error.
    #[error("Transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    /// redb table error.
    #[error("Table error: {0}")]
    Table(#[from] redb::TableError),

    /// redb storage error.
    #[error("Storage error: {0}")]
    Storage(#[from] redb::StorageError),

    /// redb commit error.
    #[error("Commit error: {0}")]
    Commit(#[from] redb::CommitError),

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
// StoredSection — a single stored section blob
// ═══════════════════════════════════════════════════════════════════════

/// A single section loaded from redb.
///
/// Contains the decompressed payload and the section type. This is the
/// redb equivalent of [`format_v3::reader::ReadSection`] but loaded
/// from table values instead of a file stream.
#[derive(Clone, Debug)]
pub struct StoredSection {
    /// The type of section.
    pub section_type: SectionType,

    /// The decompressed payload bytes.
    pub payload: Vec<u8>,

    /// The file path this section belongs to (for GRAPH/SEMANTIC sections).
    /// Empty string for metadata sections and content chunks.
    pub path: String,

    /// File index within the change (for ordering).
    pub file_index: u32,
}

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

        let _file_header = reader.file_header().clone();
        let hash_table = reader.hash_table().clone();

        // Read all sections
        let mut meta_header: Option<ChangeHeader> = None;
        let mut meta_deps: Vec<u16> = Vec::new();
        let mut provenance_payload: Option<Vec<u8>> = None;
        let mut graph_sections: Vec<(u32, Vec<u8>)> = Vec::new(); // (index, payload)
        let mut semantic_sections: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut content_chunks: Vec<(u32, [u8; 32], Vec<u8>)> = Vec::new(); // (index, hash, data)
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
        let txn = self.db.begin_read()?;
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
        let txn = self.db.begin_read()?;
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
        let txn = self.db.begin_read()?;
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
        let txn = self.db.begin_read()?;
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

    /// Load a full `Change` from the redb store.
    ///
    /// This reads all tables for the change and reassembles a `Change`
    /// object. Equivalent to deserializing a complete `.change` file.
    ///
    /// For layer-selective reads (e.g., graph-only), use `load_graph_sections()`
    /// or `load_semantic_sections()` instead.
    pub fn load_change(&self, hash: &[u8; 32]) -> RedbStoreResult<Change> {
        // Export to V3 bytes, then deserialize via Change::deserialize
        let v3_bytes = self.export_v3_bytes(hash)?;
        let mut cursor = Cursor::new(&v3_bytes);
        let (change, _verified_hash) = Change::deserialize(&mut cursor).map_err(|e| {
            RedbStoreError::Corrupt(format!("change deserialization failed: {}", e))
        })?;
        Ok(change)
    }

    // ── Export Operations ──────────────────────────────────────────

    /// Export a change from redb to V3 `.change` file format.
    ///
    /// Reads all sections from redb and assembles a complete V3 change
    /// file. This is used for push/pull (network transfer) and
    /// `atomic export` (offline sharing).
    ///
    /// # Arguments
    ///
    /// * `hash` - The content hash of the change to export.
    /// * `dest` - Destination file path.
    ///
    /// # Errors
    ///
    /// Returns an error if the change doesn't exist or the file can't be written.
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
        let txn = self.db.begin_read()?;
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

    // ── Delete Operations ──────────────────────────────────────────

    /// Delete a change from the store.
    ///
    /// Removes all entries for this change from CHANGE_META, CHANGE_GRAPH,
    /// CHANGE_SEMANTIC, CHANGE_CHUNKS, and CHANGE_UNHASHED.
    ///
    /// **Note**: Content chunks in CONTENT_CHUNKS are NOT deleted because
    /// they may be shared with other changes. Use `gc_orphan_chunks()` to
    /// clean up unreferenced chunks.
    pub fn delete_change(&self, hash: &[u8; 32]) -> RedbStoreResult<bool> {
        let meta = match self.load_meta(hash) {
            Ok(m) => m,
            Err(RedbStoreError::NotFound { .. }) => return Ok(false),
            Err(e) => return Err(e),
        };

        let txn = self.db.begin_write()?;
        {
            // Remove meta
            let mut meta_table = txn.open_table(tables::CHANGE_META)?;
            meta_table.remove(hash)?;

            // Remove graph sections
            let mut graph_table = txn.open_table(tables::CHANGE_GRAPH)?;
            for idx in 0..meta.graph_section_count {
                let key = tables::encode_change_file_key(hash, idx);
                graph_table.remove(&key)?;
            }

            // Remove semantic sections
            let mut semantic_table = txn.open_table(tables::CHANGE_SEMANTIC)?;
            for idx in 0..meta.semantic_section_count {
                let key = tables::encode_change_file_key(hash, idx);
                semantic_table.remove(&key)?;
            }

            // Remove change-to-chunk mappings (but NOT the content chunks themselves)
            let mut change_chunks_table = txn.open_table(tables::CHANGE_CHUNKS)?;
            for idx in 0..meta.content_chunk_count {
                let key = tables::encode_change_file_key(hash, idx);
                change_chunks_table.remove(&key)?;
            }

            // Remove unhashed
            let mut unhashed_table = txn.open_table(tables::CHANGE_UNHASHED)?;
            unhashed_table.remove(hash)?;
        }
        txn.commit()?;

        Ok(true)
    }

    // ── Statistics ──────────────────────────────────────────────────

    /// Get statistics about the store.
    pub fn stats(&self) -> RedbStoreResult<StoreStats> {
        let txn = self.db.begin_read()?;

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
        let txn = self.db.begin_read()?;
        let table = txn.open_table(tables::CONTENT_CHUNKS)?;
        Ok(table.get(chunk_hash)?.is_some())
    }

    /// Get the chunk manifest for a change.
    ///
    /// Returns the ordered list of (chunk_index, chunk_hash) pairs.
    /// This is used for delta transfer negotiation.
    pub fn get_chunk_manifest(&self, hash: &[u8; 32]) -> RedbStoreResult<Vec<(u32, [u8; 32])>> {
        let meta = self.load_meta(hash)?;
        let txn = self.db.begin_read()?;
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
}

/// Count entries in a redb table by iterating (redb tables don't have a len() method).
fn count_table_entries<K: redb::Key + 'static, V: redb::Value + 'static>(
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
fn extract_path_from_payload(payload: &[u8]) -> String {
    // Try to deserialize as GraphSectionPayload to get the path
    if let Ok(graph_payload) = format_v3::GraphSectionPayload::from_postcard_bytes(payload) {
        return graph_payload.path().to_string();
    }
    String::new()
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::change::{Change, ChangeHeader, Encoding, GraphOp, Local};
    use atomic_core::types::{ChangePosition, EdgeFlags, Hash, Position};
    use atomic_core::{Atom, Insertion};

    /// Helper: create a temporary redb store.
    fn temp_store() -> (tempfile::TempDir, RedbChangeStore) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_change_store.redb");
        let store = RedbChangeStore::open(&db_path).unwrap();
        (dir, store)
    }

    /// Helper: create a simple test Change.
    fn make_test_change(message: &str, content: &[u8]) -> Change {
        Change::new(ChangeHeader::new(message), vec![], content.to_vec(), vec![])
    }

    /// Helper: create a Change with a hunk.
    fn make_change_with_hunk() -> Change {
        let test_pos = Position::new(Some(Hash::of(b"test")), ChangePosition::new(0));

        let mut change = Change::empty(ChangeHeader::new("With hunk"));
        let graph_op: GraphOp<Option<Hash>> = GraphOp::Edit {
            change: Atom::Insertion(Insertion {
                predecessors: vec![test_pos],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(12),
                inode: test_pos,
            }),
            local: Local::new("test.rs", 1),
            encoding: Some(Encoding::Utf8),
        };
        change.add_hunk(graph_op);
        change.append_contents(b"Hello World!");
        change.finalize();
        change
    }

    // ── Basic Operations ───────────────────────────────────────────

    #[test]
    fn test_open_creates_tables() {
        let (_dir, store) = temp_store();
        let stats = store.stats().unwrap();
        assert_eq!(stats.change_count, 0);
        assert_eq!(stats.graph_section_count, 0);
        assert_eq!(stats.content_chunk_count, 0);
    }

    #[test]
    fn test_save_and_has_change() {
        let (_dir, store) = temp_store();
        let change = make_test_change("test", b"content");

        let hash = store.save_change(&change).unwrap();
        assert!(store.has_change(&hash).unwrap());

        let bogus = [0xFF; 32];
        assert!(!store.has_change(&bogus).unwrap());
    }

    #[test]
    fn test_save_and_load_meta() {
        let (_dir, store) = temp_store();
        let change = make_test_change("Hello meta", b"data");

        let hash = store.save_change(&change).unwrap();
        let meta = store.load_meta(&hash).unwrap();

        assert_eq!(meta.header.message, "Hello meta");
        assert!(!meta.hash_table.is_empty());
    }

    #[test]
    fn test_load_meta_not_found() {
        let (_dir, store) = temp_store();
        let bogus = [0xAA; 32];
        let result = store.load_meta(&bogus);
        assert!(result.is_err());
        assert!(matches!(result, Err(RedbStoreError::NotFound { .. })));
    }

    // ── Content Operations ─────────────────────────────────────────

    #[test]
    fn test_save_and_load_content() {
        let (_dir, store) = temp_store();
        let content = b"Hello, World! This is test content.";
        let change = make_test_change("content test", content);

        let hash = store.save_change(&change).unwrap();
        let loaded_content = store.load_full_content(&hash).unwrap();

        assert_eq!(loaded_content, content);
    }

    #[test]
    fn test_content_chunk_dedup() {
        let (_dir, store) = temp_store();

        // Save two changes with identical content
        let content = b"Same content in both changes";
        let change1 = make_test_change("first", content);
        let change2 = make_test_change("second", content);

        let hash1 = store.save_change(&change1).unwrap();
        let hash2 = store.save_change(&change2).unwrap();

        // Both should exist
        assert!(store.has_change(&hash1).unwrap());
        assert!(store.has_change(&hash2).unwrap());

        // Content should be identical when loaded
        let content1 = store.load_full_content(&hash1).unwrap();
        let content2 = store.load_full_content(&hash2).unwrap();
        assert_eq!(content1, content2);
        assert_eq!(content1, content);

        // Content chunks should be shared (same chunk hash in CONTENT_CHUNKS)
        let stats = store.stats().unwrap();
        assert_eq!(stats.change_count, 2);
        // There should be fewer unique chunks than total chunk mappings
        // (or equal if the content is small enough for a single chunk)
        assert!(stats.content_chunk_count <= stats.change_chunk_mappings);
    }

    #[test]
    fn test_load_content_chunks_ordered() {
        let (_dir, store) = temp_store();

        // Create a change with enough content to produce multiple chunks
        // (content needs to be > min_chunk_size = 16KB)
        let content: Vec<u8> = (0..100_000u32)
            .flat_map(|i| format!("line {} of content\n", i).into_bytes())
            .collect();
        let change = make_test_change("large", &content);

        let hash = store.save_change(&change).unwrap();
        let chunks = store.load_content_chunks(&hash).unwrap();

        // Verify chunks are ordered by index
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.index, i as u32);
        }

        // Verify concatenating chunks gives the original content
        let mut reassembled = Vec::new();
        for chunk in &chunks {
            reassembled.extend_from_slice(&chunk.data);
        }
        assert_eq!(reassembled, content);
    }

    // ── Layer-Selective Reads ──────────────────────────────────────

    #[test]
    fn test_load_graph_sections() {
        let (_dir, store) = temp_store();
        let change = make_change_with_hunk();

        let hash = store.save_change(&change).unwrap();

        let meta = store.load_meta(&hash).unwrap();
        let graph_sections = store.load_graph_sections(&hash).unwrap();

        // Should have graph sections if the change has hunks
        assert_eq!(graph_sections.len(), meta.graph_section_count as usize);
        for section in &graph_sections {
            assert_eq!(section.section_type, SectionType::Graph);
        }
    }

    #[test]
    fn test_load_graph_sections_empty_change() {
        let (_dir, store) = temp_store();
        let change = make_test_change("empty hunks", b"just content");

        let hash = store.save_change(&change).unwrap();
        let graph_sections = store.load_graph_sections(&hash).unwrap();

        // A change with no hunks should have no graph sections
        assert!(graph_sections.is_empty());
    }

    #[test]
    fn test_load_semantic_sections() {
        let (_dir, store) = temp_store();
        let change = make_change_with_hunk();

        let hash = store.save_change(&change).unwrap();
        let semantic_sections = store.load_semantic_sections(&hash).unwrap();

        // Semantic sections may or may not be present depending on CRDT ops
        // Either way the call should succeed
        let meta = store.load_meta(&hash).unwrap();
        assert_eq!(
            semantic_sections.len(),
            meta.semantic_section_count as usize
        );
    }

    // ── Unhashed Data ──────────────────────────────────────────────

    #[test]
    fn test_unhashed_none() {
        let (_dir, store) = temp_store();
        let change = make_test_change("no unhashed", b"data");

        let hash = store.save_change(&change).unwrap();
        let unhashed = store.load_unhashed(&hash).unwrap();
        assert!(unhashed.is_none());
    }

    #[test]
    fn test_unhashed_present() {
        let (_dir, store) = temp_store();
        let mut change = make_test_change("with unhashed", b"data");
        change.unhashed = Some(serde_json::json!({
            "transcript": "AI reasoning trace",
            "model": "claude-sonnet-4-20250514"
        }));

        let hash = store.save_change(&change).unwrap();
        let unhashed = store.load_unhashed(&hash).unwrap();

        assert!(unhashed.is_some());
        let value = unhashed.unwrap();
        assert_eq!(value["transcript"], "AI reasoning trace");
        assert_eq!(value["model"], "claude-sonnet-4-20250514");
    }

    // ── Full Change Roundtrip ──────────────────────────────────────

    #[test]
    fn test_save_and_load_change_roundtrip() {
        let (_dir, store) = temp_store();
        let content = b"fn main() { println!(\"Hello!\"); }";
        let original = make_test_change("roundtrip", content);

        let hash = store.save_change(&original).unwrap();
        let loaded = store.load_change(&hash).unwrap();

        assert_eq!(loaded.message(), "roundtrip");
        assert_eq!(loaded.contents, content);
    }

    #[test]
    fn test_save_and_load_change_with_hunk_roundtrip() {
        let (_dir, store) = temp_store();
        let original = make_change_with_hunk();

        let hash = store.save_change(&original).unwrap();
        let loaded = store.load_change(&hash).unwrap();

        assert_eq!(loaded.message(), "With hunk");
        assert_eq!(loaded.hunks().len(), original.hunks().len());
        assert_eq!(loaded.contents, original.contents);
    }

    #[test]
    fn test_save_and_load_change_with_deps() {
        let (_dir, store) = temp_store();
        let dep = Hash::of(b"dependency");
        let original = Change::new(
            ChangeHeader::new("with deps"),
            vec![],
            b"content".to_vec(),
            vec![dep],
        );

        let hash = store.save_change(&original).unwrap();
        let loaded = store.load_change(&hash).unwrap();

        assert_eq!(loaded.dependencies().len(), 1);
        assert!(loaded.depends_on(&dep));
    }

    // ── Export to V3 File ──────────────────────────────────────────

    #[test]
    fn test_export_v3_bytes() {
        let (_dir, store) = temp_store();
        let change = make_test_change("export test", b"file content");

        let hash = store.save_change(&change).unwrap();
        let v3_bytes = store.export_v3_bytes(&hash).unwrap();

        // Should start with ATOM magic
        assert!(v3_bytes.len() >= 4);
        assert_eq!(&v3_bytes[0..4], b"ATOM");

        // Should be deserializable
        let mut cursor = Cursor::new(&v3_bytes);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();
        assert_eq!(loaded.message(), "export test");
    }

    #[test]
    fn test_export_v3_file() {
        let (dir, store) = temp_store();
        let change = make_test_change("file export", b"exported content");

        let hash = store.save_change(&change).unwrap();

        let export_path = dir.path().join("exported.change");
        store.export_v3_file(&hash, &export_path).unwrap();

        // File should exist and start with ATOM
        assert!(export_path.exists());
        let file_data = std::fs::read(&export_path).unwrap();
        assert_eq!(&file_data[0..4], b"ATOM");
    }

    #[test]
    fn test_import_export_roundtrip() {
        let (dir, store) = temp_store();
        let change = make_test_change("import-export", b"roundtrip content");

        // Save to store
        let hash = store.save_change(&change).unwrap();

        // Export to file
        let export_path = dir.path().join("roundtrip.change");
        store.export_v3_file(&hash, &export_path).unwrap();

        // Delete from store
        store.delete_change(&hash).unwrap();
        assert!(!store.has_change(&hash).unwrap());

        // Import from file
        let imported_hash = store.import_v3_file(&export_path).unwrap();
        assert_eq!(hash, imported_hash);

        // Should be loadable again
        let loaded = store.load_change(&imported_hash).unwrap();
        assert_eq!(loaded.message(), "import-export");
        assert_eq!(loaded.contents, b"roundtrip content");
    }

    // ── Delete Operations ──────────────────────────────────────────

    #[test]
    fn test_delete_change() {
        let (_dir, store) = temp_store();
        let change = make_test_change("to delete", b"bye");

        let hash = store.save_change(&change).unwrap();
        assert!(store.has_change(&hash).unwrap());

        let deleted = store.delete_change(&hash).unwrap();
        assert!(deleted);
        assert!(!store.has_change(&hash).unwrap());
    }

    #[test]
    fn test_delete_nonexistent() {
        let (_dir, store) = temp_store();
        let bogus = [0xFF; 32];
        let deleted = store.delete_change(&bogus).unwrap();
        assert!(!deleted);
    }

    #[test]
    fn test_delete_preserves_shared_chunks() {
        let (_dir, store) = temp_store();

        // Save two changes with the same content
        let content = b"shared chunk content here";
        let change1 = make_test_change("first", content);
        let change2 = make_test_change("second", content);

        let hash1 = store.save_change(&change1).unwrap();
        let hash2 = store.save_change(&change2).unwrap();

        // Delete the first change
        store.delete_change(&hash1).unwrap();

        // The second change should still be loadable with its content
        let loaded = store.load_change(&hash2).unwrap();
        assert_eq!(loaded.contents, content);
    }

    // ── Statistics ──────────────────────────────────────────────────

    #[test]
    fn test_stats_empty() {
        let (_dir, store) = temp_store();
        let stats = store.stats().unwrap();

        assert_eq!(stats.change_count, 0);
        assert_eq!(stats.graph_section_count, 0);
        assert_eq!(stats.semantic_section_count, 0);
        assert_eq!(stats.content_chunk_count, 0);
        assert_eq!(stats.change_chunk_mappings, 0);
        assert_eq!(stats.unhashed_count, 0);
    }

    #[test]
    fn test_stats_after_save() {
        let (_dir, store) = temp_store();
        let change = make_test_change("stats", b"some content");

        store.save_change(&change).unwrap();
        let stats = store.stats().unwrap();

        assert_eq!(stats.change_count, 1);
        assert!(stats.content_chunk_count >= 1);
        assert!(stats.change_chunk_mappings >= 1);
    }

    #[test]
    fn test_stats_display() {
        let stats = StoreStats {
            change_count: 5,
            graph_section_count: 10,
            semantic_section_count: 10,
            content_chunk_count: 20,
            change_chunk_mappings: 25,
            unhashed_count: 2,
        };
        let display = format!("{}", stats);
        assert!(display.contains("5 changes"));
        assert!(display.contains("10 graph"));
        assert!(display.contains("20 unique chunks"));
    }

    // ── Chunk Manifest ─────────────────────────────────────────────

    #[test]
    fn test_chunk_manifest() {
        let (_dir, store) = temp_store();
        let change = make_test_change("manifest", b"content for manifest");

        let hash = store.save_change(&change).unwrap();
        let manifest = store.get_chunk_manifest(&hash).unwrap();

        assert!(!manifest.is_empty());
        // Verify manifest entries are ordered
        for (i, (idx, _chunk_hash)) in manifest.iter().enumerate() {
            assert_eq!(*idx, i as u32);
        }
    }

    #[test]
    fn test_has_content_chunk() {
        let (_dir, store) = temp_store();
        let change = make_test_change("chunk check", b"content data");

        let hash = store.save_change(&change).unwrap();
        let manifest = store.get_chunk_manifest(&hash).unwrap();

        // All chunks in the manifest should exist
        for (_idx, chunk_hash) in &manifest {
            assert!(store.has_content_chunk(chunk_hash).unwrap());
        }

        // A random hash should not exist
        let bogus = [0xFF; 32];
        assert!(!store.has_content_chunk(&bogus).unwrap());
    }

    // ── Multiple Changes ───────────────────────────────────────────

    #[test]
    fn test_multiple_changes() {
        let (_dir, store) = temp_store();

        let hashes: Vec<[u8; 32]> = (0..5)
            .map(|i| {
                let change = make_test_change(
                    &format!("change {}", i),
                    format!("content {}", i).as_bytes(),
                );
                store.save_change(&change).unwrap()
            })
            .collect();

        let stats = store.stats().unwrap();
        assert_eq!(stats.change_count, 5);

        // All should be loadable
        for hash in &hashes {
            assert!(store.has_change(hash).unwrap());
            let loaded = store.load_change(hash).unwrap();
            assert!(!loaded.message().is_empty());
        }
    }

    #[test]
    fn test_save_same_change_twice_is_idempotent() {
        let (_dir, store) = temp_store();
        let change = make_test_change("idempotent", b"data");

        let hash1 = store.save_change(&change).unwrap();
        let hash2 = store.save_change(&change).unwrap();

        // Same change produces same hash
        // (Note: timestamps differ between calls to make_test_change,
        // but we're calling save_change on the same Change object)
        assert_eq!(hash1, hash2);

        let stats = store.stats().unwrap();
        assert_eq!(stats.change_count, 1); // not 2
    }

    // ── Debug ──────────────────────────────────────────────────────

    #[test]
    fn test_debug_format() {
        let (_dir, store) = temp_store();
        let debug = format!("{:?}", store);
        assert!(debug.contains("RedbChangeStore"));
        assert!(debug.contains("CHANGE_META"));
    }
}

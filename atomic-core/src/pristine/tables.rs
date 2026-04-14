//! Table definitions for the pristine database
//!
//! This module defines all the redb tables used to store the repository graph
//! and metadata. Tables are organized by function:
//!
//! - **ID Mappings**: `EXTERNAL`, `INTERNAL` - map between internal NodeIds and external Hashes
//! - **Graph**: `GRAPH`, `INODE_GRAPH` - store vertices and edges
//! - **Views**: `VIEWS`, `VIEW_CHANGES`, `REV_VIEW_CHANGES` - view metadata
//! - **File Tree**: `TREE`, `REV_TREE`, `INODES`, `REV_INODES`, `DIRECTORIES` - file system mappings
//! - **Dependencies**: `DEPS`, `REV_DEPS` - change dependency tracking
//! - **State**: `STATES`, `TAGS` - view state and tag tracking

use redb::{MultimapTableDefinition, TableDefinition};

// ID Mapping Tables

/// Maps NodeId (u64) → Hash ([u8; 32])
///
/// Converts internal repository-local IDs to external content-addressed hashes.
/// Used when serializing changes or communicating with remotes.
pub const EXTERNAL: TableDefinition<u64, &[u8; 32]> = TableDefinition::new("external");

/// Maps Hash ([u8; 32]) → NodeId (u64)
///
/// Converts external hashes to internal IDs.
/// Used when applying changes from remotes or loading from disk.
pub const INTERNAL: TableDefinition<&[u8; 32], u64> = TableDefinition::new("internal");

/// Maps NodeId → NodeType (u8)
///
/// Tracks what type of node each ID represents:
/// - 0: Change
/// - 1: Tag
pub const NODE_TYPES: TableDefinition<u64, u8> = TableDefinition::new("node_types");

/// Node type constants
pub mod node_type {
    pub const CHANGE: u8 = 0;
    pub const TAG: u8 = 1;
    pub const ATTESTATION: u8 = 2;
    pub const PROVENANCE: u8 = 3;
}

// Graph Tables

/// Main graph table: GraphNode → `GraphEdge` (multimap)
///
/// Key: 24 bytes encoding (change_id: u64, start: u64, end: u64)
/// Value: 24 bytes encoding SerializedGraphEdge
///
/// This is the primary graph storage. Each span can have multiple outgoing edges.
pub const GRAPH: MultimapTableDefinition<&[u8; 24], &[u8; 24]> =
    MultimapTableDefinition::new("graph");

/// Inode-scoped graph for O(n) file traversal
///
/// Key: 32 bytes encoding (inode: u64, change_id: u64, start: u64, end: u64)
/// Value: 24 bytes encoding SerializedGraphEdge
///
/// This secondary index allows efficient iteration over all vertices belonging
/// to a specific file (inode), enabling O(n) file reads where n is the file size.
pub const INODE_GRAPH: MultimapTableDefinition<&[u8; 32], &[u8; 24]> =
    MultimapTableDefinition::new("inode_graph");

// View Tables

/// View metadata
///
/// Key: view name (string)
/// Value: serialized ViewState (variable length)
///
/// Stores view configuration and current state (merkle root, head change, etc.)
/// Views are perspectives of the graph - they represent which changes have been applied
/// in what order, not forks of the underlying data.
pub const VIEWS: TableDefinition<&str, &[u8]> = TableDefinition::new("views");

/// View change log: (view_id, sequence) → change_id
///
/// Key: 16 bytes encoding (view_id: u64, sequence: u64)
/// Value: change NodeId
///
/// Tracks the ordered sequence of changes applied to each view.
pub const VIEW_CHANGES: TableDefinition<&[u8; 16], u64> = TableDefinition::new("view_changes");

/// Reverse view change log: (view_id, change_id) → sequence
///
/// Key: 16 bytes encoding (view_id: u64, change_id: u64)
/// Value: sequence number
///
/// Allows looking up when a change was applied to a view.
pub const REV_VIEW_CHANGES: TableDefinition<&[u8; 16], u64> =
    TableDefinition::new("rev_view_changes");

// File Tree Tables

/// File tree: path → inode
///
/// Key: file path (string, relative to repo root)
/// Value: inode ID
///
/// Maps file paths to their stable inode identifiers.
pub const TREE: TableDefinition<&str, u64> = TableDefinition::new("tree");

/// Reverse tree: inode → path
///
/// Key: inode ID
/// Value: file path (string)
///
/// Maps inodes back to their current path.
pub const REV_TREE: TableDefinition<u64, &str> = TableDefinition::new("rev_tree");

/// Inodes: inode → Position
///
/// Key: inode ID
/// Value: 16 bytes encoding Position (change_id: u64, pos: u64)
///
/// Maps inodes to their position in the graph (the root span of the file).
pub const INODES: TableDefinition<u64, &[u8; 16]> = TableDefinition::new("inodes");

/// Reverse inodes: Position → inode
///
/// Key: 16 bytes encoding Position
/// Value: inode ID
///
/// Maps graph positions back to their inode.
pub const REV_INODES: TableDefinition<&[u8; 16], u64> = TableDefinition::new("rev_inodes");

/// Directories marker table: inode → flags
///
/// Key: inode ID
/// Value: directory flags (u8)
///
/// Marks which inodes represent directories rather than files. This enables
/// explicit directory tracking as first-class citizens in the repository.
///
/// # Directory Flags
///
/// | Flag | Value | Description |
/// |------|-------|-------------|
/// | `DIR_EXPLICIT` | 0x01 | Directory was explicitly tracked |
/// | `DIR_EMPTY` | 0x02 | Directory has no tracked children |
///
/// # Why Track Directories?
///
/// Unlike Git (which uses `.keep` files), Atomic tracks directories explicitly:
///
/// 1. **Empty directories** can be versioned without workarounds
/// 2. **Directory metadata** (permissions, ownership) can be preserved
/// 3. **Cleaner graph** - no synthetic files cluttering the repository
/// 4. **Semantic correctness** - a directory IS a directory, not a file
///
/// # Example
///
/// ```text
/// ┌─────────────────────────────────────────────────────────────────────┐
/// │                    Directory Tracking Flow                          │
/// ├─────────────────────────────────────────────────────────────────────┤
/// │                                                                     │
/// │  $ atomic add --directory src/empty_module/                         │
/// │                                                                     │
/// │  TREE[src/empty_module] = 42                                        │
/// │  REV_TREE[42] = src/empty_module                                    │
/// │  DIRECTORIES[42] = DIR_EXPLICIT | DIR_EMPTY  (0x03)                │
/// │                                                                     │
/// │  When recording:                                                    │
/// │  → Creates GraphOp::DirAdd with FOLDER edge flag                      │
/// │  → Directory becomes first-class graph citizen                      │
/// │                                                                     │
/// └─────────────────────────────────────────────────────────────────────┘
/// ```
pub const DIRECTORIES: TableDefinition<u64, u8> = TableDefinition::new("directories");

/// Directory flag constants
pub mod directory_flags {
    /// Directory was explicitly tracked (not just implicitly via file paths).
    ///
    /// When set, this directory will be included in changes even if empty.
    pub const DIR_EXPLICIT: u8 = 0x01;

    /// Directory currently has no tracked children.
    ///
    /// This flag is updated dynamically as files are added/removed.
    pub const DIR_EMPTY: u8 = 0x02;

    /// Check if flags indicate an explicitly tracked directory.
    #[inline]
    pub const fn is_explicit(flags: u8) -> bool {
        flags & DIR_EXPLICIT != 0
    }

    /// Check if flags indicate an empty directory.
    #[inline]
    pub const fn is_empty(flags: u8) -> bool {
        flags & DIR_EMPTY != 0
    }

    /// Create flags for an explicitly tracked empty directory.
    #[inline]
    pub const fn explicit_empty() -> u8 {
        DIR_EXPLICIT | DIR_EMPTY
    }

    /// Create flags for an explicitly tracked non-empty directory.
    #[inline]
    pub const fn explicit_with_children() -> u8 {
        DIR_EXPLICIT
    }
}

// Dependency Tables

/// Dependencies: change_id → `dep_id` (multimap)
///
/// Key: change NodeId
/// Value: dependency NodeId
///
/// Tracks which changes a given change depends on.
pub const DEPS: MultimapTableDefinition<u64, u64> = MultimapTableDefinition::new("deps");

/// Reverse dependencies: dep_id → `change_id` (multimap)
///
/// Key: dependency NodeId
/// Value: dependent change NodeId
///
/// Tracks which changes depend on a given change (reverse of DEPS).
pub const REV_DEPS: MultimapTableDefinition<u64, u64> = MultimapTableDefinition::new("rev_deps");

// State Tables

/// View states: (view_id, merkle) → sequence
///
/// Key: 40 bytes encoding (view_id: u64, merkle: [u8; 32])
/// Value: sequence number when this state was reached
///
/// Allows looking up when a view reached a particular state.
pub const STATES: TableDefinition<&[u8; 40], u64> = TableDefinition::new("states");

/// View tags: (view_id, sequence) → merkle
///
/// Key: 16 bytes encoding (view_id: u64, sequence: u64)
/// Value: merkle hash at that sequence
///
/// Stores tagged states (named snapshots) for views.
pub const TAGS: TableDefinition<&[u8; 16], &[u8; 32]> = TableDefinition::new("tags");

// File Mtime Cache Table
//
// Stores the last-recorded filesystem modification time for each tracked file.
// During `status()`, if a file's current mtime matches the cached value, we
// skip the expensive graph content reconstruction + comparison entirely.
//
// This is the same optimization Git uses (stat data in the index). For a
// repository with 264 files, it reduces incremental `status()` from ~10s
// (full graph traversal for every file) to ~50ms (stat-only for unchanged files).
//
// The cache is invalidated per-file whenever:
//   - A change is recorded that modifies the file (mtime updated to post-record value)
//   - A change is applied that modifies the file (mtime updated after output)
//   - The user runs `atomic reset` (cache cleared)
//
// If the mtime check is inconclusive (e.g., mtime resolution is too coarse,
// or the file was modified and reverted within the same second), we fall back
// to the full content comparison. This means false negatives (showing Clean
// when Modified) are impossible — we only skip the comparison when we're
// confident the file hasn't changed.

/// File mtime cache: path → (mtime_secs, mtime_nanos, file_size)
///
/// Key: file path (string, relative to repo root)
/// Value: 20 bytes encoding (mtime_secs: i64, mtime_nanos: u32, file_size: u64)
///
/// Stores the filesystem metadata snapshot taken at the time the file was
/// last recorded or applied. During status, if `stat()` returns the same
/// values, we know the file hasn't changed and skip content comparison.
///
/// The combination of (mtime, size) catches virtually all modifications:
/// - Content edits change mtime (and usually size)
/// - Truncation changes size
/// - `touch` changes mtime
///
/// When mtime+size don't match, the stored content hash is compared with
/// the on-disk hash to detect true modifications without reconstructing
/// graph content.
pub const FILE_INDEX: TableDefinition<&str, &[u8; 52]> = TableDefinition::new("file_index");

/// Encode file metadata for the FILE_INDEX table.
///
/// Packs (mtime_secs, mtime_nanos, file_size, content_hash) into 52 bytes.
#[inline]
pub fn encode_file_index(
    mtime_secs: i64,
    mtime_nanos: u32,
    file_size: u64,
    content_hash: &crate::types::Hash,
) -> [u8; 52] {
    let mut value = [0u8; 52];
    value[0..8].copy_from_slice(&mtime_secs.to_le_bytes());
    value[8..12].copy_from_slice(&mtime_nanos.to_le_bytes());
    value[12..20].copy_from_slice(&file_size.to_le_bytes());
    value[20..52].copy_from_slice(&content_hash.0);
    value
}

/// Decode file metadata from the FILE_INDEX table.
///
/// Returns (mtime_secs, mtime_nanos, file_size, content_hash).
#[inline]
pub fn decode_file_index(value: &[u8; 52]) -> (i64, u32, u64, crate::types::Hash) {
    let mtime_secs = i64::from_le_bytes(value[0..8].try_into().unwrap());
    let mtime_nanos = u32::from_le_bytes(value[8..12].try_into().unwrap());
    let file_size = u64::from_le_bytes(value[12..20].try_into().unwrap());
    let mut hash_bytes = [0u8; 32];
    hash_bytes.copy_from_slice(&value[20..52]);
    (
        mtime_secs,
        mtime_nanos,
        file_size,
        crate::types::Merkle(hash_bytes),
    )
}

// V3 Change Storage Tables
//
// These tables store change data directly in redb, eliminating .change files
// as the primary storage format. The V3 section-based format maps naturally
// to per-section redb values:
//
//   CHANGE_META:     Header + deps + provenance + hash table (one blob per change)
//   CHANGE_GRAPH:    Per-file graph ops (one blob per file per change)
//   CHANGE_SEMANTIC: Per-file semantic ops (one blob per file per change)
//   CONTENT_CHUNKS:  Content-addressed chunks (shared across changes via hash)
//   CHANGE_CHUNKS:   Maps a change to its ordered list of content chunk hashes
//   CHANGE_UNHASHED: Unhashed metadata (AI transcripts, reasoning, etc.)
//
// Benefits over .change files:
//   - No serialization overhead (values go in/out as compressed blobs)
//   - Automatic caching (redb page cache handles hot changes)
//   - Transactional (change storage participates in same ACID txn as graph apply)
//   - Random access (read one file's graph ops without loading everything)
//   - Layer-selective (code review reads CHANGE_SEMANTIC only, apply reads CHANGE_GRAPH only)

/// Change metadata: hash → zstd(postcard(ChangeHeader + deps + provenance + hash_table))
///
/// Key: change content hash (blake3, 32 bytes)
/// Value: compressed blob containing all metadata sections
///
/// This is the first thing read when loading a change. It contains:
/// - ChangeHeader (message, authors, timestamp)
/// - Dependency hash indices
/// - Provenance entries (AI attribution)
/// - The hash dedup table (for resolving HashIndex → full hash)
///
/// Metadata is always loaded regardless of which layers are requested,
/// because it's small and universally needed.
pub const CHANGE_META: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("change_meta");

/// Per-file graph operations: (change_hash, file_index) → zstd(postcard(GraphSectionPayload))
///
/// Key: 36 bytes encoding (change_hash: [u8; 32], file_index: u32)
/// Value: compressed blob containing CompactGraphOps for one file
///
/// Each modified file in a change gets one entry. The file_index preserves
/// the ordering from the original change file. The value is a compressed
/// `GraphSectionPayload` containing:
/// - File path
/// - Compact graph operations (using HashIndex instead of full hashes)
/// - Content byte range
///
/// Layer-selective reads: `apply` reads only this table + CONTENT_CHUNKS.
/// Code review skips this table entirely.
pub const CHANGE_GRAPH: TableDefinition<&[u8; 36], &[u8]> = TableDefinition::new("change_graph");

/// Per-file semantic operations: (change_hash, file_index) → zstd(postcard(SemanticSectionPayload))
///
/// Key: 36 bytes encoding (change_hash: [u8; 32], file_index: u32)
/// Value: compressed blob containing FileOps (Trunk/Branch/Leaf) for one file
///
/// Each modified file can have one semantic entry containing the
/// human-readable operations for diffs, blame, and code review.
///
/// Semantic sections are optional — a "thin pull" skips them entirely.
/// They can be regenerated from graph + content at any time.
///
/// Layer-selective reads: code review reads only this table + CONTENT_CHUNKS.
/// `apply` skips this table entirely.
pub const CHANGE_SEMANTIC: TableDefinition<&[u8; 36], &[u8]> =
    TableDefinition::new("change_semantic");

/// Content-addressed chunks: chunk_blake3_hash → zstd(raw content bytes)
///
/// Key: blake3 hash of the **uncompressed** chunk data (32 bytes)
/// Value: zstd-compressed chunk content
///
/// Chunks are keyed by CONTENT hash, not change hash. This enables:
/// - **Cross-change deduplication**: identical content regions (renames, copies,
///   reverts) are stored once regardless of how many changes reference them.
/// - **Delta transfer**: during push/pull, only chunks the receiver doesn't
///   have need to be transferred.
/// - **Efficient storage**: a 1-line edit to a 10 MB file shares ~99% of
///   chunks with the previous version.
///
/// Chunk sizes are determined by FastCDC content-defined chunking:
/// - Minimum: 16 KB, Average: 64 KB, Maximum: 256 KB
pub const CONTENT_CHUNKS: TableDefinition<&[u8; 32], &[u8]> =
    TableDefinition::new("content_chunks");

/// Change chunk manifest: (change_hash, chunk_index) → chunk_content_hash
///
/// Key: 36 bytes encoding (change_hash: [u8; 32], chunk_index: u32)
/// Value: blake3 hash of the chunk content (reference into CONTENT_CHUNKS)
///
/// Maps a change to its ordered list of content chunks. To reconstruct
/// the full content blob for a change:
/// 1. Iterate CHANGE_CHUNKS for the change hash (ordered by chunk_index)
/// 2. For each chunk_content_hash, look up CONTENT_CHUNKS to get the data
/// 3. Decompress and concatenate in order
///
/// This indirection enables chunk sharing across changes — multiple changes
/// can reference the same chunk by its content hash.
pub const CHANGE_CHUNKS: TableDefinition<&[u8; 36], &[u8; 32]> =
    TableDefinition::new("change_chunks");

/// Unhashed metadata: change_hash → zstd(json(Value))
///
/// Key: change content hash (blake3, 32 bytes)
/// Value: compressed JSON blob
///
/// Stores data that doesn't affect the change's identity:
/// - AI transcripts and reasoning traces
/// - Editor state and review comments
/// - Session metadata
///
/// This data is explicitly excluded from the content hash.
pub const CHANGE_UNHASHED: TableDefinition<&[u8; 32], &[u8]> =
    TableDefinition::new("change_unhashed");

// Session Tables (Sherpa-enriched provenance data)
//
// Populated when `ProvenanceGraph.profile == "sherpa-trace/1.0.0"`.
// These tables store the structured turn data extracted from the Petri net
// replay log. Values are postcard-encoded session structs from
// `atomic_core::change::session`.
//
// Composite keys use the same `[u8; 16]` shape as `VIEW_CHANGES`:
// first 8 bytes = provenance_id (u64, big-endian), second 8 bytes = secondary
// component (u64, big-endian: seq, or first-8-bytes-of-blake3 for string IDs).

/// Session events: (provenance_id, seq) → SessionEvent
///
/// The full ordered Petri net replay log for a turn. Each row is one
/// NetEvent from the JSONL trace file. Keys are composite `[u8; 16]`
/// values `(provenance_id_be, seq_be)` as produced by
/// `atomic_core::change::session::encode_session_*`. They are ordered
/// lexicographically by their big-endian byte representation, so the
/// key order matches numeric order: first by `provenance_id`, then by
/// `seq` within each provenance, enabling efficient prefix and range scans.
pub const SESSION_EVENTS: TableDefinition<&[u8; 16], &[u8]> =
    TableDefinition::new("session_events");

/// Session todos: (provenance_id, todo_id_hash) → TodoSnapshot
///
/// Final state of each todo item at the end of the turn. The `todo_id`
/// string is hashed to a fixed-size key (first 8 bytes of Blake3).
pub const SESSION_TODOS: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("session_todos");

/// Session phases: (provenance_id, phase_hash) → PhaseTimingEntry
///
/// Timing breakdown by phase for the turn. The phase name is hashed to
/// a fixed-size key (first 8 bytes of Blake3).
pub const SESSION_PHASES: TableDefinition<&[u8; 16], &[u8]> =
    TableDefinition::new("session_phases");

/// Session intents: provenance_id → IntentEntry
///
/// Intent metadata for the turn. One entry per provenance graph.
pub const SESSION_INTENTS: TableDefinition<u64, &[u8]> = TableDefinition::new("session_intents");

// Vault Tables (shared project knowledge store)
//
// The vault stores project knowledge (sessions, memory, skills, intents)
// in redb. Markdown files in `.vault/` are materialized views of
// this data. See `atomic-core/src/pristine/vault.rs` for type definitions.

/// Vault entries: vault_path → VaultEntry (serialized)
///
/// Key: vault-relative path (e.g. "sessions/abc123/_session.md")
/// Value: postcard-serialized VaultEntry (metadata + content bytes)
///
/// This is the canonical store for all vault content. The markdown files
/// on disk are generated from these entries.
pub const VAULT_ENTRIES: TableDefinition<&str, &[u8]> = TableDefinition::new("vault_entries");

/// Vault manifest: singleton key → JSON blob
///
/// Key: always "manifest"
/// Value: JSON-serialized VaultManifest
///
/// The manifest indexes the entire vault for cold-start, sync, and selective
/// fetch. Updated atomically with every vault write.
pub const VAULT_MANIFEST: TableDefinition<&str, &[u8]> = TableDefinition::new("vault_manifest");

// Knowledge Graph Tables
//
// Stores nodes and edges for the full repository knowledge graph:
// changes, files, entities, views, vault (goals, intents, memory).
// Replaces the earlier triple-only store with richer typed nodes.

/// KG nodes: node_id → KgNode (serialized)
///
/// Key: structured node ID (e.g., "change:abc123", "file:src/auth.rs")
/// Value: postcard-serialized KgNode
pub const KG_NODES: TableDefinition<&str, &[u8]> = TableDefinition::new("kg_nodes");

/// KG edges: composite key → KgEdge (serialized)
///
/// Key: "from_id\0to_id\0kind" (unique edge identifier)
/// Value: postcard-serialized KgEdge
pub const KG_EDGES: TableDefinition<&str, &[u8]> = TableDefinition::new("kg_edges");

/// KG edge index by source: from_id → [edge_key] (multimap)
///
/// Enables efficient "outgoing edges from node X" queries.
pub const KG_EDGES_FROM: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("kg_edges_from");

/// KG edge index by target: to_id → [edge_key] (multimap)
///
/// Enables efficient "incoming edges to node X" queries.
pub const KG_EDGES_TO: MultimapTableDefinition<&str, &str> =
    MultimapTableDefinition::new("kg_edges_to");

/// KG full-text search inverted index: token → [node_id] (multimap)
///
/// Simple inverted index for keyword search over node labels and summaries.
/// Tokens are lowercase, alphanumeric words extracted from label + summary.
pub const KG_FTS: MultimapTableDefinition<&str, &str> = MultimapTableDefinition::new("kg_fts");

// Embedding Tables (vector similarity search)

/// Vector embeddings: vault_path → EmbeddingRecord (serialized)
///
/// Key: composite string "vault_path\0chunk_idx" (e.g., "memory/arch.md\00")
/// Value: postcard-serialized EmbeddingRecord
///
/// Stores embeddings for semantic similarity search over vault content.
pub const EMBEDDINGS: TableDefinition<&str, &[u8]> = TableDefinition::new("embeddings");

// KG Edge Key / FTS Helpers

/// Encode a KG edge key as "from_id\0to_id\0kind".
#[inline]
pub fn encode_edge_key(from_id: &str, to_id: &str, kind: &str) -> String {
    format!("{}\0{}\0{}", from_id, to_id, kind)
}

/// Decode a KG edge key into (from_id, to_id, kind).
#[inline]
pub fn decode_edge_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut parts = key.splitn(3, '\0');
    let from_id = parts.next()?;
    let to_id = parts.next()?;
    let kind = parts.next()?;
    Some((from_id, to_id, kind))
}

/// Tokenize text for the FTS inverted index.
///
/// Extracts lowercase alphanumeric words of length >= 2.
pub fn tokenize_for_fts(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_lowercase())
        .collect()
}

/// Encode an embedding key as "path\0chunk_idx".
#[inline]
pub fn encode_embedding_key(path: &str, chunk_idx: u32) -> String {
    format!("{}\0{}", path, chunk_idx)
}

/// Decode an embedding key into (path, chunk_idx).
#[inline]
pub fn decode_embedding_key(key: &str) -> Option<(&str, u32)> {
    let (path, idx_str) = key.split_once('\0')?;
    let idx = idx_str.parse().ok()?;
    Some((path, idx))
}

// V3 Change Storage Key Encoding

/// Encode a change-hash + file-index as 36 bytes for CHANGE_GRAPH / CHANGE_SEMANTIC / CHANGE_CHUNKS keys.
///
/// Layout: `[change_hash: 32 bytes][file_index: 4 bytes LE]`
#[inline]
pub fn encode_change_file_key(change_hash: &[u8; 32], file_index: u32) -> [u8; 36] {
    let mut key = [0u8; 36];
    key[0..32].copy_from_slice(change_hash);
    key[32..36].copy_from_slice(&file_index.to_le_bytes());
    key
}

/// Decode a change-hash + file-index from 36 bytes.
///
/// Returns `(change_hash, file_index)`.
#[inline]
pub fn decode_change_file_key(key: &[u8; 36]) -> ([u8; 32], u32) {
    let mut change_hash = [0u8; 32];
    change_hash.copy_from_slice(&key[0..32]);
    let file_index = u32::from_le_bytes([key[32], key[33], key[34], key[35]]);
    (change_hash, file_index)
}

// Key Encoding Helpers

/// Encode a span as 24 bytes for use as a graph key
#[inline]
pub fn encode_vertex(change_id: u64, start: u64, end: u64) -> [u8; 24] {
    let mut key = [0u8; 24];
    key[0..8].copy_from_slice(&change_id.to_be_bytes());
    key[8..16].copy_from_slice(&start.to_be_bytes());
    key[16..24].copy_from_slice(&end.to_be_bytes());
    key
}

/// Decode a span from 24 bytes
#[inline]
pub fn decode_vertex(key: &[u8; 24]) -> (u64, u64, u64) {
    let change_id = u64::from_be_bytes(key[0..8].try_into().unwrap());
    let start = u64::from_be_bytes(key[8..16].try_into().unwrap());
    let end = u64::from_be_bytes(key[16..24].try_into().unwrap());
    (change_id, start, end)
}

/// Encode an inode-span as 32 bytes for use as an inode_graph key
#[inline]
pub fn encode_inode_vertex(inode: u64, change_id: u64, start: u64, end: u64) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[0..8].copy_from_slice(&inode.to_be_bytes());
    bytes[8..16].copy_from_slice(&change_id.to_be_bytes());
    bytes[16..24].copy_from_slice(&start.to_be_bytes());
    bytes[24..32].copy_from_slice(&end.to_be_bytes());
    bytes
}

/// Decode an inode-span from 32 bytes
#[inline]
pub fn decode_inode_vertex(key: &[u8; 32]) -> (u64, u64, u64, u64) {
    let inode = u64::from_be_bytes(key[0..8].try_into().unwrap());
    let change_id = u64::from_be_bytes(key[8..16].try_into().unwrap());
    let start = u64::from_be_bytes(key[16..24].try_into().unwrap());
    let end = u64::from_be_bytes(key[24..32].try_into().unwrap());
    (inode, change_id, start, end)
}

/// Encode a position as 16 bytes
#[inline]
pub fn encode_position(change_id: u64, pos: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&change_id.to_be_bytes());
    key[8..16].copy_from_slice(&pos.to_be_bytes());
    key
}

/// Decode a position from 16 bytes
#[inline]
pub fn decode_position(key: &[u8; 16]) -> (u64, u64) {
    let change_id = u64::from_be_bytes(key[0..8].try_into().unwrap());
    let pos = u64::from_be_bytes(key[8..16].try_into().unwrap());
    (change_id, pos)
}

/// Encode a view-sequence pair as 16 bytes
#[inline]
pub fn encode_view_seq(view_id: u64, seq: u64) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[0..8].copy_from_slice(&view_id.to_be_bytes());
    key[8..16].copy_from_slice(&seq.to_be_bytes());
    key
}

/// Decode a view-sequence pair from 16 bytes
#[inline]
pub fn decode_view_seq(key: &[u8; 16]) -> (u64, u64) {
    let view_id = u64::from_be_bytes(key[0..8].try_into().unwrap());
    let seq = u64::from_be_bytes(key[8..16].try_into().unwrap());
    (view_id, seq)
}

/// Encode a view-merkle pair as 40 bytes
#[inline]
pub fn encode_view_merkle(view_id: u64, merkle: &[u8; 32]) -> [u8; 40] {
    let mut key = [0u8; 40];
    key[0..8].copy_from_slice(&view_id.to_be_bytes());
    key[8..40].copy_from_slice(merkle);
    key
}

/// Decode a view-merkle pair from 40 bytes
#[inline]
pub fn decode_view_merkle(key: &[u8; 40]) -> (u64, [u8; 32]) {
    let view_id = u64::from_be_bytes(key[0..8].try_into().unwrap());
    let mut merkle = [0u8; 32];
    merkle.copy_from_slice(&key[8..40]);
    (view_id, merkle)
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn test_vertex_encoding_roundtrip() {
        let change_id = 12345u64;
        let start = 100u64;
        let end = 200u64;

        let encoded = encode_vertex(change_id, start, end);
        let (dec_change, dec_start, dec_end) = decode_vertex(&encoded);

        assert_eq!(dec_change, change_id);
        assert_eq!(dec_start, start);
        assert_eq!(dec_end, end);
    }

    #[test]
    fn test_inode_vertex_encoding_roundtrip() {
        let inode = 42u64;
        let change_id = 100u64;
        let start = 200u64;
        let end = 300u64;

        let encoded = encode_inode_vertex(inode, change_id, start, end);
        let (d_inode, d_change, d_start, d_end) = decode_inode_vertex(&encoded);

        assert_eq!(d_inode, inode);
        assert_eq!(d_change, change_id);
        assert_eq!(d_start, start);
        assert_eq!(d_end, end);
    }

    #[test]
    fn test_position_encoding_roundtrip() {
        let change_id = 999u64;
        let pos = 12345u64;

        let encoded = encode_position(change_id, pos);
        let (dec_change, dec_pos) = decode_position(&encoded);

        assert_eq!(dec_change, change_id);
        assert_eq!(dec_pos, pos);
    }

    #[test]
    fn test_view_seq_encoding_roundtrip() {
        let view_id = 1u64;
        let seq = 42u64;

        let encoded = encode_view_seq(view_id, seq);
        let (dec_view, dec_seq) = decode_view_seq(&encoded);

        assert_eq!(dec_view, view_id);
        assert_eq!(dec_seq, seq);
    }

    #[test]
    fn test_view_merkle_encoding_roundtrip() {
        let view_id = 5u64;
        let merkle = [0xABu8; 32];

        let encoded = encode_view_merkle(view_id, &merkle);
        let (dec_view, dec_merkle) = decode_view_merkle(&encoded);

        assert_eq!(dec_view, view_id);
        assert_eq!(dec_merkle, merkle);
    }

    #[test]
    fn test_vertex_encoding_ordering() {
        // Verify that encoded vertices sort correctly (by change_id, then start, then end)
        let v1 = encode_vertex(1, 0, 10);
        let v2 = encode_vertex(1, 10, 20);
        let v3 = encode_vertex(2, 0, 10);

        assert!(v1 < v2, "same change, v1 should be before v2");
        assert!(v2 < v3, "v2 should be before v3 (different change)");
    }

    #[test]
    fn test_vertex_encoding_ordering_across_byte_boundary() {
        // Verify ordering is correct across the 256-boundary where LE encoding
        // would produce inverted lexicographic order (the original bug).
        // NodeId 255 = 0xFF in the low byte; 256 = 0x00 0x01 in LE which
        // sorts *before* 0xFF 0x00 lexicographically. BE encoding fixes this.
        let v255 = encode_vertex(255, 0, 10);
        let v256 = encode_vertex(256, 0, 10);
        assert!(v255 < v256, "change 255 should sort before change 256");

        let v511 = encode_vertex(511, 0, 10);
        let v512 = encode_vertex(512, 0, 10);
        assert!(v511 < v512, "change 511 should sort before change 512");

        // Also verify range scan would work: start < end
        let range_start = encode_vertex(255, 0, 0);
        let range_end = encode_vertex(256, 0, 0);
        assert!(range_start < range_end, "range scan bounds must be ordered");

        // Test start/end fields crossing byte boundaries too
        let va = encode_vertex(1, 255, 256);
        let vb = encode_vertex(1, 256, 257);
        assert!(
            va < vb,
            "start=255 should sort before start=256 within same change"
        );
    }

    #[test]
    fn test_inode_vertex_encoding_ordering() {
        // Verify that inode-vertices sort by inode first
        let v1 = encode_inode_vertex(1, 2, 0, 10);
        let v2 = encode_inode_vertex(2, 1, 0, 10);

        assert!(v1 < v2, "inode 1 should sort before inode 2");
    }

    #[test]
    fn test_inode_vertex_encoding_ordering_across_byte_boundary() {
        let v255 = encode_inode_vertex(255, 1, 0, 10);
        let v256 = encode_inode_vertex(256, 1, 0, 10);
        assert!(v255 < v256, "inode 255 should sort before inode 256");
    }

    #[test]
    fn test_view_seq_ordering_across_byte_boundary() {
        let s255 = encode_view_seq(1, 255);
        let s256 = encode_view_seq(1, 256);
        assert!(s255 < s256, "seq 255 should sort before seq 256");
    }

    // Directory Flag Tests

    #[test]
    fn test_directory_flags_explicit() {
        use super::directory_flags::*;

        assert!(is_explicit(DIR_EXPLICIT));
        assert!(is_explicit(DIR_EXPLICIT | DIR_EMPTY));
        assert!(!is_explicit(0));
        assert!(!is_explicit(DIR_EMPTY));
    }

    #[test]
    fn test_directory_flags_empty() {
        use super::directory_flags::*;

        assert!(is_empty(DIR_EMPTY));
        assert!(is_empty(DIR_EXPLICIT | DIR_EMPTY));
        assert!(!is_empty(0));
        assert!(!is_empty(DIR_EXPLICIT));
    }

    #[test]
    fn test_directory_flags_explicit_empty() {
        use super::directory_flags::*;

        let flags = explicit_empty();
        assert!(is_explicit(flags));
        assert!(is_empty(flags));
        assert_eq!(flags, DIR_EXPLICIT | DIR_EMPTY);
    }

    #[test]
    fn test_directory_flags_explicit_with_children() {
        use super::directory_flags::*;

        let flags = explicit_with_children();
        assert!(is_explicit(flags));
        assert!(!is_empty(flags));
        assert_eq!(flags, DIR_EXPLICIT);
    }

    #[test]
    fn test_directory_flags_combinations() {
        use super::directory_flags::*;

        // Test all possible flag combinations
        assert!(!is_explicit(0b00));
        assert!(!is_empty(0b00));

        assert!(is_explicit(0b01));
        assert!(!is_empty(0b01));

        assert!(!is_explicit(0b10));
        assert!(is_empty(0b10));

        assert!(is_explicit(0b11));
        assert!(is_empty(0b11));
    }

    // KG Edge Key / FTS Tests

    #[test]
    fn test_edge_key_roundtrip() {
        let key = encode_edge_key("change:abc", "file:auth.rs", "MODIFIES");
        let (from, to, kind) = decode_edge_key(&key).unwrap();
        assert_eq!(from, "change:abc");
        assert_eq!(to, "file:auth.rs");
        assert_eq!(kind, "MODIFIES");
    }

    #[test]
    fn test_tokenize_for_fts() {
        let tokens = tokenize_for_fts("Fix authentication bug in auth.rs");
        assert!(tokens.contains(&"fix".to_string()));
        assert!(tokens.contains(&"authentication".to_string()));
        assert!(tokens.contains(&"bug".to_string()));
        assert!(tokens.contains(&"auth".to_string()));
        assert!(tokens.contains(&"rs".to_string()));
        // Single char "in" is >= 2 chars, so it's included
        assert!(tokens.contains(&"in".to_string()));
    }

    #[test]
    fn test_tokenize_for_fts_filters_short() {
        let tokens = tokenize_for_fts("I a am the one");
        // "I" and "a" are < 2 chars, filtered out
        assert!(!tokens.contains(&"i".to_string()));
        assert!(!tokens.contains(&"a".to_string()));
        assert!(tokens.contains(&"am".to_string()));
        assert!(tokens.contains(&"the".to_string()));
    }

    #[test]
    fn test_embedding_key_roundtrip() {
        let key = encode_embedding_key("memory/arch.md", 3);
        let (path, idx) = decode_embedding_key(&key).unwrap();
        assert_eq!(path, "memory/arch.md");
        assert_eq!(idx, 3);
    }

    #[test]
    fn test_embedding_key_zero_chunk() {
        let key = encode_embedding_key("memory/arch.md", 0);
        let (path, idx) = decode_embedding_key(&key).unwrap();
        assert_eq!(path, "memory/arch.md");
        assert_eq!(idx, 0);
    }

    #[test]
    fn test_embedding_key_decode_invalid() {
        assert!(decode_embedding_key("no-separator").is_none());
        // Non-numeric chunk index
        assert!(decode_embedding_key("path\0abc").is_none());
    }
}

//! Change structure and serialization
//!
//! A **Change** (or "patch") is the fundamental unit of modification in Atomic.
//! Changes are content-addressed (identified by a Blake3 hash) and contain
//! all information needed to apply a modification to the repository graph.
//!
//! # Change File Format (V3)
//!
//! A change file uses the V3 streaming section-based format:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │  FileHeader (64 bytes, fixed, b"ATOM" magic)                    │
//! ├──────────────────────────────────────────────────────────────────┤
//! │  Hash Dedup Table (N × 32 bytes, uncompressed)                  │
//! ├──────────────────────────────────────────────────────────────────┤
//! │  Sections (each: type + compressed_len + zstd payload)          │
//! │    HEADER    — ChangeHeader (postcard)                          │
//! │    DEPS      — Vec<HashIndex> (postcard)                        │
//! │    PROVENANCE— Vec<Provenance> (postcard, optional)             │
//! │    GRAPH ×N  — CompactGraphOp per file (postcard)               │
//! │    SEMANTIC ×N — FileOps per file (postcard, optional)          │
//! │    CONTENT ×M — content chunks (zstd, content-addressed)        │
//! │    UNHASHED  — JSON metadata (not included in hash, optional)   │
//! ├──────────────────────────────────────────────────────────────────┤
//! │  Trailer (32 bytes — blake3 content hash)                       │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Hash Computation
//!
//! The change hash is computed incrementally via `blake3::Hasher` as sections
//! are written. It covers the hash dedup table, all section headers, and all
//! compressed section payloads — except the UNHASHED section. This means:
//! - No full-file buffering required
//! - Hash is available as soon as `finalize()` is called
//! - UNHASHED metadata can change without affecting the change identity
//!
//! # Example
//!
//! ```rust,ignore
//! use atomic_core::change::{Change, ChangeHeader, Author};
//! use std::io::Cursor;
//!
//! // Create a change
//! let change = Change::new(
//!     ChangeHeader::builder()
//!         .message("Add feature")
//!         .author(Author::new("Alice", Some("alice@example.com")))
//!         .build(),
//!     hunks,
//!     contents,
//!     dependencies,
//! );
//!
//! // Serialize to V3 format
//! let mut buffer = Vec::new();
//! let hash = change.serialize(&mut buffer)?;
//!
//! // Deserialize from V3 format
//! let mut cursor = Cursor::new(&buffer);
//! let (loaded, loaded_hash) = Change::deserialize(&mut cursor)?;
//! assert_eq!(hash, loaded_hash);
//! ```

use super::format_v3;
use super::graph_op::GraphOp;
use super::header::ChangeHeader;
use super::ops::FileOps;
use super::provenance::Provenance;
use crate::Hash;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use thiserror::Error;

/// Errors that can occur during change operations.
#[derive(Debug, Error)]
pub enum ChangeError {
    /// IO error during read/write
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// V3 format error (postcard serialization, compression, hash verification, etc.)
    #[error("Format error: {0}")]
    Format(#[from] format_v3::FormatError),

    /// JSON error for unhashed section
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Hash mismatch during verification
    #[error("Change hash mismatch: claimed {claimed}, computed {computed}")]
    HashMismatch { claimed: String, computed: String },

    /// Contents hash mismatch
    #[error("Contents hash mismatch: claimed {claimed}, computed {computed}")]
    ContentsHashMismatch { claimed: String, computed: String },

    /// Missing required content
    #[error("Missing contents for hash {hash}")]
    MissingContents { hash: String },

    /// Invalid change structure
    #[error("Invalid change: {0}")]
    Invalid(String),
}

/// A complete change (patch).
///
/// This is the in-memory representation of a change, containing:
/// - The hashed portion (header, hunks, dependencies)
/// - Optional unhashed metadata
/// - The raw content blob
///
/// Serialization uses the V3 streaming format with postcard encoding,
/// per-section zstd compression, and incremental blake3 hashing.
#[derive(Clone, Debug)]
pub struct Change {
    /// The hashed portion (contributes to change hash)
    pub hashed: HashedChange,

    /// Optional unhashed metadata (JSON)
    ///
    /// This can contain arbitrary data that doesn't affect the change hash.
    /// Useful for storing editor metadata, review comments, AI transcripts, etc.
    pub unhashed: Option<serde_json::Value>,

    /// Binary content blob
    ///
    /// This contains the actual file content referenced by hunks.
    /// Hunks reference byte ranges within this blob.
    pub contents: Vec<u8>,
}

impl Change {
    /// Create a new change.
    ///
    /// # Arguments
    ///
    /// * `header` - Change metadata (message, authors, timestamp)
    /// * `hunks` - The modifications in this change
    /// * `contents` - The content blob referenced by hunks
    /// * `dependencies` - Changes that must be applied before this one
    pub fn new(
        header: ChangeHeader,
        hunks: Vec<GraphOp<Option<Hash>>>,
        contents: Vec<u8>,
        dependencies: Vec<Hash>,
    ) -> Self {
        Self::with_file_ops(header, hunks, Vec::new(), contents, dependencies)
    }

    /// Create a new change with semantic layer operations.
    ///
    /// # Arguments
    ///
    /// * `header` - Change metadata (message, authors, timestamp)
    /// * `hunks` - The graph modifications in this change
    /// * `file_ops` - Semantic layer operations (Trunk → Branch → Leaf)
    /// * `contents` - The content blob referenced by operations
    /// * `dependencies` - Changes that must be applied before this one
    pub fn with_file_ops(
        header: ChangeHeader,
        hunks: Vec<GraphOp<Option<Hash>>>,
        file_ops: Vec<FileOps>,
        contents: Vec<u8>,
        dependencies: Vec<Hash>,
    ) -> Self {
        let contents_hash = Hash::of(&contents);

        Self {
            hashed: HashedChange {
                header,
                dependencies,
                extra_known: Vec::new(),
                metadata: Vec::new(),
                provenance: Vec::new(),
                hunks,
                file_ops,
                contents_hash,
            },
            unhashed: None,
            contents,
        }
    }

    /// Create an empty change with just a header.
    ///
    /// This is useful as a starting point for building changes.
    pub fn empty(header: ChangeHeader) -> Self {
        Self {
            hashed: HashedChange {
                header,
                dependencies: Vec::new(),
                extra_known: Vec::new(),
                metadata: Vec::new(),
                provenance: Vec::new(),
                hunks: Vec::new(),
                file_ops: Vec::new(),
                contents_hash: Hash::of(&[]),
            },
            unhashed: None,
            contents: Vec::new(),
        }
    }

    /// Add semantic layer operations to this change.
    pub fn add_file_ops(&mut self, ops: FileOps) {
        self.hashed.file_ops.push(ops);
    }

    /// Set the semantic layer operations for this change, replacing any existing ones.
    pub fn set_file_ops(&mut self, ops: Vec<FileOps>) {
        self.hashed.file_ops = ops;
    }

    /// Get a reference to the file operations.
    pub fn file_ops(&self) -> &[FileOps] {
        &self.hashed.file_ops
    }

    /// Check if this change has semantic layer operations.
    pub fn has_file_ops(&self) -> bool {
        !self.hashed.file_ops.is_empty()
    }

    /// Add a hunk (graph operation) to this change.
    pub fn add_hunk(&mut self, graph_op: GraphOp<Option<Hash>>) {
        self.hashed.hunks.push(graph_op);
    }

    /// Add provenance information to this change.
    pub fn add_provenance(&mut self, provenance: Provenance) {
        self.hashed.provenance.push(provenance);
    }

    /// Get a reference to the provenance information.
    pub fn provenance(&self) -> &[Provenance] {
        &self.hashed.provenance
    }

    /// Check if this change has AI provenance.
    pub fn has_provenance(&self) -> bool {
        !self.hashed.provenance.is_empty()
    }

    /// Append content to the contents blob.
    ///
    /// Returns the starting position of the appended content.
    pub fn append_contents(&mut self, data: &[u8]) -> usize {
        let start = self.contents.len();
        self.contents.extend_from_slice(data);
        start
    }

    /// Recompute the contents hash after modifying contents.
    pub fn finalize(&mut self) {
        self.hashed.contents_hash = Hash::of(&self.contents);
    }

    /// Get the change message.
    pub fn message(&self) -> &str {
        &self.hashed.header.message
    }

    /// Get the change dependencies.
    pub fn dependencies(&self) -> &[Hash] {
        &self.hashed.dependencies
    }

    /// Get the hunks in this change.
    pub fn hunks(&self) -> &[GraphOp<Option<Hash>>] {
        &self.hashed.hunks
    }

    /// Compute the hash of this change without writing it.
    ///
    /// This serializes the change to a temporary buffer using the V3 format
    /// and returns the content hash from the trailer.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn hash(&self) -> Result<Hash, ChangeError> {
        let mut buffer = Vec::new();
        let hash = self.serialize(&mut buffer)?;
        Ok(hash)
    }

    /// Serialize this change to a writer using the V3 format.
    ///
    /// Writes the complete V3 change file (header, hash table, sections, trailer)
    /// to the given writer. The content hash is computed incrementally as sections
    /// are written.
    ///
    /// # Arguments
    ///
    /// * `writer` - Where to write the serialized change
    ///
    /// # Returns
    ///
    /// The blake3 content hash of the change (its identity).
    ///
    /// # Errors
    ///
    /// Returns an error if serialization, compression, or I/O fails.
    pub fn serialize<W: Write>(&self, writer: &mut W) -> Result<Hash, ChangeError> {
        use format_v3::*;

        // 1. Build the hash dedup table from all referenced hashes
        //    Use a placeholder self-hash (we'll know the real one after finalize)
        let placeholder_hash = [0u8; 32];
        let mut hash_table = HashDedupTable::new(placeholder_hash);

        // Collect all unique hashes from dependencies
        for dep in &self.hashed.dependencies {
            hash_table.insert(*dep.as_bytes())?;
        }
        for dep in &self.hashed.extra_known {
            hash_table.insert(*dep.as_bytes())?;
        }

        // Collect all unique hashes from hunks
        self.collect_hunk_hashes(&mut hash_table)?;

        // 2. Compute section counts
        let has_provenance = !self.hashed.provenance.is_empty();
        let has_unhashed = self.unhashed.is_some();
        let has_content = !self.contents.is_empty();

        // Chunk content with FastCDC for delta transfer + parallel compression
        let content_chunks = if has_content {
            format_v3::chunk_content(&self.contents, &format_v3::ChunkingOptions::default())
        } else {
            Vec::new()
        };

        // For now, we write all hunks in a single GRAPH section (per-file splitting is future work)
        let graph_section_count = if self.hashed.hunks.is_empty() {
            0u32
        } else {
            1
        };
        let semantic_section_count = if self.hashed.file_ops.is_empty() {
            0u32
        } else {
            1
        };
        let contents_chunks_count = content_chunks.len() as u32;

        // 3. Build file header
        let mut file_header_builder = FileHeader::builder()
            .hash_table_entries(hash_table.len() as u32)
            .graph_section_count(graph_section_count)
            .semantic_section_count(semantic_section_count)
            .contents_chunks(contents_chunks_count);

        if has_provenance {
            file_header_builder = file_header_builder.with_provenance();
        }
        if has_unhashed {
            file_header_builder = file_header_builder.with_unhashed();
        }

        let file_header = file_header_builder.build();

        // 4. Create writer and write preamble
        let mut change_writer = ChangeWriter::new(writer, WriterOptions::default());
        change_writer.write_file_header(&file_header)?;
        change_writer.write_hash_table(&hash_table)?;

        // 5. Write metadata sections
        change_writer.write_change_header(&self.hashed.header)?;

        // Write dependency indices
        let dep_indices: Vec<u16> = self
            .hashed
            .dependencies
            .iter()
            .filter_map(|dep| hash_table.lookup(dep.as_bytes()))
            .collect();
        change_writer.write_dependencies(&dep_indices)?;

        // Write provenance if present
        if has_provenance {
            change_writer.write_provenance(&self.hashed.provenance)?;
        }

        // 6. Write GRAPH section(s) — compact graph ops
        if !self.hashed.hunks.is_empty() {
            let compactor = format_v3::compact::Compactor::new(&hash_table);

            let compact_ops: Vec<format_v3::CompactGraphOp> = self
                .hashed
                .hunks
                .iter()
                .map(|op| compactor.compact_graph_op(op))
                .collect::<Result<Vec<_>, _>>()?;

            let graph_payload = format_v3::GraphSectionPayload::new(
                String::new(), // All hunks in one section for now
                compact_ops,
                0,
                self.contents.len() as u64,
            );

            let payload_bytes = graph_payload.to_postcard_bytes()?;
            change_writer.write_graph_section(&payload_bytes)?;
        }

        // 7. Write SEMANTIC section(s)
        if !self.hashed.file_ops.is_empty() {
            // Serialize all file_ops together as one semantic section
            let semantic_bytes = postcard::to_allocvec(&self.hashed.file_ops)
                .map_err(format_v3::FormatError::from)?;
            change_writer.write_semantic_section(&semantic_bytes)?;
        }

        // 8. Write content chunks (FastCDC splits content into variable-size chunks)
        //    Each chunk is independently compressed and content-addressed by its blake3
        //    hash, enabling delta transfer (only send chunks the receiver doesn't have).
        for chunk in &content_chunks {
            change_writer.write_content_chunk(chunk.index, chunk.data(&self.contents))?;
        }

        // 9. Write unhashed section
        if let Some(ref unhashed) = self.unhashed {
            let unhashed_bytes = serde_json::to_vec(unhashed)?;
            change_writer.write_unhashed(&unhashed_bytes)?;
        }

        // 10. Finalize — writes trailer and returns content hash
        let outcome = change_writer.finalize()?;
        let hash = Hash::from_bytes(outcome.content_hash);

        Ok(hash)
    }

    /// Deserialize a change from a reader (V3 format).
    ///
    /// Reads a complete V3 change file, validates the structure, decompresses
    /// sections, and verifies the content hash against the trailer.
    ///
    /// # Arguments
    ///
    /// * `reader` - Where to read the serialized change from
    ///
    /// # Returns
    ///
    /// A tuple of `(change, hash)` where hash is the verified content hash.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file header is invalid (wrong magic, unsupported version)
    /// - Any section fails to decompress or deserialize
    /// - The content hash doesn't match the trailer
    pub fn deserialize<R: Read>(reader: &mut R) -> Result<(Self, Hash), ChangeError> {
        use format_v3::*;

        // Detect V2 (bincode) format and give a clear error.
        // V2 files start with a little-endian u64 version number (1 or 2).
        // V3 files start with b"ATOM". If the first 4 bytes look like a
        // small integer rather than ASCII "ATOM", it's an old format.
        //
        // We read into a buffer first so we can pass it to ChangeReader
        // if it IS V3 (since Read is not Seek).
        let mut header_peek = [0u8; 4];
        reader.read_exact(&mut header_peek)?;

        if &header_peek != b"ATOM" {
            // Check if it looks like a V2 version number (1 or 2 as LE u64)
            let maybe_version = u32::from_le_bytes(header_peek);
            if maybe_version == 1 || maybe_version == 2 {
                return Err(ChangeError::Invalid(format!(
                    "This change file uses the legacy V2 format (version {}). \
                     It must be re-recorded with the current version of Atomic. \
                     Delete .atomic/ and run: atomic init && atomic add -r . && atomic record -m \"re-record\"",
                    maybe_version
                )));
            }
            return Err(ChangeError::Format(FormatError::InvalidMagic {
                got: header_peek,
            }));
        }

        // It's V3 — reconstruct a reader with the magic bytes prepended.
        // Chain the 4 bytes we already read with the rest of the stream.
        let rest = reader;
        let mut combined = std::io::Cursor::new(header_peek).chain(rest);

        // 1. Open the reader (reads header + hash table)
        let mut change_reader = ChangeReader::open(&mut combined)?;
        let hash_table = change_reader.hash_table().clone();

        // 2. Read all sections
        let mut header: Option<ChangeHeader> = None;
        let mut dependencies: Vec<Hash> = Vec::new();
        let mut provenance: Vec<Provenance> = Vec::new();
        let mut hunks: Vec<GraphOp<Option<Hash>>> = Vec::new();
        let mut file_ops: Vec<FileOps> = Vec::new();
        let mut contents: Vec<u8> = Vec::new();
        let mut unhashed: Option<serde_json::Value> = None;

        while let Some(section) = change_reader.next_section()? {
            match section.section_type {
                SectionType::Header => {
                    header = Some(section.deserialize()?);
                }
                SectionType::Dependencies => {
                    let dep_indices: Vec<u16> = section.deserialize()?;
                    for idx in dep_indices {
                        if let Some(hash_bytes) = hash_table.resolve(idx) {
                            dependencies.push(Hash::from_bytes(*hash_bytes));
                        }
                    }
                }
                SectionType::Provenance => {
                    let prov_refs: Vec<Provenance> = section.deserialize()?;
                    provenance = prov_refs;
                }
                SectionType::Graph => {
                    let graph_payload: GraphSectionPayload =
                        GraphSectionPayload::from_postcard_bytes(&section.payload)?;
                    let compactor = compact::Compactor::new(&hash_table);
                    for compact_op in graph_payload.ops() {
                        hunks.push(compactor.expand_graph_op(compact_op)?);
                    }
                }
                SectionType::Semantic => {
                    let ops: Vec<FileOps> = postcard::from_bytes(&section.payload)
                        .map_err(format_v3::FormatError::from)?;
                    file_ops = ops;
                }
                SectionType::Content => {
                    contents.extend_from_slice(&section.payload);
                }
                SectionType::Unhashed => {
                    unhashed = Some(serde_json::from_slice(&section.payload)?);
                }
            }
        }

        // 3. Verify the content hash
        let content_hash_bytes = change_reader.verify()?;
        let content_hash = Hash::from_bytes(content_hash_bytes);

        // 4. Validate required sections
        let header =
            header.ok_or_else(|| ChangeError::Invalid("missing HEADER section".to_string()))?;

        // 5. Compute contents hash for verification
        let contents_hash = Hash::of(&contents);

        // 6. Build the Change
        let change = Change {
            hashed: HashedChange {
                header,
                dependencies,
                extra_known: Vec::new(),
                metadata: Vec::new(),
                provenance,
                hunks,
                file_ops,
                contents_hash,
            },
            unhashed,
            contents,
        };

        Ok((change, content_hash))
    }

    /// Check if this change depends on another change.
    pub fn depends_on(&self, hash: &Hash) -> bool {
        self.hashed.dependencies.contains(hash)
    }

    /// Check if this change knows about another change.
    ///
    /// A change "knows" another if it's either a dependency or extra_known.
    pub fn knows(&self, hash: &Hash) -> bool {
        self.hashed.dependencies.contains(hash) || self.hashed.extra_known.contains(hash)
    }

    /// Collect all unique hashes referenced by hunks into the hash dedup table.
    ///
    /// This walks every position, graph node, and introduced_by field in
    /// every hunk and registers each `Some(Hash)` in the table.
    fn collect_hunk_hashes(
        &self,
        table: &mut format_v3::HashDedupTable,
    ) -> Result<(), format_v3::FormatError> {
        use crate::change::atom::{Atom, EdgeUpdate, Insertion, NewEdge};

        fn collect_position_hash(
            pos: &crate::Position<Option<Hash>>,
            table: &mut format_v3::HashDedupTable,
        ) -> Result<(), format_v3::FormatError> {
            if let Some(ref h) = pos.change {
                table.insert(*h.as_bytes())?;
            }
            Ok(())
        }

        fn collect_graph_node_hash(
            node: &crate::GraphNode<Option<Hash>>,
            table: &mut format_v3::HashDedupTable,
        ) -> Result<(), format_v3::FormatError> {
            if let Some(ref h) = node.change {
                table.insert(*h.as_bytes())?;
            }
            Ok(())
        }

        fn collect_insertion_hashes(
            v: &Insertion<Option<Hash>>,
            table: &mut format_v3::HashDedupTable,
        ) -> Result<(), format_v3::FormatError> {
            for p in &v.predecessors {
                collect_position_hash(p, table)?;
            }
            for p in &v.successors {
                collect_position_hash(p, table)?;
            }
            collect_position_hash(&v.inode, table)?;
            Ok(())
        }

        fn collect_new_edge_hashes(
            e: &NewEdge<Option<Hash>>,
            table: &mut format_v3::HashDedupTable,
        ) -> Result<(), format_v3::FormatError> {
            collect_position_hash(&e.from, table)?;
            collect_graph_node_hash(&e.to, table)?;
            if let Some(ref h) = e.introduced_by {
                table.insert(*h.as_bytes())?;
            }
            Ok(())
        }

        fn collect_edge_update_hashes(
            em: &EdgeUpdate<Option<Hash>>,
            table: &mut format_v3::HashDedupTable,
        ) -> Result<(), format_v3::FormatError> {
            for e in &em.edges {
                collect_new_edge_hashes(e, table)?;
            }
            collect_position_hash(&em.inode, table)?;
            Ok(())
        }

        fn collect_atom_hashes(
            atom: &Atom<Option<Hash>>,
            table: &mut format_v3::HashDedupTable,
        ) -> Result<(), format_v3::FormatError> {
            match atom {
                Atom::Insertion(v) => collect_insertion_hashes(v, table),
                Atom::EdgeUpdate(em) => collect_edge_update_hashes(em, table),
            }
        }

        for hunk in &self.hashed.hunks {
            match hunk {
                GraphOp::FileAdd {
                    add_name,
                    add_inode,
                    contents,
                    ..
                } => {
                    collect_insertion_hashes(add_name, table)?;
                    collect_insertion_hashes(add_inode, table)?;
                    if let Some(c) = contents {
                        collect_insertion_hashes(c, table)?;
                    }
                }
                GraphOp::DirAdd {
                    add_name,
                    add_inode,
                    ..
                } => {
                    collect_insertion_hashes(add_name, table)?;
                    collect_insertion_hashes(add_inode, table)?;
                }
                GraphOp::DirDel { del, .. } => {
                    collect_edge_update_hashes(del, table)?;
                }
                GraphOp::DirUndel { undel, .. } => {
                    collect_edge_update_hashes(undel, table)?;
                }
                GraphOp::FileDel { del, contents, .. } => {
                    collect_edge_update_hashes(del, table)?;
                    if let Some(c) = contents {
                        collect_edge_update_hashes(c, table)?;
                    }
                }
                GraphOp::FileUndel {
                    undel, contents, ..
                } => {
                    collect_edge_update_hashes(undel, table)?;
                    if let Some(c) = contents {
                        collect_edge_update_hashes(c, table)?;
                    }
                }
                GraphOp::FileMove { del, add, .. } => {
                    collect_edge_update_hashes(del, table)?;
                    collect_insertion_hashes(add, table)?;
                }
                GraphOp::Edit { change, .. } => {
                    collect_atom_hashes(change, table)?;
                }
                GraphOp::Replacement {
                    change,
                    replacement,
                    ..
                } => {
                    collect_edge_update_hashes(change, table)?;
                    collect_insertion_hashes(replacement, table)?;
                }
                GraphOp::SolveNameConflict { name, .. } => {
                    collect_edge_update_hashes(name, table)?;
                }
                GraphOp::UnsolveNameConflict { name, .. } => {
                    collect_edge_update_hashes(name, table)?;
                }
                GraphOp::SolveOrderConflict { change, .. } => {
                    collect_edge_update_hashes(change, table)?;
                }
                GraphOp::UnsolveOrderConflict { change, .. } => {
                    collect_edge_update_hashes(change, table)?;
                }
                GraphOp::ResurrectZombies { change, .. } => {
                    collect_edge_update_hashes(change, table)?;
                }
                GraphOp::AddRoot { name, inode } => {
                    collect_insertion_hashes(name, table)?;
                    collect_insertion_hashes(inode, table)?;
                }
                GraphOp::DelRoot { name, inode } => {
                    collect_edge_update_hashes(name, table)?;
                    collect_edge_update_hashes(inode, table)?;
                }
            }
        }

        Ok(())
    }
}

impl Default for Change {
    fn default() -> Self {
        Self::empty(ChangeHeader::default())
    }
}

/// The hashed portion of a change.
///
/// This structure contains everything that contributes to the change hash.
/// Modifying any field here will result in a different change hash.
///
/// In V3, these fields are distributed across multiple sections:
/// - `header` → HEADER section
/// - `dependencies` → DEPS section (as hash indices)
/// - `provenance` → PROVENANCE section
/// - `hunks` → GRAPH sections (as CompactGraphOps)
/// - `file_ops` → SEMANTIC sections
/// - `contents_hash` → verified against CONTENT chunks
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashedChange {
    /// Change metadata (message, authors, timestamp)
    pub header: ChangeHeader,

    /// Direct dependencies (hashes of required changes)
    ///
    /// These changes MUST be applied before this change can be applied.
    pub dependencies: Vec<Hash>,

    /// Extra known changes (for context)
    ///
    /// These changes were known when this change was created, but are not
    /// strictly required. Used for better merge behavior.
    pub extra_known: Vec<Hash>,

    /// Custom metadata (opaque bytes)
    ///
    /// Application-specific metadata that affects the change hash.
    #[serde(default)]
    pub metadata: Vec<u8>,

    /// AI provenance information (optional)
    ///
    /// Tracks AI involvement in creating this change, including:
    /// - Vendor/model information
    /// - Prompt hashes (for privacy)
    /// - Token usage and cost
    /// - Suggestion type (complete, partial, collaborative)
    #[serde(default)]
    pub provenance: Vec<Provenance>,

    /// The actual modifications (graph operations)
    ///
    /// These hunks represent the low-level graph operations (vertices, edges)
    /// that modify the repository graph. They are the "storage layer" operations.
    pub hunks: Vec<GraphOp<Option<Hash>>>,

    /// Semantic layer operations (CRDT model)
    ///
    /// These operations represent the human-readable changes organized as:
    /// - FileOps (Trunk level): File create/delete/move/undelete
    /// - LineOps (Branch level): Line insert/delete/restore
    /// - LeafOp (Leaf level): Token insert/delete/replace
    ///
    /// This enables:
    /// - Line-number based diffs (not byte ranges)
    /// - Token-level highlighting (`--word-diff`)
    /// - Fine-grained blame (who wrote each token)
    /// - Human-readable code review
    #[serde(default)]
    pub file_ops: Vec<FileOps>,

    /// Hash of the contents blob
    ///
    /// This allows verification of the contents without including them
    /// in the hashed section directly.
    pub contents_hash: Hash,
}

impl HashedChange {
    /// Get all dependencies and extra_known combined.
    pub fn all_known(&self) -> impl Iterator<Item = &Hash> {
        self.dependencies.iter().chain(self.extra_known.iter())
    }

    /// Check if this change has any hunks.
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty() && self.file_ops.is_empty()
    }

    /// Get the number of hunks.
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Check if this change has AI provenance.
    pub fn has_provenance(&self) -> bool {
        !self.provenance.is_empty()
    }

    /// Get the number of provenance entries.
    pub fn provenance_count(&self) -> usize {
        self.provenance.len()
    }

    /// Check if this change has semantic layer operations.
    pub fn has_file_ops(&self) -> bool {
        !self.file_ops.is_empty()
    }

    /// Get the number of file operations.
    pub fn file_ops_count(&self) -> usize {
        self.file_ops.len()
    }

    /// Get a reference to the file operations.
    pub fn file_ops(&self) -> &[FileOps] {
        &self.file_ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::atom::{Atom, Insertion};
    use crate::change::{Author, Encoding, Local};
    use crate::{ChangePosition, EdgeFlags, Position};
    use std::io::Cursor;

    fn test_hash_position(pos: u64) -> Position<Option<Hash>> {
        Position::new(Some(Hash::of(b"test")), ChangePosition::new(pos))
    }

    fn test_new_vertex() -> Insertion<Option<Hash>> {
        Insertion {
            predecessors: vec![],
            successors: vec![],
            flag: EdgeFlags::BLOCK,
            start: ChangePosition::new(0),
            end: ChangePosition::new(10),
            inode: test_hash_position(0),
        }
    }

    // ========================================================================
    // HashedChange Tests
    // ========================================================================

    #[test]
    fn test_hashed_change_is_empty() {
        let hashed = HashedChange {
            header: ChangeHeader::default(),
            dependencies: Vec::new(),
            extra_known: Vec::new(),
            metadata: Vec::new(),
            provenance: Vec::new(),
            hunks: Vec::new(),
            file_ops: Vec::new(),
            contents_hash: Hash::of(&[]),
        };

        assert!(hashed.is_empty());
        assert_eq!(hashed.hunk_count(), 0);
        assert!(!hashed.has_provenance());
        assert_eq!(hashed.provenance_count(), 0);
        assert!(!hashed.has_file_ops());
        assert_eq!(hashed.file_ops_count(), 0);
    }

    #[test]
    fn test_hashed_change_all_known() {
        let dep1 = Hash::of(b"dep1");
        let dep2 = Hash::of(b"dep2");
        let known1 = Hash::of(b"known1");

        let hashed = HashedChange {
            header: ChangeHeader::default(),
            dependencies: vec![dep1, dep2],
            extra_known: vec![known1],
            metadata: Vec::new(),
            provenance: Vec::new(),
            hunks: Vec::new(),
            file_ops: Vec::new(),
            contents_hash: Hash::of(&[]),
        };

        let all: Vec<_> = hashed.all_known().collect();
        assert_eq!(all.len(), 3);
        assert!(all.contains(&&dep1));
        assert!(all.contains(&&dep2));
        assert!(all.contains(&&known1));
    }

    // ========================================================================
    // Change Construction Tests
    // ========================================================================

    #[test]
    fn test_change_new() {
        let header = ChangeHeader::new("Test");
        let dep = Hash::of(b"dep");
        let change = Change::new(header, vec![], b"content".to_vec(), vec![dep]);

        assert_eq!(change.message(), "Test");
        assert_eq!(change.dependencies().len(), 1);
        assert_eq!(change.contents, b"content");
    }

    #[test]
    fn test_change_empty() {
        let change = Change::empty(ChangeHeader::new("Empty"));
        assert!(change.hunks().is_empty());
        assert!(change.contents.is_empty());
        assert_eq!(change.message(), "Empty");
    }

    #[test]
    fn test_change_add_hunk() {
        let mut change = Change::empty(ChangeHeader::new("Test"));
        let graph_op: GraphOp<Option<Hash>> = GraphOp::Edit {
            change: Atom::Insertion(test_new_vertex()),
            local: Local::new("test.rs", 1),
            encoding: Some(Encoding::Utf8),
        };
        change.add_hunk(graph_op);

        assert_eq!(change.hunks().len(), 1);
    }

    #[test]
    fn test_change_append_contents() {
        let mut change = Change::empty(ChangeHeader::new("Test"));
        let pos = change.append_contents(b"Hello");
        assert_eq!(pos, 0);
        let pos2 = change.append_contents(b" World");
        assert_eq!(pos2, 5);
        assert_eq!(change.contents, b"Hello World");
    }

    #[test]
    fn test_change_finalize() {
        let mut change = Change::empty(ChangeHeader::new("Test"));
        change.append_contents(b"content");
        change.finalize();
        assert_eq!(change.hashed.contents_hash, Hash::of(b"content"));
    }

    #[test]
    fn test_change_hash() {
        let change = Change::new(
            ChangeHeader::new("Test hash"),
            vec![],
            b"content".to_vec(),
            vec![],
        );
        let hash = change.hash().unwrap();
        // Hash should be non-zero
        assert_ne!(hash, Hash::of(&[]));
    }

    #[test]
    fn test_change_depends_on() {
        let dep = Hash::of(b"dep");
        let change = Change::new(ChangeHeader::new("Test"), vec![], vec![], vec![dep]);
        assert!(change.depends_on(&dep));
        assert!(!change.depends_on(&Hash::of(b"other")));
    }

    #[test]
    fn test_change_knows() {
        let dep = Hash::of(b"dep");
        let mut change = Change::new(ChangeHeader::new("Test"), vec![], vec![], vec![dep]);
        let extra = Hash::of(b"extra");
        change.hashed.extra_known.push(extra);

        assert!(change.knows(&dep));
        assert!(change.knows(&extra));
        assert!(!change.knows(&Hash::of(b"unknown")));
    }

    #[test]
    fn test_change_default() {
        let change = Change::default();
        assert!(change.hunks().is_empty());
        assert!(change.contents.is_empty());
    }

    // ========================================================================
    // V3 Serialization Tests
    // ========================================================================

    #[test]
    fn test_change_serialize_deserialize() {
        let header = ChangeHeader::builder()
            .message("Test serialization")
            .author(Author::new("Bob", Some("bob@example.com")))
            .build();

        let contents = b"File contents here".to_vec();
        let change = Change::new(header, vec![], contents, vec![]);

        // Serialize
        let mut buffer = Vec::new();
        let hash = change.serialize(&mut buffer).unwrap();

        // Verify V3 magic bytes
        assert_eq!(&buffer[0..4], b"ATOM");

        // Deserialize
        let mut cursor = Cursor::new(buffer);
        let (loaded, loaded_hash) = Change::deserialize(&mut cursor).unwrap();

        // Verify
        assert_eq!(hash, loaded_hash);
        assert_eq!(change.message(), loaded.message());
        assert_eq!(change.contents, loaded.contents);
    }

    #[test]
    fn test_change_serialize_with_hunks() {
        let mut change = Change::empty(ChangeHeader::new("With hunks"));

        let graph_op: GraphOp<Option<Hash>> = GraphOp::Edit {
            change: Atom::Insertion(test_new_vertex()),
            local: Local::new("test.rs", 42),
            encoding: Some(Encoding::Utf8),
        };
        change.add_hunk(graph_op);
        change.append_contents(b"Hello World");
        change.finalize();

        // Serialize
        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        // Deserialize
        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert_eq!(loaded.hunks().len(), 1);
        assert_eq!(loaded.contents, b"Hello World");
    }

    #[test]
    fn test_change_serialize_with_unhashed() {
        let mut change = Change::empty(ChangeHeader::new("With unhashed"));
        change.unhashed = Some(serde_json::json!({
            "editor": "vim",
            "custom_field": 42
        }));

        // Serialize
        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        // Deserialize
        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert!(loaded.unhashed.is_some());
        let unhashed = loaded.unhashed.unwrap();
        assert_eq!(unhashed["editor"], "vim");
        assert_eq!(unhashed["custom_field"], 42);
    }

    #[test]
    fn test_change_serialize_with_dependencies() {
        let dep1 = Hash::of(b"dep1");
        let dep2 = Hash::of(b"dep2");

        let change = Change::new(
            ChangeHeader::new("With deps"),
            vec![],
            vec![],
            vec![dep1, dep2],
        );

        // Serialize
        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        // Deserialize
        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert_eq!(loaded.dependencies().len(), 2);
        assert!(loaded.depends_on(&dep1));
        assert!(loaded.depends_on(&dep2));
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_empty_contents() {
        let change = Change::new(ChangeHeader::new("Empty contents"), vec![], vec![], vec![]);

        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert!(loaded.contents.is_empty());
    }

    #[test]
    fn test_large_contents() {
        let large_contents = vec![0u8; 1024 * 1024]; // 1MB
        let change = Change::new(
            ChangeHeader::new("Large contents"),
            vec![],
            large_contents.clone(),
            vec![],
        );

        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert_eq!(loaded.contents.len(), 1024 * 1024);
    }

    #[test]
    fn test_multiple_hunks() {
        let mut change = Change::empty(ChangeHeader::new("Multiple hunks"));

        for i in 0..10 {
            let graph_op: GraphOp<Option<Hash>> = GraphOp::Edit {
                change: Atom::Insertion(Insertion {
                    predecessors: vec![],
                    successors: vec![],
                    flag: EdgeFlags::BLOCK,
                    start: ChangePosition::new(i * 10),
                    end: ChangePosition::new((i + 1) * 10),
                    inode: test_hash_position(0),
                }),
                local: Local::new("test.rs", i + 1),
                encoding: Some(Encoding::Utf8),
            };
            change.add_hunk(graph_op);
        }

        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let (loaded, _) = Change::deserialize(&mut cursor).unwrap();

        assert_eq!(loaded.hunks().len(), 10);
    }

    #[test]
    fn test_hash_stability() {
        // Same change content should always produce same hash
        // Use a fixed timestamp to ensure deterministic hashing
        let fixed_ts = chrono::DateTime::parse_from_rfc3339("2024-01-15T10:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let make_change = || {
            let header = ChangeHeader::builder()
                .message("Stable hash test")
                .timestamp(fixed_ts)
                .build();
            Change::new(header, vec![], b"content".to_vec(), vec![])
        };

        let hash1 = make_change().hash().unwrap();
        let hash2 = make_change().hash().unwrap();

        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_v3_magic_bytes() {
        let change = Change::empty(ChangeHeader::new("Magic"));
        let mut buffer = Vec::new();
        change.serialize(&mut buffer).unwrap();

        // V3 files start with b"ATOM"
        assert!(buffer.len() >= 4);
        assert_eq!(&buffer[0..4], b"ATOM");
    }

    #[test]
    fn test_serialize_deserialize_roundtrip_with_all_fields() {
        let dep = Hash::of(b"dep");
        let mut change = Change::new(
            ChangeHeader::builder()
                .message("Full roundtrip")
                .description("A complete test")
                .author(Author::new("Alice", Some("alice@test.com")))
                .build(),
            vec![],
            b"file content".to_vec(),
            vec![dep],
        );

        // Add a hunk
        let graph_op: GraphOp<Option<Hash>> = GraphOp::Edit {
            change: Atom::Insertion(Insertion {
                predecessors: vec![test_hash_position(0)],
                successors: vec![],
                flag: EdgeFlags::BLOCK,
                start: ChangePosition::new(0),
                end: ChangePosition::new(12),
                inode: test_hash_position(0),
            }),
            local: Local::new("file.txt", 1),
            encoding: Some(Encoding::Utf8),
        };
        change.add_hunk(graph_op);

        // Add unhashed
        change.unhashed = Some(serde_json::json!({"notes": "test"}));

        change.finalize();

        // Roundtrip
        let mut buffer = Vec::new();
        let hash = change.serialize(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let (loaded, loaded_hash) = Change::deserialize(&mut cursor).unwrap();

        assert_eq!(hash, loaded_hash);
        assert_eq!(loaded.message(), "Full roundtrip");
        assert_eq!(
            loaded.hashed.header.description.as_deref(),
            Some("A complete test")
        );
        assert_eq!(loaded.dependencies().len(), 1);
        assert!(loaded.depends_on(&dep));
        assert_eq!(loaded.hunks().len(), 1);
        assert_eq!(loaded.contents, b"file content");
        assert!(loaded.unhashed.is_some());
    }
}

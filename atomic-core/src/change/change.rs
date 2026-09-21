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

use super::classification::{
    validate_canonical_hashes, CausalFrontier, ChangeKind, ChangeOrigin, ChangeValidationError,
    VerifiedCausalFrontier,
};
use super::format_v3;
use super::graph_op::GraphOp;
use super::header::ChangeHeader;
use super::ops::FileOps;
use super::provenance::Provenance;
use crate::Hash;
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

    /// Invalid or noncanonical hashed change facts.
    #[error(transparent)]
    Validation(#[from] ChangeValidationError),
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
                kind: ChangeKind::Durable,
                supersedes: None,
                origin: ChangeOrigin::Native,
                causal_frontier: CausalFrontier::empty(),
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
                kind: ChangeKind::Durable,
                supersedes: None,
                origin: ChangeOrigin::Native,
                causal_frontier: CausalFrontier::empty(),
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

    /// Set all hashed lifecycle/origin facts and validate them together.
    pub fn with_classification(
        mut self,
        kind: ChangeKind,
        supersedes: Option<Hash>,
        origin: ChangeOrigin,
        causal_frontier: CausalFrontier,
    ) -> Result<Self, ChangeValidationError> {
        self.hashed.kind = kind;
        self.hashed.supersedes = supersedes;
        self.hashed.origin = origin;
        self.hashed.causal_frontier = causal_frontier;
        self.hashed.validate_v2()?;
        Ok(self)
    }

    /// Lifecycle class of this change.
    pub fn kind(&self) -> &ChangeKind {
        &self.hashed.kind
    }

    /// Hash-authoritative origin of this change.
    pub fn origin(&self) -> &ChangeOrigin {
        &self.hashed.origin
    }

    /// Previous snapshot replaced by this change, if any.
    pub fn supersedes(&self) -> Option<&Hash> {
        self.hashed.supersedes.as_ref()
    }

    /// Causal closure roots known by this change.
    pub fn causal_frontier(&self) -> &CausalFrontier {
        &self.hashed.causal_frontier
    }

    /// Validate canonical schema-version-2 facts and derived content hash.
    pub fn validate_v2(&self) -> Result<(), ChangeError> {
        self.hashed.validate_v2()?;
        let computed = Hash::of(&self.contents);
        if computed != self.hashed.contents_hash {
            return Err(ChangeError::ContentsHashMismatch {
                claimed: self.hashed.contents_hash.to_string(),
                computed: computed.to_string(),
            });
        }
        Ok(())
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

        self.validate_v2()?;

        // 1. Build the hash dedup table from all referenced hashes
        //    Use a placeholder self-hash (we'll know the real one after finalize)
        let placeholder_hash = [0u8; 32];
        let mut hash_table = HashDedupTable::new(placeholder_hash);

        // Collect all unique hashes from dependencies
        for dep in &self.hashed.dependencies {
            hash_table.insert(*dep.as_bytes())?;
        }
        for known in &self.hashed.extra_known {
            hash_table.insert(*known.as_bytes())?;
        }
        if let Some(supersedes) = self.hashed.supersedes {
            hash_table.insert(*supersedes.as_bytes())?;
        }
        for root in self.hashed.causal_frontier.roots() {
            hash_table.insert(*root.as_bytes())?;
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

        // 5. Write metadata sections. Schema version 2 freezes the complete
        // hashed change metadata in the HEADER payload.
        let header_payload =
            format_v3::types::envelope::encode_change_header_v2(&self.hashed, &hash_table)?;
        change_writer.write_change_header_v2_payload(&header_payload)?;

        // Write dependency indices
        let dep_indices: Vec<u16> = self
            .hashed
            .dependencies
            .iter()
            .map(|dep| hash_table.require(dep.as_bytes()))
            .collect::<Result<_, _>>()?;
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
        let file_header = *change_reader.file_header();
        let format_version = file_header.version;
        let hash_table = change_reader.hash_table().clone();
        let strict_v2 = format_version == FORMAT_VERSION;

        // 2. Read all sections. V2 enforces the canonical section manifest;
        // V1 keeps its historical framing and gains only semantic defaults.
        let mut legacy_header: Option<ChangeHeader> = None;
        let mut decoded_v2: Option<format_v3::types::envelope::DecodedChangeHeaderV2> = None;
        let mut dependencies: Vec<Hash> = Vec::new();
        let mut provenance: Vec<Provenance> = Vec::new();
        let mut hunks: Vec<GraphOp<Option<Hash>>> = Vec::new();
        let mut file_ops: Vec<FileOps> = Vec::new();
        let mut contents: Vec<u8> = Vec::new();
        let mut unhashed: Option<serde_json::Value> = None;
        let mut last_ordering = None;
        let mut header_count = 0u32;
        let mut deps_count = 0u32;
        let mut provenance_count = 0u32;
        let mut graph_count = 0u32;
        let mut semantic_count = 0u32;
        let mut content_count = 0u32;
        let mut unhashed_count = 0u32;

        while let Some(section) = change_reader.next_section()? {
            if strict_v2 {
                let ordering = section.section_type.ordering();
                if last_ordering.is_some_and(|last| ordering < last) {
                    return Err(ChangeError::Format(FormatError::UnexpectedSection {
                        got: section.section_type.name().to_string(),
                        expected: "canonical V2 section ordering".to_string(),
                    }));
                }
                last_ordering = Some(ordering);
            }

            match section.section_type {
                SectionType::Header => {
                    header_count += 1;
                    if strict_v2 {
                        if header_count != 1 {
                            return Err(ChangeError::Invalid(
                                "duplicate V2 HEADER section".to_string(),
                            ));
                        }
                        decoded_v2 = Some(format_v3::types::envelope::decode_change_header_v2(
                            &section.payload,
                            &hash_table,
                        )?);
                    } else {
                        legacy_header = Some(
                            postcard::from_bytes(&section.payload)
                                .map_err(format_v3::FormatError::from)?,
                        );
                    }
                }
                SectionType::Dependencies => {
                    deps_count += 1;
                    if strict_v2 && deps_count != 1 {
                        return Err(ChangeError::Invalid(
                            "duplicate V2 DEPS section".to_string(),
                        ));
                    }
                    let dep_indices: Vec<u16> = section.deserialize()?;
                    dependencies = dep_indices
                        .into_iter()
                        .map(|index| {
                            format_v3::types::envelope::decode_hash(
                                index,
                                &hash_table,
                                "dependencies",
                            )
                        })
                        .collect::<Result<_, _>>()?;
                }
                SectionType::Provenance => {
                    provenance_count += 1;
                    if strict_v2 && provenance_count != 1 {
                        return Err(ChangeError::Invalid(
                            "duplicate V2 PROVENANCE section".to_string(),
                        ));
                    }
                    provenance = super::provenance::deserialize_postcard(&section.payload)
                        .map_err(|error| {
                            ChangeError::Invalid(format!(
                                "failed to deserialize provenance section: {error}"
                            ))
                        })?;
                }
                SectionType::Graph => {
                    graph_count += 1;
                    let graph_payload: GraphSectionPayload =
                        GraphSectionPayload::from_postcard_bytes(&section.payload)?;
                    let compactor = compact::Compactor::new(&hash_table);
                    for compact_op in graph_payload.ops() {
                        hunks.push(compactor.expand_graph_op(compact_op)?);
                    }
                }
                SectionType::Semantic => {
                    semantic_count += 1;
                    let ops: Vec<FileOps> = postcard::from_bytes(&section.payload)
                        .map_err(format_v3::FormatError::from)?;
                    file_ops.extend(ops);
                }
                SectionType::Content => {
                    content_count += 1;
                    contents.extend_from_slice(&section.payload);
                }
                SectionType::Unhashed => {
                    unhashed_count += 1;
                    if strict_v2 && unhashed_count != 1 {
                        return Err(ChangeError::Invalid(
                            "duplicate V2 UNHASHED section".to_string(),
                        ));
                    }
                    unhashed = Some(serde_json::from_slice(&section.payload)?);
                }
            }
        }

        // 3. Verify the original bytes before constructing semantic defaults.
        let content_hash_bytes = change_reader.verify()?;
        let content_hash = Hash::from_bytes(content_hash_bytes);

        if strict_v2 {
            let expected_provenance = u32::from(
                file_header
                    .flags
                    .has(format_v3::FileHeaderFlags::HAS_PROVENANCE),
            );
            let expected_unhashed = u32::from(
                file_header
                    .flags
                    .has(format_v3::FileHeaderFlags::HAS_UNHASHED),
            );
            if header_count != 1
                || deps_count != 1
                || provenance_count != expected_provenance
                || graph_count != file_header.graph_section_count
                || semantic_count != file_header.semantic_section_count
                || content_count != file_header.contents_chunks
                || unhashed_count != expected_unhashed
            {
                return Err(ChangeError::Invalid(
                    "V2 section manifest does not match the file header".to_string(),
                ));
            }
        }

        // 4. Apply version-specific hashed metadata semantics.
        let (header, kind, supersedes, origin, causal_frontier, extra_known, metadata) =
            if format_version == LEGACY_FORMAT_VERSION {
                let header = legacy_header.ok_or_else(|| {
                    ChangeError::Invalid("missing legacy V1 HEADER section".to_string())
                })?;
                (
                    header,
                    ChangeKind::Durable,
                    None,
                    ChangeOrigin::Native,
                    CausalFrontier::empty(),
                    Vec::new(),
                    Vec::new(),
                )
            } else {
                let decoded = decoded_v2.ok_or_else(|| {
                    ChangeError::Invalid("missing V2 HEADER envelope".to_string())
                })?;
                (
                    decoded.header,
                    decoded.kind,
                    decoded.supersedes,
                    decoded.origin,
                    decoded.causal_frontier,
                    decoded.extra_known,
                    decoded.metadata,
                )
            };

        // 5. Contents hash is derived canonically from the reconstructed chunks.
        let contents_hash = Hash::of(&contents);

        // 6. Build and validate the Change.
        let change = Change {
            hashed: HashedChange {
                header,
                kind,
                supersedes,
                origin,
                causal_frontier,
                dependencies,
                extra_known,
                metadata,
                provenance,
                hunks,
                file_ops,
                contents_hash,
            },
            unhashed,
            contents,
        };
        if strict_v2 {
            change.validate_v2()?;
        }

        Ok((change, content_hash))
    }

    /// Check if this change depends on another change.
    pub fn depends_on(&self, hash: &Hash) -> bool {
        self.hashed.dependencies.contains(hash)
    }

    /// Check if this change directly knows about another change.
    ///
    /// Direct knowledge comes from a context dependency or `extra_known`.
    /// Causal-frontier closure membership is accepted only by
    /// [`Self::knows_with_frontier`] after repository verification.
    pub fn knows(&self, hash: &Hash) -> bool {
        self.hashed.dependencies.contains(hash) || self.hashed.extra_known.contains(hash)
    }

    /// Check direct knowledge plus a repository-verified causal closure.
    ///
    /// A verified index for a different frontier is ignored, preventing callers
    /// from using unrelated closure membership as causal proof.
    pub fn knows_with_frontier(&self, hash: &Hash, verified: &VerifiedCausalFrontier) -> bool {
        self.knows(hash)
            || (verified.matches(&self.hashed.causal_frontier) && verified.contains(hash))
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
                GraphOp::SetAttr { inode, .. } => collect_position_hash(inode, table)?,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashedChange {
    /// Human-readable change metadata.
    pub header: ChangeHeader,

    /// Durable or working-copy snapshot lifecycle class.
    pub kind: ChangeKind,

    /// Previous snapshot replaced by this snapshot.
    pub supersedes: Option<Hash>,

    /// Native or Git-derived origin.
    pub origin: ChangeOrigin,

    /// Canonical causal closure roots known by this change.
    pub causal_frontier: CausalFrontier,

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
    pub metadata: Vec<u8>,

    /// AI provenance information (optional)
    ///
    /// Tracks AI involvement in creating this change, including:
    /// - Vendor/model information
    /// - Prompt hashes (for privacy)
    /// - Token usage and cost
    /// - Suggestion type (complete, partial, collaborative)
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
    pub file_ops: Vec<FileOps>,

    /// Hash of the contents blob
    ///
    /// This allows verification of the contents without including them
    /// in the hashed section directly.
    pub contents_hash: Hash,
}

impl HashedChange {
    /// Validate schema-version-2 lifecycle, origin, and canonical hash facts.
    pub fn validate_v2(&self) -> Result<(), ChangeValidationError> {
        validate_canonical_hashes("dependencies", &self.dependencies)?;
        validate_canonical_hashes("extra_known", &self.extra_known)?;
        self.causal_frontier.validate()?;
        self.origin.validate()?;

        if let Some(overlap) = self
            .dependencies
            .iter()
            .find(|dependency| self.extra_known.binary_search(dependency).is_ok())
        {
            return Err(ChangeValidationError::DependencyExtraKnownOverlap { hash: *overlap });
        }
        if let Some(parents) = self.origin.git_parents() {
            for (parent_index, parent) in parents.iter().enumerate() {
                if parent.algorithm() == crate::GitHashAlgorithm::Sha256
                    && self
                        .dependencies
                        .iter()
                        .any(|dependency| &dependency.as_bytes()[..] == parent.as_bytes())
                {
                    return Err(ChangeValidationError::GitParentDependency { parent_index });
                }
            }
        }

        if let Some(supersedes) = self.supersedes {
            if supersedes == Hash::NONE {
                return Err(ChangeValidationError::ZeroHash {
                    field: "supersedes",
                    index: 0,
                });
            }
            if !self.kind.is_snapshot() {
                return Err(ChangeValidationError::SupersedesRequiresSnapshot);
            }
            if self.dependencies.contains(&supersedes) {
                return Err(ChangeValidationError::SupersedesDependency);
            }
        }

        if self.kind.is_snapshot() && !self.origin.is_native() {
            return Err(ChangeValidationError::SnapshotMustBeNative);
        }
        if !self.origin.is_native() && !self.kind.is_durable() {
            return Err(ChangeValidationError::GitOriginRequiresDurable);
        }
        if matches!(self.origin, ChangeOrigin::GitResolution { .. }) {
            if self.causal_frontier.is_empty() {
                return Err(ChangeValidationError::GitResolutionRequiresFrontier);
            }
        } else if !self.causal_frontier.is_empty() {
            return Err(ChangeValidationError::CausalFrontierRequiresGitResolution);
        }

        Ok(())
    }

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
    use crate::{
        Base32, ChangePosition, EdgeFlags, GitHashAlgorithm, GitObjectId, Position, WorkingCopyId,
    };
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

    fn sha1(byte: u8) -> GitObjectId {
        GitObjectId::new(GitHashAlgorithm::Sha1, vec![byte; 20]).unwrap()
    }

    fn sha256(byte: u8) -> GitObjectId {
        GitObjectId::new(GitHashAlgorithm::Sha256, vec![byte; 32]).unwrap()
    }

    fn legacy_v1_fixture_bytes() -> Vec<u8> {
        let hex: String = include_str!("format_v3/fixtures/cb_fmt1_legacy_v1_object.hex")
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect();
        assert_eq!(hex.len() % 2, 0);
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect()
    }

    // HashedChange Tests

    #[test]
    fn test_hashed_change_is_empty() {
        let hashed = HashedChange {
            header: ChangeHeader::default(),
            kind: ChangeKind::Durable,
            supersedes: None,
            origin: ChangeOrigin::Native,
            causal_frontier: CausalFrontier::empty(),
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
            kind: ChangeKind::Durable,
            supersedes: None,
            origin: ChangeOrigin::Native,
            causal_frontier: CausalFrontier::empty(),
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

    // Change Construction Tests

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

    // V3 Serialization Tests

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

        let mut dependencies = vec![dep1, dep2];
        dependencies.sort();
        let change = Change::new(ChangeHeader::new("With deps"), vec![], vec![], dependencies);

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

    #[test]
    fn legacy_v1_object_fixture_keeps_original_bytes_hash_and_defaults() {
        const OBJECT_HASH: &str = "KQEBIVO7FVXRZ5PWLT75G267BRXZU5GKHWLV62MHBQDG67VE5BGQ";
        const FILE_HASH: &str = "QABDDCIBSDQARC2BT5TEYUDYB3M46IOVJLKYFOE3NJTPI4BLNLAA";

        let bytes = legacy_v1_fixture_bytes();
        assert_eq!(Hash::of(&bytes).to_base32(), FILE_HASH);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);

        let (change, verified_hash) = Change::deserialize(&mut Cursor::new(&bytes)).unwrap();
        assert_eq!(verified_hash.to_base32(), OBJECT_HASH);
        assert_eq!(change.message(), "CB-FMT1 legacy V1 fixture");
        assert_eq!(change.kind(), &ChangeKind::Durable);
        assert_eq!(change.origin(), &ChangeOrigin::Native);
        assert!(change.supersedes().is_none());
        assert!(change.causal_frontier().is_empty());
        assert!(change.hashed.extra_known.is_empty());
        assert!(change.hashed.metadata.is_empty());
        assert_eq!(change.contents, b"legacy-v1-content\n");
    }

    #[test]
    fn v2_snapshot_roundtrip_preserves_hashed_metadata() {
        let supersedes = Hash::from_bytes([3; 32]);
        let extra_known = Hash::from_bytes([4; 32]);
        let working_copy = WorkingCopyId::from_bytes([5; 16]);
        let mut change = Change::empty(ChangeHeader::new("Snapshot"))
            .with_classification(
                ChangeKind::Snapshot { working_copy },
                Some(supersedes),
                ChangeOrigin::Native,
                CausalFrontier::empty(),
            )
            .unwrap();
        change.hashed.extra_known = vec![extra_known];
        change.hashed.metadata = b"opaque hashed metadata".to_vec();

        let mut bytes = Vec::new();
        let hash = change.serialize(&mut bytes).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 2);

        let (loaded, loaded_hash) = Change::deserialize(&mut Cursor::new(&bytes)).unwrap();
        assert_eq!(loaded_hash, hash);
        assert_eq!(loaded.hashed, change.hashed);
        assert_eq!(loaded.kind().working_copy(), Some(working_copy));
        assert_eq!(loaded.supersedes(), Some(&supersedes));
    }

    #[test]
    fn v2_git_synthesized_roundtrip_preserves_parent_order_and_derivation() {
        let parents = vec![sha1(9), sha1(2)];
        let origin = ChangeOrigin::git_synthesized(
            sha1(1),
            parents.clone(),
            crate::change::GitDerivation::MultiParent,
        )
        .unwrap();
        let change = Change::empty(ChangeHeader::new("Git synthesized"))
            .with_classification(ChangeKind::Durable, None, origin, CausalFrontier::empty())
            .unwrap();

        let mut bytes = Vec::new();
        let hash = change.serialize(&mut bytes).unwrap();
        let (loaded, loaded_hash) = Change::deserialize(&mut Cursor::new(&bytes)).unwrap();

        assert_eq!(loaded_hash, hash);
        assert_eq!(loaded.origin().git_parents().unwrap(), parents);
        assert_eq!(loaded.hashed, change.hashed);
    }

    #[test]
    fn v2_roundtrips_every_git_derivation() {
        use crate::change::GitDerivation;

        let cases = [
            (GitDerivation::Root, Vec::new()),
            (GitDerivation::FirstParent, vec![sha1(2)]),
            (GitDerivation::MultiParent, vec![sha1(2), sha1(3)]),
            (GitDerivation::Squash, vec![sha1(2)]),
            (GitDerivation::EmptyCommit, Vec::new()),
            (GitDerivation::RewriteCandidate, vec![sha1(2)]),
        ];

        for (derivation, parents) in cases {
            let origin = ChangeOrigin::git_synthesized(sha1(1), parents, derivation).unwrap();
            let change = Change::empty(ChangeHeader::new("derivation"))
                .with_classification(ChangeKind::Durable, None, origin, CausalFrontier::empty())
                .unwrap();
            let mut bytes = Vec::new();
            change.serialize(&mut bytes).unwrap();
            let (loaded, _) = Change::deserialize(&mut Cursor::new(&bytes)).unwrap();
            assert_eq!(loaded.origin(), change.origin());
        }
    }

    #[test]
    fn v2_hashes_ordered_git_parents_and_derivation() {
        use crate::change::GitDerivation;

        let timestamp = chrono::DateTime::parse_from_rfc3339("2026-09-06T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let header = ChangeHeader::builder()
            .message("ordered parents")
            .timestamp(timestamp)
            .build();
        let build = |parents, derivation| {
            Change::empty(header.clone())
                .with_classification(
                    ChangeKind::Durable,
                    None,
                    ChangeOrigin::git_synthesized(sha1(1), parents, derivation).unwrap(),
                    CausalFrontier::empty(),
                )
                .unwrap()
        };

        let ordered = build(vec![sha1(2), sha1(3)], GitDerivation::MultiParent);
        let reversed = build(vec![sha1(3), sha1(2)], GitDerivation::MultiParent);
        let squash = build(vec![sha1(2), sha1(3)], GitDerivation::Squash);

        assert_ne!(ordered.hash().unwrap(), reversed.hash().unwrap());
        assert_ne!(ordered.hash().unwrap(), squash.hash().unwrap());
    }

    #[test]
    fn v2_git_resolution_roundtrip_preserves_nonempty_frontier() {
        let parents = vec![sha1(8), sha1(3)];
        let origin = ChangeOrigin::git_resolution(sha1(1), parents.clone()).unwrap();
        let frontier =
            CausalFrontier::new(vec![Hash::from_bytes([10; 32]), Hash::from_bytes([11; 32])])
                .unwrap();
        let change = Change::empty(ChangeHeader::new("Git resolution"))
            .with_classification(ChangeKind::Durable, None, origin, frontier)
            .unwrap();

        let mut bytes = Vec::new();
        change.serialize(&mut bytes).unwrap();
        let (loaded, _) = Change::deserialize(&mut Cursor::new(&bytes)).unwrap();

        assert_eq!(loaded.origin().git_parents().unwrap(), parents);
        assert_eq!(loaded.causal_frontier(), change.causal_frontier());
    }

    #[test]
    fn v2_new_hashed_fields_mutate_the_object_hash() {
        let timestamp = chrono::DateTime::parse_from_rfc3339("2026-09-06T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let header = ChangeHeader::builder()
            .message("Hash mutation")
            .timestamp(timestamp)
            .build();
        let base = Change::empty(header.clone());
        let base_hash = base.hash().unwrap();

        let snapshot = Change::empty(header.clone())
            .with_classification(
                ChangeKind::Snapshot {
                    working_copy: WorkingCopyId::from_bytes([1; 16]),
                },
                None,
                ChangeOrigin::Native,
                CausalFrontier::empty(),
            )
            .unwrap();
        assert_ne!(snapshot.hash().unwrap(), base_hash);
        let other_owner = Change::empty(header.clone())
            .with_classification(
                ChangeKind::Snapshot {
                    working_copy: WorkingCopyId::from_bytes([2; 16]),
                },
                None,
                ChangeOrigin::Native,
                CausalFrontier::empty(),
            )
            .unwrap();
        assert_ne!(snapshot.hash().unwrap(), other_owner.hash().unwrap());

        let superseding = Change::empty(header.clone())
            .with_classification(
                ChangeKind::Snapshot {
                    working_copy: WorkingCopyId::from_bytes([1; 16]),
                },
                Some(Hash::from_bytes([2; 32])),
                ChangeOrigin::Native,
                CausalFrontier::empty(),
            )
            .unwrap();
        assert_ne!(superseding.hash().unwrap(), snapshot.hash().unwrap());

        let git = Change::empty(header.clone())
            .with_classification(
                ChangeKind::Durable,
                None,
                ChangeOrigin::git_synthesized(sha1(1), vec![], crate::change::GitDerivation::Root)
                    .unwrap(),
                CausalFrontier::empty(),
            )
            .unwrap();
        assert_ne!(git.hash().unwrap(), base_hash);

        let frontier = Change::empty(header.clone())
            .with_classification(
                ChangeKind::Durable,
                None,
                ChangeOrigin::git_resolution(sha1(1), vec![sha1(2), sha1(3)]).unwrap(),
                CausalFrontier::new(vec![Hash::from_bytes([3; 32])]).unwrap(),
            )
            .unwrap();
        assert_ne!(frontier.hash().unwrap(), base_hash);

        let mut extra_known = Change::empty(header.clone());
        extra_known.hashed.extra_known = vec![Hash::from_bytes([4; 32])];
        assert_ne!(extra_known.hash().unwrap(), base_hash);

        let mut metadata = Change::empty(header);
        metadata.hashed.metadata = b"hashed".to_vec();
        assert_ne!(metadata.hash().unwrap(), base_hash);
    }

    #[test]
    fn v2_validation_rejects_impossible_lifecycle_combinations() {
        assert!(matches!(
            Change::empty(ChangeHeader::new("invalid")).with_classification(
                ChangeKind::Durable,
                None,
                ChangeOrigin::Native,
                CausalFrontier::new(vec![Hash::from_bytes([1; 32])]).unwrap(),
            ),
            Err(ChangeValidationError::CausalFrontierRequiresGitResolution)
        ));

        let git_origin =
            ChangeOrigin::git_synthesized(sha1(1), vec![], crate::change::GitDerivation::Root)
                .unwrap();
        assert!(matches!(
            Change::empty(ChangeHeader::new("invalid")).with_classification(
                ChangeKind::Snapshot {
                    working_copy: WorkingCopyId::from_bytes([1; 16]),
                },
                None,
                git_origin,
                CausalFrontier::empty(),
            ),
            Err(ChangeValidationError::SnapshotMustBeNative)
        ));

        assert!(matches!(
            Change::empty(ChangeHeader::new("invalid")).with_classification(
                ChangeKind::Durable,
                Some(Hash::from_bytes([2; 32])),
                ChangeOrigin::Native,
                CausalFrontier::empty(),
            ),
            Err(ChangeValidationError::SupersedesRequiresSnapshot)
        ));

        let dependency = Hash::from_bytes([3; 32]);
        let change = Change::new(
            ChangeHeader::new("invalid"),
            vec![],
            vec![],
            vec![dependency],
        );
        assert!(matches!(
            change.with_classification(
                ChangeKind::Snapshot {
                    working_copy: WorkingCopyId::from_bytes([1; 16]),
                },
                Some(dependency),
                ChangeOrigin::Native,
                CausalFrontier::empty(),
            ),
            Err(ChangeValidationError::SupersedesDependency)
        ));

        let resolution = ChangeOrigin::git_resolution(sha1(1), vec![sha1(2), sha1(3)]).unwrap();
        assert!(matches!(
            Change::empty(ChangeHeader::new("invalid")).with_classification(
                ChangeKind::Durable,
                None,
                resolution,
                CausalFrontier::empty(),
            ),
            Err(ChangeValidationError::GitResolutionRequiresFrontier)
        ));

        let git_parent_digest = Hash::from_bytes([7; 32]);
        let change = Change::new(
            ChangeHeader::new("Git parent dependency"),
            vec![],
            vec![],
            vec![git_parent_digest],
        );
        assert!(matches!(
            change.with_classification(
                ChangeKind::Durable,
                None,
                ChangeOrigin::git_synthesized(
                    sha256(1),
                    vec![sha256(7)],
                    crate::change::GitDerivation::FirstParent,
                )
                .unwrap(),
                CausalFrontier::empty(),
            ),
            Err(ChangeValidationError::GitParentDependency { parent_index: 0 })
        ));
    }

    #[test]
    fn v2_validation_rejects_noncanonical_hash_lists_and_stale_contents_hash() {
        let mut change = Change::empty(ChangeHeader::new("unsorted dependencies"));
        change.hashed.dependencies = vec![Hash::from_bytes([2; 32]), Hash::from_bytes([1; 32])];
        assert!(matches!(change.hash(), Err(ChangeError::Validation(_))));

        let mut change = Change::empty(ChangeHeader::new("duplicate extra known"));
        let duplicate = Hash::from_bytes([1; 32]);
        change.hashed.extra_known = vec![duplicate, duplicate];
        assert!(matches!(change.hash(), Err(ChangeError::Validation(_))));

        let mut change = Change::empty(ChangeHeader::new("overlapping knowledge"));
        change.hashed.dependencies = vec![duplicate];
        change.hashed.extra_known = vec![duplicate];
        assert!(matches!(
            change.hash(),
            Err(ChangeError::Validation(
                ChangeValidationError::DependencyExtraKnownOverlap { .. }
            ))
        ));

        let mut change = Change::empty(ChangeHeader::new("stale content hash"));
        change
            .contents
            .extend_from_slice(b"changed without finalize");
        assert!(matches!(
            change.hash(),
            Err(ChangeError::ContentsHashMismatch { .. })
        ));
    }

    // Edge Cases

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

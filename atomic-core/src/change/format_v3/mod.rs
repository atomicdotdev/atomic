//! Change Format V3 — Streaming, Compressed, Postcard-Serialized Change Files.
//!
//! This module implements the V3 change file format for Atomic VCS, a clean-break
//! replacement for the V1/V2 bincode-based format. V3 is designed for:
//!
//! - **10x smaller change files** via hash deduplication + postcard varint encoding
//! - **Constant memory usage** via streaming section-based I/O
//! - **Parallel compression** of independent per-file sections
//! - **Selective loading** of graph-only or semantic-only layers
//! - **Content-defined chunking** for delta transfer of content blobs
//!
//! # File Layout
//!
//! A V3 change file has the following structure:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │  FileHeader           (64 bytes, fixed, uncompressed)           │
//! ├──────────────────────────────────────────────────────────────────┤
//! │  Hash Dedup Table     (N × 32 bytes, uncompressed)              │
//! │  ┌──────────────────────────────────────────────────────────┐   │
//! │  │ hash₀ = self hash  (always index 0)                      │   │
//! │  │ hash₁ = dependency 1                                     │   │
//! │  │ hash₂ = dependency 2                                     │   │
//! │  │ ...                                                       │   │
//! │  └──────────────────────────────────────────────────────────┘   │
//! ├──────────────────────────────────────────────────────────────────┤
//! │  Sections (each: SectionHeader + compressed payload)            │
//! │  ┌──────────────────────────────────────────────────────────┐   │
//! │  │ HEADER section       zstd(postcard(ChangeHeader))        │   │
//! │  │ DEPS section         zstd(postcard(Vec<HashIndex>))      │   │
//! │  │ PROVENANCE section   zstd(postcard(Vec<Provenance>))     │   │
//! │  │ GRAPH sections ×N    zstd(postcard(GraphSection))        │   │
//! │  │ SEMANTIC sections ×N zstd(postcard(SemanticSection))     │   │
//! │  │ CONTENT chunks ×M    zstd(raw bytes) + chunk metadata    │   │
//! │  │ UNHASHED section     zstd(json(Value))                   │   │
//! │  └──────────────────────────────────────────────────────────┘   │
//! ├──────────────────────────────────────────────────────────────────┤
//! │  Trailer               (32 bytes — blake3 content hash)         │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Innovation: Hash Deduplication
//!
//! In V1/V2, every `Position<Option<Hash>>` stores a full 32-byte hash plus a
//! 1-byte `Option` discriminant (33 bytes). An initial record of 194K LOC
//! produces ~500K position references, almost all referencing the **same** hash.
//! That's ~16 MB of redundant hash data before compression.
//!
//! V3 stores each unique hash **once** in a dedup table at the top of the file,
//! then references them by `u16` index throughout. With postcard's varint encoding,
//! index 0 (the most common — self-reference) takes just **1 byte**. A full
//! `Position` shrinks from 41 bytes (V2) to 2-5 bytes (V3).
//!
//! ```text
//! V2: Position<Option<Hash>> = Option<Hash>(33) + u64(8) = 41 bytes
//! V3: CompactPosition        = varint(u16)(1-3) + varint(u32)(1-5) = 2-8 bytes
//! Typical savings: 90%+
//! ```
//!
//! # Section Architecture
//!
//! Each section is independently compressed with zstd and tagged with a
//! [`SectionType`] byte. This enables:
//!
//! - **Thin pull**: Download only GRAPH + CONTENT sections to apply a change.
//!   Skip SEMANTIC entirely — reconstruct it locally on demand.
//! - **Thin review**: Download only SEMANTIC + CONTENT sections for code review.
//!   Never load graph operations.
//! - **Streaming apply**: Start applying GRAPH sections before SEMANTIC/CONTENT
//!   sections have arrived.
//! - **Parallel compression**: Each section compresses independently with rayon.
//! - **Incremental hashing**: Feed each compressed section through `blake3::Hasher`
//!   as it's written — no need to buffer the entire file in memory.
//!
//! # Serialization: Postcard vs Bincode
//!
//! V3 uses [postcard](https://docs.rs/postcard) instead of bincode for all
//! structured data. Postcard uses varint encoding, which is dramatically more
//! compact for the small integers that dominate change files:
//!
//! | Type | Bincode | Postcard | Savings |
//! |------|---------|----------|---------|
//! | `u64(0)` | 8 bytes | 1 byte | 87% |
//! | `u64(100)` | 8 bytes | 1 byte | 87% |
//! | `Vec<T>` length | 8 bytes | 1-3 bytes | 62-87% |
//! | `enum` discriminant | 4 bytes | 1 byte | 75% |
//!
//! # Module Structure
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`error`] | Error types ([`FormatError`], [`FormatResult`]) |
//! | [`types`] | Core types ([`HashIndex`], [`CompactPosition`], [`SectionType`], [`FileHeader`], etc.) |
//! | [`hash_table`] | Hash deduplication table ([`HashDedupTable`]) |
//!
//! # Backward Compatibility
//!
//! **None.** V3 is a clean break from V1/V2. There is no version detection,
//! no migration code, and no dual codepath. Old `.change` files must be
//! re-recorded. The `b"ATOM"` magic bytes and version field exist for
//! forward compatibility — if V4 is ever needed, readers can detect it.
//!
//! # Implementation Status
//!
//! - [x] **Phase 1**: Core types, hash dedup table, postcard serialization
//! - [ ] **Phase 2**: Per-file sections, parallel compression, streaming I/O
//! - [ ] **Phase 3**: ChangeWriter / ChangeReader with incremental hashing
//! - [ ] **Phase 4**: Content-defined chunking (FastCDC)
//! - [ ] **Phase 5**: Streaming push/pull protocol integration
//!
//! # Examples
//!
//! ## Hash Dedup Table
//!
//! ```rust
//! use atomic_core::change::format_v3::{HashDedupTable, HASH_INDEX_SELF};
//!
//! // Create a table with the change's own hash
//! let self_hash = blake3::hash(b"my change content").as_bytes().to_owned();
//! let mut table = HashDedupTable::new(self_hash);
//!
//! // Register dependency hashes
//! let dep_hash = blake3::hash(b"dependency change").as_bytes().to_owned();
//! let dep_index = table.insert(dep_hash).unwrap();
//! assert_eq!(dep_index, 1); // self=0, first dep=1
//!
//! // Look up indices for compact serialization
//! assert_eq!(table.lookup(&self_hash), Some(HASH_INDEX_SELF));
//! assert_eq!(table.lookup(&dep_hash), Some(1));
//! ```
//!
//! ## Compact Positions
//!
//! ```rust
//! use atomic_core::change::format_v3::{CompactPosition, HASH_INDEX_SELF};
//!
//! // Self-referencing position (most common case)
//! let pos = CompactPosition::self_ref(42);
//! assert_eq!(pos.change, HASH_INDEX_SELF); // index 0
//! assert_eq!(pos.pos, 42);
//!
//! // Postcard encodes this as just 2 bytes (vs 41 bytes in V2)
//! let bytes = postcard::to_allocvec(&pos).unwrap();
//! assert_eq!(bytes.len(), 2);
//! ```
//!
//! ## File Header
//!
//! ```rust
//! use atomic_core::change::format_v3::{FileHeader, FileHeaderFlags};
//!
//! let header = FileHeader::builder()
//!     .hash_table_entries(5)
//!     .graph_section_count(3)
//!     .semantic_section_count(3)
//!     .contents_chunks(10)
//!     .total_uncompressed(1024 * 1024)
//!     .with_provenance()
//!     .build();
//!
//! // Serialize to exactly 64 bytes
//! let bytes = header.to_bytes();
//! assert_eq!(bytes.len(), 64);
//! assert_eq!(&bytes[0..4], b"ATOM");
//!
//! // Deserialize and validate
//! let decoded = FileHeader::from_bytes(&bytes).unwrap();
//! assert_eq!(decoded.graph_section_count, 3);
//! assert!(decoded.flags.has(FileHeaderFlags::HAS_SEMANTIC));
//! ```
//!
//! # References
//!
//! - [Proposal: Change Format V2](../../docs/PROPOSAL-CHANGE-FORMAT-V2.md) — Full design document
//! - [postcard crate](https://docs.rs/postcard) — Varint serialization
//! - [blake3 incremental hashing](https://docs.rs/blake3/latest/blake3/struct.Hasher.html)
//! - [zstd streaming](https://docs.rs/zstd/latest/zstd/stream/) — Per-section compression

pub mod chunking;
pub mod compact;
pub mod error;
pub mod hash_table;
pub mod reader;
pub mod sections;
pub mod types;
pub mod writer;

// ── Re-exports ─────────────────────────────────────────────────────────
//
// All public types are re-exported at the module level for ergonomic imports:
//
//   use atomic_core::change::format_v3::{HashDedupTable, CompactPosition, ...};
//
// Instead of:
//
//   use atomic_core::change::format_v3::hash_table::HashDedupTable;
//   use atomic_core::change::format_v3::types::CompactPosition;

// Error types
pub use error::{FormatError, FormatResult, FORMAT_VERSION, MAGIC, MAX_HASH_TABLE_ENTRIES};

// Core types
pub use types::{
    CompactPosition, ContentChunkHeader, FileHeader, FileHeaderBuilder, FileHeaderFlags,
    SectionHeader, SectionType, Trailer,
};

// Hash index types and helpers
pub use types::{is_none_index, HashIndex, HASH_INDEX_NONE, HASH_INDEX_SELF};

// Hash dedup table
pub use hash_table::{HashDedupTable, HashDedupTableStats};

// Compact graph types
pub use compact::{
    CompactAtom, CompactEdgeUpdate, CompactGraphNode, CompactGraphOp, CompactInsertion,
    CompactNewEdge, Compactor,
};

// Section payloads
pub use sections::{GraphSectionPayload, SectionPair, SemanticSectionPayload};

// Content chunking
pub use chunking::{
    chunk_content, chunk_content_with_stats, compress_chunks_parallel,
    compress_chunks_parallel_with_stats, ChunkingOptions, ChunkingStats, CompressedChunk,
    CompressionStats, ContentChunk,
};

// Writer
pub use writer::{ChangeWriter, WriteOutcome, WriterOptions, WriterStats};

// Reader
pub use reader::{ChangeReader, ContentChunkInfo, ReadSection, ReaderStats};

#[cfg(test)]
mod tests {
    use super::*;

    // ── Module-level integration tests ─────────────────────────────
    //
    // These tests verify that the public API works together correctly
    // across module boundaries. Unit tests for individual types are
    // in their respective modules (error.rs, types.rs, hash_table.rs,
    // writer.rs, reader.rs).

    #[test]
    fn test_all_public_types_are_exported() {
        // Verify that all expected types are accessible from the module root.
        // This test exists to catch accidental removal of re-exports.

        // Error types
        let _: FormatError = FormatError::HashTableFull;
        let _: FormatResult<()> = Ok(());

        // Constants
        assert_eq!(MAGIC, *b"ATOM");
        assert_eq!(FORMAT_VERSION, 1);
        assert_eq!(MAX_HASH_TABLE_ENTRIES, 65534);

        // Hash index types
        let _: HashIndex = HASH_INDEX_SELF;
        let _: HashIndex = HASH_INDEX_NONE;
        assert!(!is_none_index(HASH_INDEX_SELF));
        assert!(is_none_index(HASH_INDEX_NONE));

        // Core types
        let _ = CompactPosition::self_ref(0);
        let _ = SectionType::Header;
        let _ = FileHeaderFlags::NONE;
        let _ = FileHeader::default();
        let _ = FileHeader::builder();
        let _ = SectionHeader::new(SectionType::Graph, 0);
        let _ = ContentChunkHeader::new(0, [0u8; 32], 0, 0);
        let _ = Trailer {
            content_hash: [0u8; 32],
        };

        // Hash dedup table
        let table = HashDedupTable::new([0u8; 32]);
        let _ = table.stats();

        // Writer types
        let _ = WriterOptions::default();
        let _ = WriterStats::default();

        // Reader types
        let _ = ReaderStats::default();

        // Compact types
        let _ = CompactGraphNode::self_ref(0, 0);
        let _ = CompactPosition::self_ref(0); // already tested above, but verifies re-export
        let _: CompactAtom = CompactAtom::EdgeUpdate(CompactEdgeUpdate {
            edges: vec![],
            inode: CompactPosition::root(0),
        });

        // Section payload types
        let _ = GraphSectionPayload::new("test".to_string(), vec![], 0, 0);

        // Chunking types
        let _ = ChunkingOptions::default();
        let chunks = chunk_content(b"hello", &ChunkingOptions::default());
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_end_to_end_header_and_hash_table() {
        // Simulate writing a file header + hash table, then reading them back.
        // This tests that the types compose correctly for I/O.

        let self_hash = *blake3::hash(b"change content").as_bytes();
        let dep1 = *blake3::hash(b"dependency 1").as_bytes();
        let dep2 = *blake3::hash(b"dependency 2").as_bytes();

        // Build the hash table
        let mut hash_table = HashDedupTable::new(self_hash);
        hash_table.insert(dep1).unwrap();
        hash_table.insert(dep2).unwrap();

        // Build the file header
        let header = FileHeader::builder()
            .hash_table_entries(hash_table.len() as u32)
            .graph_section_count(2)
            .semantic_section_count(2)
            .contents_chunks(5)
            .total_uncompressed(10000)
            .with_provenance()
            .build();

        // Validate the header
        header.validate().unwrap();
        assert_eq!(header.hash_table_entries, 3);
        assert!(header.flags.has(FileHeaderFlags::HAS_PROVENANCE));
        assert!(header.flags.has(FileHeaderFlags::HAS_SEMANTIC));

        // Write header + hash table
        let mut buf = Vec::new();
        header.write_to(&mut buf).unwrap();
        hash_table.write_to(&mut buf).unwrap();

        // Expected size: 64 (header) + 3×32 (hashes) = 160 bytes
        assert_eq!(buf.len(), 64 + 3 * 32);

        // Read them back
        let mut cursor = std::io::Cursor::new(&buf);
        let loaded_header = FileHeader::read_from(&mut cursor).unwrap();
        let loaded_table =
            HashDedupTable::read_from(&mut cursor, loaded_header.hash_table_entries).unwrap();

        // Verify everything matches
        assert_eq!(loaded_header, header);
        assert_eq!(loaded_table.len(), 3);
        assert_eq!(loaded_table.resolve(0).unwrap(), &self_hash);
        assert_eq!(loaded_table.resolve(1).unwrap(), &dep1);
        assert_eq!(loaded_table.resolve(2).unwrap(), &dep2);
    }

    #[test]
    fn test_compact_position_with_hash_table() {
        // Verify that CompactPosition indices work with HashDedupTable lookups.

        let self_hash = *blake3::hash(b"self").as_bytes();
        let dep_hash = *blake3::hash(b"dep").as_bytes();

        let mut table = HashDedupTable::new(self_hash);
        let dep_idx = table.insert(dep_hash).unwrap();

        // Create positions referencing hashes by index
        let self_pos = CompactPosition::new(HASH_INDEX_SELF, 100);
        let dep_pos = CompactPosition::new(dep_idx, 200);
        let root_pos = CompactPosition::root(0);

        // Resolve positions back to hashes via the table
        let resolved_self = table.resolve(self_pos.change).unwrap();
        assert_eq!(resolved_self, &self_hash);

        let resolved_dep = table.resolve(dep_pos.change).unwrap();
        assert_eq!(resolved_dep, &dep_hash);

        // Root position (HASH_INDEX_NONE) resolves to None
        assert_eq!(table.resolve(root_pos.change), None);
    }

    #[test]
    fn test_section_types_cover_all_file_regions() {
        // Verify that every section type has unique byte values and that
        // the byte ranges don't overlap.

        let all_types = [
            SectionType::Header,
            SectionType::Dependencies,
            SectionType::Provenance,
            SectionType::Graph,
            SectionType::Semantic,
            SectionType::Content,
            SectionType::Unhashed,
        ];

        // All byte values should be unique
        let mut bytes: Vec<u8> = all_types.iter().map(|t| t.to_byte()).collect();
        let original_len = bytes.len();
        bytes.sort();
        bytes.dedup();
        assert_eq!(
            bytes.len(),
            original_len,
            "section type bytes must be unique"
        );

        // All names should be unique
        let mut names: Vec<&str> = all_types.iter().map(|t| t.name()).collect();
        let original_len = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            original_len,
            "section type names must be unique"
        );

        // Ordering should be strictly increasing
        for i in 1..all_types.len() {
            assert!(
                all_types[i - 1].ordering() < all_types[i].ordering(),
                "{} must come before {} in section ordering",
                all_types[i - 1].name(),
                all_types[i].name()
            );
        }
    }

    #[test]
    fn test_postcard_size_savings_demonstration() {
        // Demonstrate the concrete size savings of postcard + hash dedup
        // for a realistic set of positions.
        //
        // Scenario: initial record of a file — all positions reference
        // the change itself (index 0) at various byte offsets.

        let positions: Vec<CompactPosition> = (0..1000u32)
            .map(|i| CompactPosition::self_ref(i * 100))
            .collect();

        // Serialize all positions with postcard
        let serialized = postcard::to_allocvec(&positions).unwrap();

        // V2 equivalent would be: 1000 × 41 bytes = 41,000 bytes
        let v2_size = 1000 * 41;

        // V3 should be dramatically smaller
        let v3_size = serialized.len();

        let savings_pct = (1.0 - (v3_size as f64 / v2_size as f64)) * 100.0;

        assert!(
            v3_size < v2_size / 5,
            "V3 ({} bytes) should be at least 5x smaller than V2 ({} bytes), savings: {:.1}%",
            v3_size,
            v2_size,
            savings_pct
        );
    }

    #[test]
    fn test_error_types_are_useful() {
        // Verify that errors carry enough context for debugging.

        // Invalid magic
        let err = FormatError::InvalidMagic { got: *b"GITX" };
        assert!(err.is_format_error());
        assert!(err.suggestion().is_some());
        // Debug format shows byte values, not ASCII
        assert!(err.to_string().contains("invalid magic bytes"));

        // Hash table full
        let err = FormatError::HashTableFull;
        assert!(err.is_hash_error());
        assert!(err.suggestion().is_some());

        // I/O error preserves kind
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broke");
        let err: FormatError = io_err.into();
        assert!(err.is_io());
    }

    #[test]
    fn test_trailer_with_incremental_hash() {
        // Simulate incremental hashing of sections, then writing a trailer.
        // This is the pattern used by ChangeWriter (future Phase 2).

        let mut hasher = blake3::Hasher::new();

        // Simulate feeding compressed section data through the hasher
        let section1_data = b"compressed graph section data";
        let section2_data = b"compressed semantic section data";
        let content_data = b"compressed content chunk";

        hasher.update(section1_data);
        hasher.update(section2_data);
        hasher.update(content_data);

        // Finalize the hash
        let hash = hasher.finalize();
        let trailer = Trailer {
            content_hash: *hash.as_bytes(),
        };

        // Roundtrip through bytes
        let mut buf = Vec::new();
        trailer.write_to(&mut buf).unwrap();
        assert_eq!(buf.len(), 32);

        let mut cursor = std::io::Cursor::new(&buf);
        let loaded = Trailer::read_from(&mut cursor).unwrap();
        assert_eq!(loaded.content_hash, trailer.content_hash);

        // Verify the hash is deterministic
        let mut hasher2 = blake3::Hasher::new();
        hasher2.update(section1_data);
        hasher2.update(section2_data);
        hasher2.update(content_data);
        assert_eq!(*hasher2.finalize().as_bytes(), trailer.content_hash);
    }

    #[test]
    fn test_minimal_valid_change_file_bytes() {
        // Write the absolute minimal valid V3 change file structure:
        // FileHeader + HashTable(1 entry) + 2 section headers + Trailer
        //
        // This doesn't include real compressed payloads — it's just testing
        // that the framing types compose correctly for a complete file.

        let self_hash = *blake3::hash(b"minimal change").as_bytes();
        let hash_table = HashDedupTable::new(self_hash);

        let header = FileHeader::builder()
            .hash_table_entries(1)
            .graph_section_count(0)
            .build();

        let mut buf = Vec::new();

        // 1. File header (64 bytes)
        header.write_to(&mut buf).unwrap();

        // 2. Hash table (32 bytes)
        hash_table.write_to(&mut buf).unwrap();

        // 3. HEADER section (5-byte frame + 0 payload — just testing framing)
        let header_section = SectionHeader::new(SectionType::Header, 0);
        header_section.write_to(&mut buf).unwrap();

        // 4. DEPS section (5-byte frame + 0 payload)
        let deps_section = SectionHeader::new(SectionType::Dependencies, 0);
        deps_section.write_to(&mut buf).unwrap();

        // 5. Trailer (32 bytes)
        let trailer = Trailer {
            content_hash: self_hash,
        };
        trailer.write_to(&mut buf).unwrap();

        // Total: 64 + 32 + 5 + 5 + 32 = 138 bytes
        assert_eq!(buf.len(), 138);

        // Read it back
        let mut cursor = std::io::Cursor::new(&buf);

        let loaded_header = FileHeader::read_from(&mut cursor).unwrap();
        assert_eq!(loaded_header.hash_table_entries, 1);

        let loaded_table =
            HashDedupTable::read_from(&mut cursor, loaded_header.hash_table_entries).unwrap();
        assert_eq!(loaded_table.self_hash().unwrap(), &self_hash);

        let loaded_section1 = SectionHeader::read_from(&mut cursor).unwrap();
        assert_eq!(loaded_section1.section_type, SectionType::Header);
        assert_eq!(loaded_section1.compressed_len, 0);

        let loaded_section2 = SectionHeader::read_from(&mut cursor).unwrap();
        assert_eq!(loaded_section2.section_type, SectionType::Dependencies);
        assert_eq!(loaded_section2.compressed_len, 0);

        let loaded_trailer = Trailer::read_from(&mut cursor).unwrap();
        assert_eq!(loaded_trailer.content_hash, self_hash);
    }
}

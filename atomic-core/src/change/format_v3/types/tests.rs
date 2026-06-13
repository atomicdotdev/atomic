//! Tests for V3 format core types.

use super::builder::Trailer;
use super::hash_index::{is_none_index, CompactPosition, HASH_INDEX_NONE, HASH_INDEX_SELF};
use super::header::{FileHeader, FileHeaderFlags};
use super::section::{ContentChunkHeader, SectionHeader, SectionType};
use crate::change::format_v3::error::{FormatError, FORMAT_VERSION, MAGIC, MAX_HASH_TABLE_ENTRIES};

// ── HashIndex ──────────────────────────────────────────────────

#[test]
fn test_hash_index_none_value() {
    assert_eq!(HASH_INDEX_NONE, 0xFFFF);
    assert_eq!(HASH_INDEX_NONE, u16::MAX);
}

#[test]
fn test_hash_index_self_value() {
    assert_eq!(HASH_INDEX_SELF, 0);
}

#[test]
fn test_is_none_index() {
    assert!(is_none_index(HASH_INDEX_NONE));
    assert!(!is_none_index(HASH_INDEX_SELF));
    assert!(!is_none_index(1));
    assert!(!is_none_index(0xFFFE));
}

#[test]
fn test_hash_index_range() {
    // Valid indices: 0 through 0xFFFE (65534 values)
    // Reserved: 0xFFFF (NONE sentinel)
    assert_eq!(MAX_HASH_TABLE_ENTRIES, 65534);
    assert_eq!(HASH_INDEX_NONE as usize, MAX_HASH_TABLE_ENTRIES + 1);
}

// ── CompactPosition ───────────────────────────────────────────

#[test]
fn test_compact_position_new() {
    let pos = CompactPosition::new(5, 100);
    assert_eq!(pos.change, 5);
    assert_eq!(pos.pos, 100);
}

#[test]
fn test_compact_position_root() {
    let pos = CompactPosition::root(42);
    assert_eq!(pos.change, HASH_INDEX_NONE);
    assert_eq!(pos.pos, 42);
    assert!(pos.is_root());
    assert!(!pos.is_self_ref());
}

#[test]
fn test_compact_position_self_ref() {
    let pos = CompactPosition::self_ref(99);
    assert_eq!(pos.change, HASH_INDEX_SELF);
    assert_eq!(pos.pos, 99);
    assert!(!pos.is_root());
    assert!(pos.is_self_ref());
}

#[test]
fn test_compact_position_dependency_ref() {
    let pos = CompactPosition::new(3, 500);
    assert!(!pos.is_root());
    assert!(!pos.is_self_ref());
    assert_eq!(pos.change, 3);
    assert_eq!(pos.pos, 500);
}

#[test]
fn test_compact_position_display_root() {
    let pos = CompactPosition::root(0);
    assert_eq!(format!("{}", pos), "ROOT:0");

    let pos = CompactPosition::root(42);
    assert_eq!(format!("{}", pos), "ROOT:42");
}

#[test]
fn test_compact_position_display_self() {
    let pos = CompactPosition::self_ref(100);
    assert_eq!(format!("{}", pos), "SELF:100");
}

#[test]
fn test_compact_position_display_dependency() {
    let pos = CompactPosition::new(7, 256);
    assert_eq!(format!("{}", pos), "#7:256");
}

#[test]
fn test_compact_position_equality() {
    let a = CompactPosition::new(1, 10);
    let b = CompactPosition::new(1, 10);
    let c = CompactPosition::new(1, 20);
    let d = CompactPosition::new(2, 10);

    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_ne!(a, d);
}

#[test]
fn test_compact_position_ordering() {
    let positions = vec![
        CompactPosition::new(2, 10),
        CompactPosition::new(1, 20),
        CompactPosition::new(1, 10),
        CompactPosition::root(0),
    ];

    let mut sorted = positions.clone();
    sorted.sort();

    // Sorted by change index first, then pos
    assert_eq!(sorted[0], CompactPosition::new(1, 10));
    assert_eq!(sorted[1], CompactPosition::new(1, 20));
    assert_eq!(sorted[2], CompactPosition::new(2, 10));
    // HASH_INDEX_NONE (0xFFFF) sorts last
    assert_eq!(sorted[3], CompactPosition::root(0));
}

#[test]
fn test_compact_position_hash() {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    set.insert(CompactPosition::self_ref(10));
    set.insert(CompactPosition::self_ref(10)); // duplicate
    set.insert(CompactPosition::self_ref(20));
    assert_eq!(set.len(), 2);
}

#[test]
fn test_compact_position_postcard_roundtrip() {
    let positions = vec![
        CompactPosition::root(0),
        CompactPosition::self_ref(0),
        CompactPosition::self_ref(42),
        CompactPosition::new(1, 100),
        CompactPosition::new(100, 10000),
        CompactPosition::new(0xFFFE, u32::MAX),
    ];

    for pos in &positions {
        let bytes = postcard::to_allocvec(pos).expect("serialize");
        let decoded: CompactPosition = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(*pos, decoded, "roundtrip failed for {:?}", pos);
    }
}

#[test]
fn test_compact_position_postcard_size() {
    // Index 0, pos 0 → both varint(0) = 1 byte each → 2 bytes total
    let small = CompactPosition::self_ref(0);
    let bytes = postcard::to_allocvec(&small).unwrap();
    assert_eq!(bytes.len(), 2, "SELF:0 should be 2 bytes in postcard");

    // Index 0, pos 42 → varint(0) + varint(42) = 1 + 1 = 2 bytes
    let medium = CompactPosition::self_ref(42);
    let bytes = postcard::to_allocvec(&medium).unwrap();
    assert_eq!(bytes.len(), 2, "SELF:42 should be 2 bytes in postcard");

    // Index 0, pos 200 → varint(0) + varint(200) = 1 + 2 = 3 bytes
    let larger = CompactPosition::self_ref(200);
    let bytes = postcard::to_allocvec(&larger).unwrap();
    assert!(bytes.len() <= 3, "SELF:200 should be at most 3 bytes");

    // Compare with what bincode Option<Hash> + u64 would cost: 33 + 8 = 41 bytes
    // We're at 2-3 bytes. That's a 90%+ reduction.
}

#[test]
fn test_compact_position_max_values() {
    // Maximum valid index (0xFFFE) and maximum pos (u32::MAX)
    let max = CompactPosition::new(0xFFFE, u32::MAX);
    let bytes = postcard::to_allocvec(&max).unwrap();
    let decoded: CompactPosition = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(max, decoded);
    // Even max values should be reasonable size (3 + 5 = 8 bytes max)
    assert!(bytes.len() <= 8);
}

// ── SectionType ───────────────────────────────────────────────

#[test]
fn test_section_type_byte_values() {
    assert_eq!(SectionType::Header.to_byte(), 0x01);
    assert_eq!(SectionType::Dependencies.to_byte(), 0x02);
    assert_eq!(SectionType::Provenance.to_byte(), 0x03);
    assert_eq!(SectionType::Graph.to_byte(), 0x10);
    assert_eq!(SectionType::Content.to_byte(), 0x20);
    assert_eq!(SectionType::Semantic.to_byte(), 0x30);
    assert_eq!(SectionType::Unhashed.to_byte(), 0xF0);
}

#[test]
fn test_section_type_from_byte_roundtrip() {
    let types = [
        SectionType::Header,
        SectionType::Dependencies,
        SectionType::Provenance,
        SectionType::Graph,
        SectionType::Content,
        SectionType::Semantic,
        SectionType::Unhashed,
    ];

    for st in &types {
        let byte = st.to_byte();
        let decoded = SectionType::from_byte(byte).unwrap();
        assert_eq!(*st, decoded, "roundtrip failed for {:?}", st);
    }
}

#[test]
fn test_section_type_from_byte_invalid() {
    // Test some invalid bytes
    for byte in [0x00, 0x04, 0x0F, 0x11, 0x21, 0x31, 0xFF] {
        let result = SectionType::from_byte(byte);
        assert!(result.is_err(), "byte 0x{:02X} should be invalid", byte);
    }
}

#[test]
fn test_section_type_is_hashed() {
    assert!(SectionType::Header.is_hashed());
    assert!(SectionType::Dependencies.is_hashed());
    assert!(SectionType::Provenance.is_hashed());
    assert!(SectionType::Graph.is_hashed());
    assert!(SectionType::Content.is_hashed());
    assert!(SectionType::Semantic.is_hashed());

    // Only Unhashed is not hashed
    assert!(!SectionType::Unhashed.is_hashed());
}

#[test]
fn test_section_type_is_metadata() {
    assert!(SectionType::Header.is_metadata());
    assert!(SectionType::Dependencies.is_metadata());
    assert!(SectionType::Provenance.is_metadata());

    assert!(!SectionType::Graph.is_metadata());
    assert!(!SectionType::Content.is_metadata());
    assert!(!SectionType::Semantic.is_metadata());
    assert!(!SectionType::Unhashed.is_metadata());
}

#[test]
fn test_section_type_is_per_file() {
    assert!(SectionType::Graph.is_per_file());
    assert!(SectionType::Semantic.is_per_file());

    assert!(!SectionType::Header.is_per_file());
    assert!(!SectionType::Dependencies.is_per_file());
    assert!(!SectionType::Provenance.is_per_file());
    assert!(!SectionType::Content.is_per_file());
    assert!(!SectionType::Unhashed.is_per_file());
}

#[test]
fn test_section_type_name() {
    assert_eq!(SectionType::Header.name(), "HEADER");
    assert_eq!(SectionType::Dependencies.name(), "DEPS");
    assert_eq!(SectionType::Provenance.name(), "PROVENANCE");
    assert_eq!(SectionType::Graph.name(), "GRAPH");
    assert_eq!(SectionType::Content.name(), "CONTENT");
    assert_eq!(SectionType::Semantic.name(), "SEMANTIC");
    assert_eq!(SectionType::Unhashed.name(), "UNHASHED");
}

#[test]
fn test_section_type_display() {
    assert_eq!(format!("{}", SectionType::Graph), "GRAPH");
    assert_eq!(format!("{}", SectionType::Unhashed), "UNHASHED");
}

#[test]
fn test_section_type_ordering() {
    // Sections must have strictly increasing ordering values
    let ordered = [
        SectionType::Header,
        SectionType::Dependencies,
        SectionType::Provenance,
        SectionType::Graph,
        SectionType::Semantic,
        SectionType::Content,
        SectionType::Unhashed,
    ];

    for i in 1..ordered.len() {
        assert!(
            ordered[i - 1].ordering() < ordered[i].ordering(),
            "{} (ordering {}) should come before {} (ordering {})",
            ordered[i - 1].name(),
            ordered[i - 1].ordering(),
            ordered[i].name(),
            ordered[i].ordering(),
        );
    }
}

#[test]
fn test_section_type_postcard_roundtrip() {
    let types = [
        SectionType::Header,
        SectionType::Dependencies,
        SectionType::Provenance,
        SectionType::Graph,
        SectionType::Content,
        SectionType::Semantic,
        SectionType::Unhashed,
    ];

    for st in &types {
        let bytes = postcard::to_allocvec(st).unwrap();
        let decoded: SectionType = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(*st, decoded);
    }
}

// ── FileHeaderFlags ───────────────────────────────────────────

#[test]
fn test_flags_none() {
    let flags = FileHeaderFlags::NONE;
    assert!(flags.is_empty());
    assert!(!flags.has(FileHeaderFlags::HAS_PROVENANCE));
    assert!(!flags.has(FileHeaderFlags::HAS_SEMANTIC));
    assert!(!flags.has(FileHeaderFlags::HAS_UNHASHED));
    assert!(!flags.has_unknown_flags());
}

#[test]
fn test_flags_set_and_clear() {
    let mut flags = FileHeaderFlags::NONE;

    flags.set(FileHeaderFlags::HAS_PROVENANCE);
    assert!(flags.has(FileHeaderFlags::HAS_PROVENANCE));
    assert!(!flags.has(FileHeaderFlags::HAS_SEMANTIC));

    flags.set(FileHeaderFlags::HAS_SEMANTIC);
    assert!(flags.has(FileHeaderFlags::HAS_PROVENANCE));
    assert!(flags.has(FileHeaderFlags::HAS_SEMANTIC));

    flags.clear(FileHeaderFlags::HAS_PROVENANCE);
    assert!(!flags.has(FileHeaderFlags::HAS_PROVENANCE));
    assert!(flags.has(FileHeaderFlags::HAS_SEMANTIC));
}

#[test]
fn test_flags_raw_roundtrip() {
    let mut flags = FileHeaderFlags::NONE;
    flags.set(FileHeaderFlags::HAS_PROVENANCE);
    flags.set(FileHeaderFlags::HAS_UNHASHED);

    let raw = flags.to_raw();
    let decoded = FileHeaderFlags::from_raw(raw);
    assert_eq!(flags, decoded);
}

#[test]
fn test_flags_unknown_bits() {
    let flags = FileHeaderFlags::from_raw(0xFF00_0000);
    assert!(flags.has_unknown_flags());
    assert!(!flags.has(FileHeaderFlags::HAS_PROVENANCE));
}

#[test]
fn test_flags_display_none() {
    let flags = FileHeaderFlags::NONE;
    assert_eq!(format!("{}", flags), "(none)");
}

#[test]
fn test_flags_display_some() {
    let mut flags = FileHeaderFlags::NONE;
    flags.set(FileHeaderFlags::HAS_PROVENANCE);
    flags.set(FileHeaderFlags::HAS_SEMANTIC);
    let display = format!("{}", flags);
    assert!(display.contains("PROVENANCE"));
    assert!(display.contains("SEMANTIC"));
}

// ── FileHeader ────────────────────────────────────────────────

#[test]
fn test_file_header_size() {
    assert_eq!(FileHeader::SIZE, 64);
}

#[test]
fn test_file_header_default() {
    let header = FileHeader::default();
    assert_eq!(header.magic, MAGIC);
    assert_eq!(header.version, FORMAT_VERSION);
    assert!(header.flags.is_empty());
    assert_eq!(header.hash_table_entries, 0);
    assert_eq!(header.graph_section_count, 0);
    assert_eq!(header.semantic_section_count, 0);
    assert_eq!(header.contents_chunks, 0);
    assert_eq!(header.total_uncompressed, 0);
    assert_eq!(header.reserved, [0u8; 28]);
}

#[test]
fn test_file_header_to_bytes_magic() {
    let header = FileHeader::default();
    let bytes = header.to_bytes();
    assert_eq!(&bytes[0..4], b"ATOM");
}

#[test]
fn test_file_header_to_bytes_version() {
    let header = FileHeader::default();
    let bytes = header.to_bytes();
    let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    assert_eq!(version, FORMAT_VERSION);
}

#[test]
fn test_file_header_roundtrip() {
    let header = FileHeader::builder()
        .hash_table_entries(10)
        .graph_section_count(5)
        .semantic_section_count(5)
        .contents_chunks(20)
        .total_uncompressed(1024 * 1024)
        .with_provenance()
        .with_unhashed()
        .build();

    let bytes = header.to_bytes();
    assert_eq!(bytes.len(), FileHeader::SIZE);

    let decoded = FileHeader::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.magic, header.magic);
    assert_eq!(decoded.version, header.version);
    assert_eq!(decoded.flags, header.flags);
    assert_eq!(decoded.hash_table_entries, header.hash_table_entries);
    assert_eq!(decoded.graph_section_count, header.graph_section_count);
    assert_eq!(
        decoded.semantic_section_count,
        header.semantic_section_count
    );
    assert_eq!(decoded.contents_chunks, header.contents_chunks);
    assert_eq!(decoded.total_uncompressed, header.total_uncompressed);
    assert_eq!(decoded.reserved, header.reserved);
}

#[test]
fn test_file_header_from_bytes_invalid_magic() {
    let mut bytes = FileHeader::default().to_bytes();
    bytes[0] = b'X'; // corrupt magic

    let result = FileHeader::from_bytes(&bytes);
    assert!(result.is_err());
    if let Err(FormatError::InvalidMagic { got }) = result {
        assert_eq!(got[0], b'X');
    } else {
        panic!("expected InvalidMagic error");
    }
}

#[test]
fn test_file_header_from_bytes_wrong_version() {
    let mut bytes = FileHeader::default().to_bytes();
    // Set version to 99
    bytes[4..8].copy_from_slice(&99u32.to_le_bytes());

    let result = FileHeader::from_bytes(&bytes);
    assert!(result.is_err());
    if let Err(FormatError::UnsupportedVersion { expected, got }) = result {
        assert_eq!(expected, FORMAT_VERSION);
        assert_eq!(got, 99);
    } else {
        panic!("expected UnsupportedVersion error");
    }
}

#[test]
fn test_file_header_io_roundtrip() {
    let header = FileHeader::builder()
        .hash_table_entries(3)
        .graph_section_count(2)
        .build();

    let mut buf = Vec::new();
    header.write_to(&mut buf).unwrap();
    assert_eq!(buf.len(), FileHeader::SIZE);

    let mut cursor = std::io::Cursor::new(&buf);
    let decoded = FileHeader::read_from(&mut cursor).unwrap();
    assert_eq!(decoded.hash_table_entries, 3);
    assert_eq!(decoded.graph_section_count, 2);
}

#[test]
fn test_file_header_read_truncated() {
    // Only 10 bytes — not enough for a full header
    let buf = [0u8; 10];
    let mut cursor = std::io::Cursor::new(&buf[..]);
    let result = FileHeader::read_from(&mut cursor);
    assert!(result.is_err());
}

// ── FileHeaderBuilder ─────────────────────────────────────────

#[test]
fn test_builder_default() {
    let header = FileHeader::builder().build();
    assert_eq!(header.magic, MAGIC);
    assert_eq!(header.version, FORMAT_VERSION);
    assert_eq!(header.hash_table_entries, 0);
    assert_eq!(header.graph_section_count, 0);
    assert_eq!(header.semantic_section_count, 0);
    assert_eq!(header.contents_chunks, 0);
    assert_eq!(header.total_uncompressed, 0);
    assert!(header.flags.is_empty());
}

#[test]
fn test_builder_auto_flags_semantic() {
    let header = FileHeader::builder().semantic_section_count(3).build();
    assert!(header.flags.has(FileHeaderFlags::HAS_SEMANTIC));

    let header = FileHeader::builder().semantic_section_count(0).build();
    assert!(!header.flags.has(FileHeaderFlags::HAS_SEMANTIC));
}

#[test]
fn test_builder_auto_flags_provenance() {
    let header = FileHeader::builder().with_provenance().build();
    assert!(header.flags.has(FileHeaderFlags::HAS_PROVENANCE));

    let header = FileHeader::builder().build();
    assert!(!header.flags.has(FileHeaderFlags::HAS_PROVENANCE));
}

#[test]
fn test_builder_auto_flags_unhashed() {
    let header = FileHeader::builder().with_unhashed().build();
    assert!(header.flags.has(FileHeaderFlags::HAS_UNHASHED));
}

#[test]
fn test_builder_chaining() {
    let header = FileHeader::builder()
        .hash_table_entries(5)
        .graph_section_count(10)
        .semantic_section_count(10)
        .contents_chunks(50)
        .total_uncompressed(5_000_000)
        .with_provenance()
        .with_unhashed()
        .build();

    assert_eq!(header.hash_table_entries, 5);
    assert_eq!(header.graph_section_count, 10);
    assert_eq!(header.semantic_section_count, 10);
    assert_eq!(header.contents_chunks, 50);
    assert_eq!(header.total_uncompressed, 5_000_000);
    assert!(header.flags.has(FileHeaderFlags::HAS_PROVENANCE));
    assert!(header.flags.has(FileHeaderFlags::HAS_SEMANTIC));
    assert!(header.flags.has(FileHeaderFlags::HAS_UNHASHED));
}

// ── FileHeader::total_section_count ───────────────────────────

#[test]
fn test_total_section_count_minimal() {
    // Minimal: HEADER + DEPS = 2
    let header = FileHeader::default();
    assert_eq!(header.total_section_count(), 2);
}

#[test]
fn test_total_section_count_with_all() {
    let header = FileHeader::builder()
        .graph_section_count(3)
        .semantic_section_count(3)
        .contents_chunks(10)
        .with_provenance()
        .with_unhashed()
        .build();

    // 2 (HEADER + DEPS) + 1 (PROVENANCE) + 3 (GRAPH) + 3 (SEMANTIC) + 10 (CONTENT) + 1 (UNHASHED) = 20
    assert_eq!(header.total_section_count(), 20);
}

// ── FileHeader::validate ──────────────────────────────────────

#[test]
fn test_validate_default_header() {
    let header = FileHeader::default();
    assert!(header.validate().is_ok());
}

#[test]
fn test_validate_builder_header() {
    let header = FileHeader::builder()
        .hash_table_entries(100)
        .graph_section_count(5)
        .semantic_section_count(5)
        .build();
    assert!(header.validate().is_ok());
}

#[test]
fn test_validate_hash_table_too_large() {
    let header = FileHeader {
        hash_table_entries: (MAX_HASH_TABLE_ENTRIES as u32) + 1,
        ..Default::default()
    };
    assert!(header.validate().is_err());
}

#[test]
fn test_validate_semantic_flag_mismatch_flag_set_count_zero() {
    let mut header = FileHeader {
        semantic_section_count: 0,
        ..Default::default()
    };
    header.flags.set(FileHeaderFlags::HAS_SEMANTIC);
    assert!(header.validate().is_err());
}

#[test]
fn test_validate_semantic_flag_mismatch_count_set_flag_clear() {
    let header = FileHeader {
        semantic_section_count: 5,
        ..Default::default()
    };
    // Don't set HAS_SEMANTIC flag
    assert!(header.validate().is_err());
}

// ── SectionHeader ─────────────────────────────────────────────

#[test]
fn test_section_header_size() {
    assert_eq!(SectionHeader::SIZE, 5);
}

#[test]
fn test_section_header_roundtrip() {
    let header = SectionHeader::new(SectionType::Graph, 12345);
    let bytes = header.to_bytes();
    assert_eq!(bytes.len(), SectionHeader::SIZE);

    let decoded = SectionHeader::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.section_type, SectionType::Graph);
    assert_eq!(decoded.compressed_len, 12345);
}

#[test]
fn test_section_header_all_types() {
    let types = [
        SectionType::Header,
        SectionType::Dependencies,
        SectionType::Provenance,
        SectionType::Graph,
        SectionType::Content,
        SectionType::Semantic,
        SectionType::Unhashed,
    ];

    for st in &types {
        let header = SectionHeader::new(*st, 999);
        let bytes = header.to_bytes();
        let decoded = SectionHeader::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.section_type, *st);
        assert_eq!(decoded.compressed_len, 999);
    }
}

#[test]
fn test_section_header_io_roundtrip() {
    let header = SectionHeader::new(SectionType::Semantic, 4096);

    let mut buf = Vec::new();
    header.write_to(&mut buf).unwrap();
    assert_eq!(buf.len(), SectionHeader::SIZE);

    let mut cursor = std::io::Cursor::new(&buf);
    let decoded = SectionHeader::read_from(&mut cursor).unwrap();
    assert_eq!(decoded.section_type, SectionType::Semantic);
    assert_eq!(decoded.compressed_len, 4096);
}

#[test]
fn test_section_header_invalid_type() {
    let bytes: [u8; 5] = [0xFF, 0, 0, 0, 0];
    let result = SectionHeader::from_bytes(&bytes);
    assert!(result.is_err());
}

#[test]
fn test_section_header_max_compressed_len() {
    let header = SectionHeader::new(SectionType::Content, u32::MAX);
    let bytes = header.to_bytes();
    let decoded = SectionHeader::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.compressed_len, u32::MAX);
}

// ── ContentChunkHeader ────────────────────────────────────────

#[test]
fn test_content_chunk_header_size() {
    assert_eq!(ContentChunkHeader::SIZE, 45);
}

#[test]
fn test_content_chunk_header_roundtrip() {
    let hash = [42u8; 32];
    let header = ContentChunkHeader::new(7, hash, 65536, 32000);

    let bytes = header.to_bytes();
    assert_eq!(bytes.len(), ContentChunkHeader::SIZE);
    assert_eq!(bytes[0], SectionType::Content.to_byte());

    let decoded = ContentChunkHeader::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.chunk_index, 7);
    assert_eq!(decoded.chunk_hash, hash);
    assert_eq!(decoded.uncompressed_len, 65536);
    assert_eq!(decoded.compressed_len, 32000);
}

#[test]
fn test_content_chunk_header_io_roundtrip() {
    let hash = blake3::hash(b"test content").as_bytes().to_owned();
    let mut chunk_hash = [0u8; 32];
    chunk_hash.copy_from_slice(&hash);

    let header = ContentChunkHeader::new(0, chunk_hash, 1024, 512);

    let mut buf = Vec::new();
    header.write_to(&mut buf).unwrap();
    assert_eq!(buf.len(), ContentChunkHeader::SIZE);

    let mut cursor = std::io::Cursor::new(&buf);
    let decoded = ContentChunkHeader::read_from(&mut cursor).unwrap();
    assert_eq!(decoded.chunk_index, 0);
    assert_eq!(decoded.chunk_hash, chunk_hash);
    assert_eq!(decoded.uncompressed_len, 1024);
    assert_eq!(decoded.compressed_len, 512);
}

#[test]
fn test_content_chunk_header_wrong_section_type() {
    let mut bytes = [0u8; ContentChunkHeader::SIZE];
    bytes[0] = SectionType::Graph.to_byte(); // Wrong type!

    let result = ContentChunkHeader::from_bytes(&bytes);
    assert!(result.is_err());
}

#[test]
fn test_content_chunk_header_compression_ratio() {
    let header = ContentChunkHeader::new(0, [0u8; 32], 1000, 500);
    assert!((header.compression_ratio() - 0.5).abs() < f64::EPSILON);

    let header = ContentChunkHeader::new(0, [0u8; 32], 1000, 1000);
    assert!((header.compression_ratio() - 1.0).abs() < f64::EPSILON);

    let header = ContentChunkHeader::new(0, [0u8; 32], 0, 0);
    assert!(header.compression_ratio().is_nan());
}

// ── Trailer ───────────────────────────────────────────────────

#[test]
fn test_trailer_size() {
    assert_eq!(Trailer::SIZE, 32);
}

#[test]
fn test_trailer_roundtrip() {
    let hash = blake3::hash(b"change content").as_bytes().to_owned();
    let mut content_hash = [0u8; 32];
    content_hash.copy_from_slice(&hash);

    let trailer = Trailer { content_hash };
    let bytes = trailer.to_bytes();
    assert_eq!(bytes.len(), Trailer::SIZE);

    let decoded = Trailer::from_bytes(&bytes);
    assert_eq!(decoded.content_hash, content_hash);
}

#[test]
fn test_trailer_io_roundtrip() {
    let trailer = Trailer {
        content_hash: [0xAB; 32],
    };

    let mut buf = Vec::new();
    trailer.write_to(&mut buf).unwrap();
    assert_eq!(buf.len(), Trailer::SIZE);

    let mut cursor = std::io::Cursor::new(&buf);
    let decoded = Trailer::read_from(&mut cursor).unwrap();
    assert_eq!(decoded.content_hash, [0xAB; 32]);
}

#[test]
fn test_trailer_equality() {
    let a = Trailer {
        content_hash: [1; 32],
    };
    let b = Trailer {
        content_hash: [1; 32],
    };
    let c = Trailer {
        content_hash: [2; 32],
    };
    assert_eq!(a, b);
    assert_ne!(a, c);
}

// ── Cross-type integration ────────────────────────────────────

#[test]
fn test_full_header_section_trailer_sizes() {
    // Verify the combined overhead for a minimal change file:
    // FileHeader (64) + SectionHeader*2 (10) + Trailer (32) = 106 bytes overhead minimum
    let overhead = FileHeader::SIZE + (SectionHeader::SIZE * 2) + Trailer::SIZE;
    assert_eq!(overhead, 106);
}

#[test]
fn test_file_header_preserved_in_bytes() {
    // Ensure reserved bytes stay zero through roundtrip
    let header = FileHeader::builder()
        .hash_table_entries(42)
        .graph_section_count(1)
        .build();

    let bytes = header.to_bytes();
    for (i, byte) in bytes.iter().enumerate().skip(36).take(28) {
        assert_eq!(*byte, 0, "reserved byte at index {} should be zero", i);
    }
}

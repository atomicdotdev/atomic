//! Tests for the V3 change file writer.

use super::options::{WriterOptions, WriterStats, DEFAULT_COMPRESSION_LEVEL};
use super::state_machine::ChangeWriter;
use crate::change::format_v3::types::{
    FileHeader, SectionHeader, SectionType, Trailer, HASH_INDEX_NONE,
};
use crate::change::format_v3::HashDedupTable;
use crate::change::header::ChangeHeader;

/// Helper: create a minimal writer with header + hash table already written.
fn writer_with_preamble(buf: &mut Vec<u8>) -> ChangeWriter<'_, Vec<u8>> {
    let self_hash = *blake3::hash(b"test change").as_bytes();
    let hash_table = HashDedupTable::new(self_hash);

    let file_header = FileHeader::builder()
        .hash_table_entries(1)
        .graph_section_count(0)
        .build();

    let mut writer = ChangeWriter::new(buf, WriterOptions::default());
    writer.write_file_header(&file_header).unwrap();
    writer.write_hash_table(&hash_table).unwrap();
    writer
}

/// Helper: create a minimal writer with preamble + metadata sections written.
fn writer_with_metadata(buf: &mut Vec<u8>) -> ChangeWriter<'_, Vec<u8>> {
    let mut writer = writer_with_preamble(buf);
    let header = ChangeHeader::new("Test");
    writer.write_change_header(&header).unwrap();
    writer.write_dependencies(&[]).unwrap();
    writer
}

// ── WriterOptions ──────────────────────────────────────────────

#[test]
fn test_writer_options_default() {
    let opts = WriterOptions::default();
    assert_eq!(opts.compression_level(), DEFAULT_COMPRESSION_LEVEL);
    assert_eq!(opts.compression_level(), 3);
}

#[test]
fn test_writer_options_fast() {
    let opts = WriterOptions::fast();
    assert_eq!(opts.compression_level(), 1);
}

#[test]
fn test_writer_options_max_compression() {
    let opts = WriterOptions::max_compression();
    assert_eq!(opts.compression_level(), 19);
}

#[test]
fn test_writer_options_custom_level() {
    let opts = WriterOptions::with_compression_level(10);
    assert_eq!(opts.compression_level(), 10);
}

#[test]
fn test_writer_options_clamped_low() {
    let opts = WriterOptions::with_compression_level(-5);
    assert_eq!(opts.compression_level(), 1);
}

#[test]
fn test_writer_options_clamped_high() {
    let opts = WriterOptions::with_compression_level(100);
    assert_eq!(opts.compression_level(), 22);
}

// ── WriterStats ────────────────────────────────────────────────

#[test]
fn test_writer_stats_default() {
    let stats = WriterStats::default();
    assert_eq!(stats.sections_written, 0);
    assert_eq!(stats.graph_sections_written, 0);
    assert_eq!(stats.semantic_sections_written, 0);
    assert_eq!(stats.content_chunks_written, 0);
    assert_eq!(stats.total_uncompressed, 0);
    assert_eq!(stats.total_compressed, 0);
    assert_eq!(stats.total_bytes_written, 0);
}

#[test]
fn test_writer_stats_compression_ratio() {
    let stats = WriterStats {
        total_uncompressed: 1000,
        total_compressed: 500,
        ..WriterStats::default()
    };
    assert!((stats.compression_ratio() - 0.5).abs() < f64::EPSILON);
}

#[test]
fn test_writer_stats_compression_ratio_nan() {
    let stats = WriterStats::default();
    assert!(stats.compression_ratio().is_nan());
}

#[test]
fn test_writer_stats_space_savings() {
    let stats = WriterStats {
        total_uncompressed: 1000,
        total_compressed: 300,
        ..WriterStats::default()
    };
    assert!((stats.space_savings_pct() - 70.0).abs() < f64::EPSILON);
}

#[test]
fn test_writer_stats_space_savings_zero() {
    let stats = WriterStats::default();
    assert!((stats.space_savings_pct() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_writer_stats_display() {
    let stats = WriterStats {
        sections_written: 5,
        total_uncompressed: 10000,
        total_compressed: 3000,
        total_bytes_written: 3500,
        ..WriterStats::default()
    };
    let display = format!("{}", stats);
    assert!(display.contains("5 sections"));
    assert!(display.contains("10000 bytes uncompressed"));
    assert!(display.contains("3000 bytes compressed"));
    assert!(display.contains("3500 bytes total"));
}

// ── State Machine: Initial State ───────────────────────────────

#[test]
fn test_initial_state() {
    let mut buf = Vec::new();
    let writer = ChangeWriter::new(&mut buf, WriterOptions::default());
    assert_eq!(writer.state_name(), "CREATED");
    assert_eq!(buf.len(), 0);
}

// ── State Machine: File Header ─────────────────────────────────

#[test]
fn test_write_file_header() {
    let mut buf = Vec::new();
    {
        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());

        let header = FileHeader::default();
        writer.write_file_header(&header).unwrap();

        assert_eq!(writer.state_name(), "FILE_HEADER_WRITTEN");
        assert_eq!(writer.stats().total_bytes_written, 64);
    }
    assert_eq!(buf.len(), FileHeader::SIZE);
    assert_eq!(&buf[0..4], b"ATOM");
}

#[test]
fn test_write_file_header_twice_fails() {
    let mut buf = Vec::new();
    let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());

    writer.write_file_header(&FileHeader::default()).unwrap();
    let result = writer.write_file_header(&FileHeader::default());
    assert!(result.is_err());
}

// ── State Machine: Hash Table ──────────────────────────────────

#[test]
fn test_write_hash_table() {
    let mut buf = Vec::new();
    {
        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer
            .write_file_header(&FileHeader::builder().hash_table_entries(1).build())
            .unwrap();

        let table = HashDedupTable::new([0xAA; 32]);
        writer.write_hash_table(&table).unwrap();

        assert_eq!(writer.state_name(), "HASH_TABLE_WRITTEN");
    }
    assert_eq!(buf.len(), FileHeader::SIZE + 32); // header + 1 hash
}

#[test]
fn test_write_hash_table_before_header_fails() {
    let mut buf = Vec::new();
    let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());

    let table = HashDedupTable::new([0; 32]);
    let result = writer.write_hash_table(&table);
    assert!(result.is_err());
}

// ── State Machine: Metadata Sections ───────────────────────────

#[test]
fn test_write_change_header() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);

    let header = ChangeHeader::new("Test change");
    writer.write_change_header(&header).unwrap();

    assert_eq!(writer.state_name(), "WRITING_METADATA");
    assert_eq!(writer.stats().sections_written, 1);
}

#[test]
fn test_write_change_header_before_hash_table_fails() {
    let mut buf = Vec::new();
    let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
    writer.write_file_header(&FileHeader::default()).unwrap();

    let result = writer.write_change_header(&ChangeHeader::new("Test"));
    assert!(result.is_err());
}

#[test]
fn test_write_change_header_twice_fails() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);

    writer
        .write_change_header(&ChangeHeader::new("First"))
        .unwrap();
    let result = writer.write_change_header(&ChangeHeader::new("Second"));
    assert!(result.is_err());
}

#[test]
fn test_write_dependencies() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);

    writer
        .write_change_header(&ChangeHeader::new("Test"))
        .unwrap();
    writer.write_dependencies(&[1, 2, 3]).unwrap();

    assert_eq!(writer.stats().sections_written, 2);
}

#[test]
fn test_write_dependencies_empty() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);

    writer
        .write_change_header(&ChangeHeader::new("Test"))
        .unwrap();
    writer.write_dependencies(&[]).unwrap();

    assert_eq!(writer.stats().sections_written, 2);
}

#[test]
fn test_write_dependencies_filters_none_sentinel() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);

    writer
        .write_change_header(&ChangeHeader::new("Test"))
        .unwrap();
    // Include HASH_INDEX_NONE — should be filtered out
    writer.write_dependencies(&[1, HASH_INDEX_NONE, 2]).unwrap();

    assert_eq!(writer.stats().sections_written, 2);
}

#[test]
fn test_write_dependencies_before_header_section_fails() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);

    let result = writer.write_dependencies(&[]);
    assert!(result.is_err());
}

#[test]
fn test_write_dependencies_twice_fails() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);

    writer
        .write_change_header(&ChangeHeader::new("Test"))
        .unwrap();
    writer.write_dependencies(&[]).unwrap();
    let result = writer.write_dependencies(&[]);
    assert!(result.is_err());
}

#[test]
fn test_write_provenance() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);

    writer
        .write_change_header(&ChangeHeader::new("Test"))
        .unwrap();
    writer.write_dependencies(&[]).unwrap();
    writer.write_provenance(&[]).unwrap();

    assert_eq!(writer.stats().sections_written, 3);
}

#[test]
fn test_write_provenance_before_deps_fails() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);

    writer
        .write_change_header(&ChangeHeader::new("Test"))
        .unwrap();
    let result = writer.write_provenance(&[]);
    assert!(result.is_err());
}

// ── State Machine: Graph Sections ──────────────────────────────

#[test]
fn test_write_graph_section() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    let payload = b"graph data for file_a.rs";
    writer.write_graph_section(payload).unwrap();

    assert_eq!(writer.stats().graph_sections_written, 1);
    assert_eq!(writer.state_name(), "WRITING_GRAPH");
}

#[test]
fn test_write_multiple_graph_sections() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    writer.write_graph_section(b"file_a").unwrap();
    writer.write_graph_section(b"file_b").unwrap();
    writer.write_graph_section(b"file_c").unwrap();

    assert_eq!(writer.stats().graph_sections_written, 3);
}

#[test]
fn test_write_graph_before_deps_fails() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);
    writer
        .write_change_header(&ChangeHeader::new("Test"))
        .unwrap();

    let result = writer.write_graph_section(b"data");
    assert!(result.is_err());
}

// ── State Machine: Semantic Sections ───────────────────────────

#[test]
fn test_write_semantic_section() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    writer.write_semantic_section(b"semantic data").unwrap();

    assert_eq!(writer.stats().semantic_sections_written, 1);
    assert_eq!(writer.state_name(), "WRITING_SEMANTIC");
}

#[test]
fn test_write_semantic_after_graph() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    writer.write_graph_section(b"graph").unwrap();
    writer.write_semantic_section(b"semantic").unwrap();

    assert_eq!(writer.stats().graph_sections_written, 1);
    assert_eq!(writer.stats().semantic_sections_written, 1);
}

#[test]
fn test_write_graph_after_semantic_fails() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    writer.write_semantic_section(b"semantic").unwrap();
    let result = writer.write_graph_section(b"graph");
    assert!(result.is_err()); // Can't go back to GRAPH after SEMANTIC
}

// ── State Machine: Content Chunks ──────────────────────────────

#[test]
fn test_write_content_chunk() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    let data = b"Hello, World!";
    writer.write_content_chunk(0, data).unwrap();

    assert_eq!(writer.stats().content_chunks_written, 1);
    assert_eq!(writer.state_name(), "WRITING_CONTENT");
}

#[test]
fn test_write_multiple_content_chunks() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    for i in 0..5 {
        let data = format!("chunk {}", i);
        writer.write_content_chunk(i, data.as_bytes()).unwrap();
    }

    assert_eq!(writer.stats().content_chunks_written, 5);
}

#[test]
fn test_write_content_after_graph_and_semantic() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    writer.write_graph_section(b"graph").unwrap();
    writer.write_semantic_section(b"semantic").unwrap();
    writer.write_content_chunk(0, b"content").unwrap();

    assert_eq!(writer.stats().graph_sections_written, 1);
    assert_eq!(writer.stats().semantic_sections_written, 1);
    assert_eq!(writer.stats().content_chunks_written, 1);
}

#[test]
fn test_write_semantic_after_content_fails() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    writer.write_content_chunk(0, b"content").unwrap();
    let result = writer.write_semantic_section(b"semantic");
    assert!(result.is_err());
}

// ── State Machine: Unhashed ────────────────────────────────────

#[test]
fn test_write_unhashed() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    let json = serde_json::to_vec(&serde_json::json!({
        "transcript": "AI reasoning trace"
    }))
    .unwrap();
    writer.write_unhashed(&json).unwrap();

    assert_eq!(writer.state_name(), "WROTE_UNHASHED");
}

#[test]
fn test_write_unhashed_twice_fails() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    writer.write_unhashed(b"first").unwrap();
    let result = writer.write_unhashed(b"second");
    assert!(result.is_err());
}

// ── Finalize ───────────────────────────────────────────────────

#[test]
fn test_finalize_minimal() {
    let mut buf = Vec::new();
    let writer = writer_with_metadata(&mut buf);

    let outcome = writer.finalize().unwrap();

    // Hash should be non-zero
    assert_ne!(outcome.content_hash, [0u8; 32]);
    // Stats should show 2 sections (HEADER + DEPS)
    assert_eq!(outcome.stats.sections_written, 2);
    // Total bytes should be positive
    assert!(outcome.stats.total_bytes_written > 0);
    // Buf should end with 32-byte trailer
    assert!(buf.len() >= 32);
    let trailer_start = buf.len() - 32;
    assert_eq!(&buf[trailer_start..], &outcome.content_hash);
}

#[test]
fn test_finalize_after_all_section_types() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    writer.write_provenance(&[]).unwrap();
    writer.write_graph_section(b"graph data").unwrap();
    writer.write_semantic_section(b"semantic data").unwrap();
    writer.write_content_chunk(0, b"content data").unwrap();
    writer.write_unhashed(b"unhashed data").unwrap();

    let outcome = writer.finalize().unwrap();

    assert_ne!(outcome.content_hash, [0u8; 32]);
    // 2 metadata + 1 provenance + 1 graph + 1 semantic + 1 content + 1 unhashed = 7
    assert_eq!(outcome.stats.sections_written, 7);
    assert_eq!(outcome.stats.graph_sections_written, 1);
    assert_eq!(outcome.stats.semantic_sections_written, 1);
    assert_eq!(outcome.stats.content_chunks_written, 1);
}

#[test]
fn test_finalize_without_deps_fails() {
    let mut buf = Vec::new();
    let mut writer = writer_with_preamble(&mut buf);
    writer
        .write_change_header(&ChangeHeader::new("Test"))
        .unwrap();

    let result = writer.finalize();
    assert!(result.is_err());
}

#[test]
fn test_finalize_without_header_section_fails() {
    let mut buf = Vec::new();
    let writer = writer_with_preamble(&mut buf);

    let result = writer.finalize();
    assert!(result.is_err());
}

// ── Hash Determinism ───────────────────────────────────────────

/// Helper: create a ChangeHeader with a fixed timestamp for deterministic tests.
fn fixed_header(message: &str) -> ChangeHeader {
    ChangeHeader {
        message: message.to_string(),
        description: None,
        timestamp: chrono::DateTime::parse_from_rfc3339("2025-01-15T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        authors: Vec::new(),
    }
}

#[test]
fn test_hash_is_deterministic() {
    let write_once = || {
        let mut buf = Vec::new();
        let self_hash = *blake3::hash(b"test").as_bytes();
        let hash_table = HashDedupTable::new(self_hash);
        let file_header = FileHeader::builder()
            .hash_table_entries(1)
            .graph_section_count(1)
            .semantic_section_count(1)
            .contents_chunks(1)
            .build();

        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table).unwrap();
        writer
            .write_change_header(&fixed_header("Deterministic"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        writer.write_graph_section(b"graph data abc").unwrap();
        writer.write_semantic_section(b"semantic data xyz").unwrap();
        writer.write_content_chunk(0, b"file content 123").unwrap();

        let outcome = writer.finalize().unwrap();
        (buf, outcome.content_hash)
    };

    let (buf1, hash1) = write_once();
    let (buf2, hash2) = write_once();

    assert_eq!(hash1, hash2, "hash should be deterministic");
    assert_eq!(buf1, buf2, "output should be byte-for-byte identical");
}

#[test]
fn test_hash_excludes_unhashed_section() {
    // Write two files: one with UNHASHED, one without.
    // Their content hashes should be identical.
    // Use a fixed timestamp so both writes produce the same header bytes.
    let write_with_unhashed = |include_unhashed: bool| -> [u8; 32] {
        let mut buf = Vec::new();
        let self_hash = *blake3::hash(b"test").as_bytes();
        let hash_table = HashDedupTable::new(self_hash);
        let file_header = FileHeader::builder().hash_table_entries(1).build();

        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table).unwrap();
        writer.write_change_header(&fixed_header("Test")).unwrap();
        writer.write_dependencies(&[]).unwrap();

        if include_unhashed {
            writer
                .write_unhashed(b"this should not affect the hash")
                .unwrap();
        }

        writer.finalize().unwrap().content_hash
    };

    let hash_without = write_with_unhashed(false);
    let hash_with = write_with_unhashed(true);

    assert_eq!(
        hash_without, hash_with,
        "UNHASHED section must not affect content hash"
    );
}

#[test]
fn test_different_content_produces_different_hash() {
    let write_with_content = |content: &[u8]| -> [u8; 32] {
        let mut buf = Vec::new();
        let self_hash = *blake3::hash(b"test").as_bytes();
        let hash_table = HashDedupTable::new(self_hash);
        let file_header = FileHeader::builder()
            .hash_table_entries(1)
            .graph_section_count(1)
            .build();

        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table).unwrap();
        writer
            .write_change_header(&ChangeHeader::new("Test"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        writer.write_graph_section(content).unwrap();

        writer.finalize().unwrap().content_hash
    };

    let hash1 = write_with_content(b"content A");
    let hash2 = write_with_content(b"content B");

    assert_ne!(
        hash1, hash2,
        "different content should produce different hashes"
    );
}

// ── Pre-Compressed Section ─────────────────────────────────────

#[test]
fn test_write_compressed_section() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    let payload = b"some graph data";
    let compressed = zstd::encode_all(&payload[..], 3).unwrap();

    writer
        .write_compressed_section(SectionType::Graph, &compressed, payload.len() as u64)
        .unwrap();

    assert_eq!(writer.stats().sections_written, 3); // HEADER + DEPS + GRAPH
    assert_eq!(writer.stats().graph_sections_written, 1);
}

// ── Content Chunk Hash Correctness ─────────────────────────────

#[test]
fn test_content_chunk_hash_is_of_uncompressed_data() {
    // Write a change with a content chunk, then read it back with the reader
    // to verify the chunk hash matches blake3 of the uncompressed data.
    let data = b"The quick brown fox jumps over the lazy dog";
    let expected_hash = blake3::hash(data);

    let mut buf = Vec::new();
    {
        let self_hash = *blake3::hash(b"chunk hash test").as_bytes();
        let hash_table = HashDedupTable::new(self_hash);

        let file_header = FileHeader::builder()
            .hash_table_entries(1)
            .graph_section_count(0)
            .contents_chunks(1)
            .build();

        let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table).unwrap();
        writer
            .write_change_header(&ChangeHeader::new("Chunk test"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        writer.write_content_chunk(0, data).unwrap();
        writer.finalize().unwrap();
    }

    // Read it back with the reader and check the chunk info
    let mut cursor = std::io::Cursor::new(&buf);
    let mut reader = crate::change::format_v3::ChangeReader::open(&mut cursor).unwrap();

    let sections = reader.read_all_sections().unwrap();
    let content_section = sections
        .iter()
        .find(|s| s.section_type == SectionType::Content)
        .expect("should find CONTENT section");

    let info = content_section
        .content_chunk_info
        .as_ref()
        .expect("content section should have chunk info");

    assert_eq!(info.chunk_hash, *expected_hash.as_bytes());
    assert_eq!(&content_section.payload, data);
}

// ── Full Roundtrip Size Check ──────────────────────────────────

#[test]
fn test_minimal_file_size() {
    let mut buf = Vec::new();
    let outcome = {
        let writer = writer_with_metadata(&mut buf);
        writer.finalize().unwrap()
    };

    // Minimum: FileHeader(64) + HashTable(32) + HEADER section(5+N) +
    //          DEPS section(5+N) + Trailer(32)
    let min_overhead = FileHeader::SIZE + 32 + SectionHeader::SIZE * 2 + Trailer::SIZE;
    assert!(
        buf.len() >= min_overhead,
        "file size {} should be >= minimum overhead {}",
        buf.len(),
        min_overhead,
    );

    // Verify the file starts with ATOM magic
    assert_eq!(&buf[0..4], b"ATOM");

    // Verify the file ends with the content hash
    let trailer_bytes = &buf[buf.len() - 32..];
    assert_eq!(trailer_bytes, &outcome.content_hash);
}

// ── Compression Actually Works ─────────────────────────────────

#[test]
fn test_compression_reduces_size() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    // Write a highly compressible payload (lots of zeros)
    let big_payload = vec![0u8; 10000];
    writer.write_graph_section(&big_payload).unwrap();

    let stats = writer.stats();
    assert!(
        stats.total_compressed < stats.total_uncompressed,
        "compressed ({}) should be smaller than uncompressed ({})",
        stats.total_compressed,
        stats.total_uncompressed,
    );
}

// ── Stats Tracking ─────────────────────────────────────────────

#[test]
fn test_stats_accumulate_correctly() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    let pre_graph_sections = writer.stats().sections_written;
    writer.write_graph_section(b"g1").unwrap();
    writer.write_graph_section(b"g2").unwrap();
    writer.write_semantic_section(b"s1").unwrap();
    writer.write_content_chunk(0, b"c1").unwrap();
    writer.write_content_chunk(1, b"c2").unwrap();
    writer.write_content_chunk(2, b"c3").unwrap();

    let stats = writer.stats();
    assert_eq!(stats.graph_sections_written, 2);
    assert_eq!(stats.semantic_sections_written, 1);
    assert_eq!(stats.content_chunks_written, 3);
    // 2 (metadata) + 2 (graph) + 1 (semantic) + 3 (content) = 8
    assert_eq!(stats.sections_written, pre_graph_sections + 6);
}

// ── Edge Cases ─────────────────────────────────────────────────

#[test]
fn test_write_empty_graph_section() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    writer.write_graph_section(b"").unwrap();
    assert_eq!(writer.stats().graph_sections_written, 1);
}

#[test]
fn test_write_large_content_chunk() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    // 256 KB chunk (max in the FastCDC scheme)
    let data = vec![42u8; 256 * 1024];
    writer.write_content_chunk(0, &data).unwrap();

    assert_eq!(writer.stats().content_chunks_written, 1);
    assert!(writer.stats().total_uncompressed >= 256 * 1024);
}

#[test]
fn test_finalize_after_unhashed() {
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    writer.write_unhashed(b"notes").unwrap();
    let outcome = writer.finalize().unwrap();

    assert_ne!(outcome.content_hash, [0u8; 32]);
    // 2 metadata + 1 unhashed = 3
    assert_eq!(outcome.stats.sections_written, 3);
}

// ── Ordering Enforcement Integration ───────────────────────────

#[test]
fn test_full_forward_progression() {
    // Write every section type in the correct order
    let mut buf = Vec::new();
    let self_hash = *blake3::hash(b"full test").as_bytes();
    let dep_hash = *blake3::hash(b"dependency").as_bytes();
    let mut hash_table = HashDedupTable::new(self_hash);
    let dep_idx = hash_table.insert(dep_hash).unwrap();

    let file_header = FileHeader::builder()
        .hash_table_entries(hash_table.len() as u32)
        .graph_section_count(2)
        .semantic_section_count(2)
        .contents_chunks(3)
        .with_provenance()
        .with_unhashed()
        .build();

    let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());

    // 1. File header
    writer.write_file_header(&file_header).unwrap();
    assert_eq!(writer.state_name(), "FILE_HEADER_WRITTEN");

    // 2. Hash table
    writer.write_hash_table(&hash_table).unwrap();
    assert_eq!(writer.state_name(), "HASH_TABLE_WRITTEN");

    // 3. Metadata
    writer
        .write_change_header(&ChangeHeader::new("Full test"))
        .unwrap();
    writer.write_dependencies(&[dep_idx]).unwrap();
    writer.write_provenance(&[]).unwrap();
    assert_eq!(writer.state_name(), "WRITING_METADATA");

    // 4. Graph sections
    writer.write_graph_section(b"graph file_a").unwrap();
    writer.write_graph_section(b"graph file_b").unwrap();
    assert_eq!(writer.state_name(), "WRITING_GRAPH");

    // 5. Semantic sections
    writer.write_semantic_section(b"semantic file_a").unwrap();
    writer.write_semantic_section(b"semantic file_b").unwrap();
    assert_eq!(writer.state_name(), "WRITING_SEMANTIC");

    // 6. Content chunks
    writer.write_content_chunk(0, b"chunk 0 data").unwrap();
    writer.write_content_chunk(1, b"chunk 1 data").unwrap();
    writer.write_content_chunk(2, b"chunk 2 data").unwrap();
    assert_eq!(writer.state_name(), "WRITING_CONTENT");

    // 7. Unhashed
    writer.write_unhashed(b"transcript").unwrap();
    assert_eq!(writer.state_name(), "WROTE_UNHASHED");

    // 8. Finalize
    let outcome = writer.finalize().unwrap();

    assert_ne!(outcome.content_hash, [0u8; 32]);
    assert_eq!(outcome.stats.graph_sections_written, 2);
    assert_eq!(outcome.stats.semantic_sections_written, 2);
    assert_eq!(outcome.stats.content_chunks_written, 3);
    // HEADER + DEPS + PROV + 2 GRAPH + 2 SEMANTIC + 3 CONTENT + UNHASHED = 11
    assert_eq!(outcome.stats.sections_written, 11);
}

#[test]
fn test_skip_graph_and_semantic() {
    // It's valid to have zero GRAPH and zero SEMANTIC sections
    let mut buf = Vec::new();
    let mut writer = writer_with_metadata(&mut buf);

    // Go directly to content
    writer.write_content_chunk(0, b"content only").unwrap();

    let outcome = writer.finalize().unwrap();
    assert_eq!(outcome.stats.graph_sections_written, 0);
    assert_eq!(outcome.stats.semantic_sections_written, 0);
    assert_eq!(outcome.stats.content_chunks_written, 1);
}

#[test]
fn test_skip_all_optional_sections() {
    // Minimal valid file: just HEADER + DEPS + finalize
    let mut buf = Vec::new();
    let writer = writer_with_metadata(&mut buf);

    let outcome = writer.finalize().unwrap();
    assert_eq!(outcome.stats.sections_written, 2);
    assert_eq!(outcome.stats.graph_sections_written, 0);
    assert_eq!(outcome.stats.semantic_sections_written, 0);
    assert_eq!(outcome.stats.content_chunks_written, 0);
}

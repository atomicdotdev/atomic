//! Tests for the V3 change file reader.

use super::*;
use crate::change::format_v3::hash_table::HashDedupTable;
use crate::change::format_v3::{ChangeWriter, FileHeader, FormatError, SectionType, WriterOptions};
use crate::change::header::ChangeHeader;
use std::io::Cursor;

fn write_minimal_change() -> (Vec<u8>, [u8; 32]) {
    let mut buf = Vec::new();
    let self_hash = *blake3::hash(b"minimal").as_bytes();
    let hash_table = HashDedupTable::new(self_hash);

    let file_header = FileHeader::builder()
        .hash_table_entries(1)
        .graph_section_count(0)
        .build();

    let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
    writer.write_file_header(&file_header).unwrap();
    writer.write_hash_table(&hash_table).unwrap();
    writer
        .write_change_header(&ChangeHeader::new("Minimal change"))
        .unwrap();
    writer.write_dependencies(&[]).unwrap();
    let outcome = writer.finalize().unwrap();

    (buf, outcome.content_hash)
}

// ── Layer-Selective Reading ─────────────────────────────────────

#[test]
fn test_graph_sections_convenience() {
    let (buf, expected_hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let graph = reader.graph_sections().unwrap();

    assert_eq!(graph.len(), 2);
    for s in &graph {
        assert_eq!(s.section_type, SectionType::Graph);
    }

    // Hash should still verify after selective read
    let hash = reader.verify().unwrap();
    assert_eq!(hash, expected_hash);
}

#[test]
fn test_semantic_sections_convenience() {
    let (buf, expected_hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let semantic = reader.semantic_sections().unwrap();

    assert_eq!(semantic.len(), 2);
    for s in &semantic {
        assert_eq!(s.section_type, SectionType::Semantic);
    }

    let hash = reader.verify().unwrap();
    assert_eq!(hash, expected_hash);
}

#[test]
fn test_content_chunks_convenience() {
    let (buf, expected_hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let content = reader.content_chunks().unwrap();

    assert_eq!(content.len(), 3);
    for s in &content {
        assert_eq!(s.section_type, SectionType::Content);
        assert!(s.content_chunk_info.is_some());
    }

    let hash = reader.verify().unwrap();
    assert_eq!(hash, expected_hash);
}

#[test]
fn test_graph_sections_when_none_exist() {
    // Minimal change has no GRAPH sections
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let graph = reader.graph_sections().unwrap();
    assert!(graph.is_empty());
}

#[test]
fn test_semantic_sections_when_none_exist() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let semantic = reader.semantic_sections().unwrap();
    assert!(semantic.is_empty());
}

#[test]
fn test_content_chunks_when_none_exist() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let content = reader.content_chunks().unwrap();
    assert!(content.is_empty());
}

#[test]
fn test_graph_sections_stats_tracking() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    reader.graph_sections().unwrap();

    let stats = reader.stats();
    assert_eq!(stats.graph_sections_read, 2);
    // All non-GRAPH sections were skipped
    assert!(stats.sections_skipped > 0);
}

/// Helper: write a full change file with all section types.
fn write_full_change() -> (Vec<u8>, [u8; 32]) {
    let mut buf = Vec::new();
    let self_hash = *blake3::hash(b"full").as_bytes();
    let dep_hash = *blake3::hash(b"dep1").as_bytes();

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
    writer.write_file_header(&file_header).unwrap();
    writer.write_hash_table(&hash_table).unwrap();
    writer
        .write_change_header(&ChangeHeader::new("Full change"))
        .unwrap();
    writer.write_dependencies(&[dep_idx]).unwrap();
    writer.write_provenance(&[]).unwrap();
    writer
        .write_graph_section(b"graph data for file_a.rs")
        .unwrap();
    writer
        .write_graph_section(b"graph data for file_b.rs")
        .unwrap();
    writer
        .write_semantic_section(b"semantic data for file_a.rs")
        .unwrap();
    writer
        .write_semantic_section(b"semantic data for file_b.rs")
        .unwrap();
    writer
        .write_content_chunk(0, b"chunk zero content bytes")
        .unwrap();
    writer
        .write_content_chunk(1, b"chunk one content bytes")
        .unwrap();
    writer
        .write_content_chunk(2, b"chunk two content bytes")
        .unwrap();
    writer
        .write_unhashed(b"{\"transcript\": \"AI reasoning\"}")
        .unwrap();
    let outcome = writer.finalize().unwrap();

    (buf, outcome.content_hash)
}

// ── Opening ────────────────────────────────────────────────────

#[test]
fn test_open_minimal() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);

    let reader = ChangeReader::open(&mut cursor).unwrap();

    assert_eq!(reader.file_header().hash_table_entries, 1);
    assert_eq!(reader.hash_table().len(), 1);
    assert_eq!(reader.remaining_sections(), 2); // HEADER + DEPS
}

#[test]
fn test_open_full() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);

    let reader = ChangeReader::open(&mut cursor).unwrap();

    assert_eq!(reader.file_header().hash_table_entries, 2);
    assert_eq!(reader.hash_table().len(), 2);
    // HEADER + DEPS + PROV + 2 GRAPH + 2 SEMANTIC + 3 CONTENT + UNHASHED = 11
    assert_eq!(reader.remaining_sections(), 11);
}

#[test]
fn test_open_invalid_magic() {
    let mut buf = vec![0u8; 128];
    buf[0..4].copy_from_slice(b"NOPE");

    let mut cursor = Cursor::new(&buf);
    let result = ChangeReader::open(&mut cursor);

    assert!(result.is_err());
    assert!(matches!(result, Err(FormatError::InvalidMagic { .. })));
}

#[test]
fn test_open_truncated() {
    let buf = vec![0u8; 10]; // way too short
    let mut cursor = Cursor::new(&buf);

    let result = ChangeReader::open(&mut cursor);
    assert!(result.is_err());
}

// ── Reading Sections ───────────────────────────────────────────

#[test]
fn test_read_minimal_sections() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    // First section: HEADER
    let section = reader.next_section().unwrap().unwrap();
    assert_eq!(section.section_type, SectionType::Header);
    assert!(!section.payload.is_empty());
    assert!(section.content_chunk_info.is_none());
    assert!(section.is_hashed());

    // Second section: DEPS
    let section = reader.next_section().unwrap().unwrap();
    assert_eq!(section.section_type, SectionType::Dependencies);
    assert!(section.is_hashed());

    // No more sections
    let section = reader.next_section().unwrap();
    assert!(section.is_none());
}

#[test]
fn test_read_full_sections() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let sections = reader.read_all_sections().unwrap();

    // HEADER + DEPS + PROV + 2 GRAPH + 2 SEMANTIC + 3 CONTENT + UNHASHED = 11
    assert_eq!(sections.len(), 11);

    assert_eq!(sections[0].section_type, SectionType::Header);
    assert_eq!(sections[1].section_type, SectionType::Dependencies);
    assert_eq!(sections[2].section_type, SectionType::Provenance);
    assert_eq!(sections[3].section_type, SectionType::Graph);
    assert_eq!(sections[4].section_type, SectionType::Graph);
    assert_eq!(sections[5].section_type, SectionType::Semantic);
    assert_eq!(sections[6].section_type, SectionType::Semantic);
    assert_eq!(sections[7].section_type, SectionType::Content);
    assert_eq!(sections[8].section_type, SectionType::Content);
    assert_eq!(sections[9].section_type, SectionType::Content);
    assert_eq!(sections[10].section_type, SectionType::Unhashed);
}

#[test]
fn test_read_content_chunk_info() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let sections = reader.read_all_sections().unwrap();

    // Content chunks should have chunk info
    let content_sections: Vec<_> = sections
        .iter()
        .filter(|s| s.section_type == SectionType::Content)
        .collect();

    assert_eq!(content_sections.len(), 3);

    for (i, section) in content_sections.iter().enumerate() {
        let info = section.content_chunk_info.as_ref().unwrap();
        assert_eq!(info.chunk_index, i as u32);
        // The chunk hash should be blake3 of the decompressed content
        let expected_hash = blake3::hash(&section.payload);
        assert_eq!(info.chunk_hash, *expected_hash.as_bytes());
    }
}

#[test]
fn test_read_unhashed_not_hashed() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let sections = reader.read_all_sections().unwrap();
    let unhashed = sections.last().unwrap();
    assert_eq!(unhashed.section_type, SectionType::Unhashed);
    assert!(!unhashed.is_hashed());
}

// ── Deserialization ────────────────────────────────────────────

#[test]
fn test_deserialize_change_header() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let section = reader.next_section().unwrap().unwrap();
    assert_eq!(section.section_type, SectionType::Header);

    let header: ChangeHeader = section.deserialize().unwrap();
    assert_eq!(header.message, "Minimal change");
}

#[test]
fn test_deserialize_dependencies() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    // Skip HEADER
    reader.next_section().unwrap();

    let section = reader.next_section().unwrap().unwrap();
    assert_eq!(section.section_type, SectionType::Dependencies);

    let deps: Vec<u16> = section.deserialize().unwrap();
    assert!(deps.is_empty()); // no dependencies
}

#[test]
fn test_deserialize_dependencies_with_values() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    // Skip HEADER
    reader.next_section().unwrap();

    let section = reader.next_section().unwrap().unwrap();
    let deps: Vec<u16> = section.deserialize().unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0], 1); // dep_idx was 1
}

// ── Peek ───────────────────────────────────────────────────────

#[test]
fn test_peek_section_type() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    // Peek without consuming
    let peeked = reader.peek_section_type().unwrap();
    assert_eq!(peeked, Some(SectionType::Header));

    // Peek again — should return the same thing
    let peeked2 = reader.peek_section_type().unwrap();
    assert_eq!(peeked2, Some(SectionType::Header));

    // Now read it — should consume the peeked section
    let section = reader.next_section().unwrap().unwrap();
    assert_eq!(section.section_type, SectionType::Header);

    // Peek the next one
    let peeked3 = reader.peek_section_type().unwrap();
    assert_eq!(peeked3, Some(SectionType::Dependencies));
}

#[test]
fn test_peek_after_all_sections_returns_none() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    reader.read_all_sections().unwrap();

    let peeked = reader.peek_section_type().unwrap();
    assert_eq!(peeked, None);
}

// ── Skip ───────────────────────────────────────────────────────

#[test]
fn test_skip_section() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    // Skip HEADER
    assert!(reader.skip_section().unwrap());
    assert_eq!(reader.stats().sections_skipped, 1);

    // Read DEPS normally
    let section = reader.next_section().unwrap().unwrap();
    assert_eq!(section.section_type, SectionType::Dependencies);

    // No more to skip
    assert!(!reader.skip_section().unwrap());
}

#[test]
fn test_skip_all_sections() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let mut skip_count = 0;
    while reader.skip_section().unwrap() {
        skip_count += 1;
    }

    assert_eq!(skip_count, 11); // all sections skipped
    assert_eq!(reader.stats().sections_skipped, 11);
    assert_eq!(reader.stats().sections_read, 0);
}

#[test]
fn test_selective_reading_graph_only() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let mut graph_payloads = Vec::new();

    // Read only GRAPH sections, skip everything else
    loop {
        match reader.peek_section_type().unwrap() {
            Some(SectionType::Graph) => {
                let section = reader.next_section().unwrap().unwrap();
                graph_payloads.push(section.payload);
            }
            Some(_) => {
                reader.skip_section().unwrap();
            }
            None => break,
        }
    }

    assert_eq!(graph_payloads.len(), 2);
    assert_eq!(reader.stats().graph_sections_read, 2);
    assert_eq!(reader.stats().sections_read, 2);
    assert_eq!(reader.stats().sections_skipped, 9); // 11 total - 2 read
}

// ── Verification ───────────────────────────────────────────────

#[test]
fn test_verify_minimal() {
    let (buf, expected_hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    reader.read_all_sections().unwrap();
    let computed_hash = reader.verify().unwrap();

    assert_eq!(computed_hash, expected_hash);
}

#[test]
fn test_verify_full() {
    let (buf, expected_hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    reader.read_all_sections().unwrap();
    let computed_hash = reader.verify().unwrap();

    assert_eq!(computed_hash, expected_hash);
}

#[test]
fn test_verify_after_skipping_all() {
    // Hash should still verify even when all sections are skipped
    let (buf, expected_hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    while reader.skip_section().unwrap() {}
    let computed_hash = reader.verify().unwrap();

    assert_eq!(computed_hash, expected_hash);
}

#[test]
fn test_verify_after_selective_read() {
    // Hash should verify even when some sections are read and some skipped
    let (buf, expected_hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    // Read first two sections, skip the rest
    reader.next_section().unwrap();
    reader.next_section().unwrap();
    while reader.skip_section().unwrap() {}

    let computed_hash = reader.verify().unwrap();
    assert_eq!(computed_hash, expected_hash);
}

#[test]
fn test_verify_corrupt_data() {
    let (mut buf, _hash) = write_minimal_change();

    // Corrupt some bytes in the middle of the file (after the file header)
    if buf.len() > 100 {
        buf[80] ^= 0xFF;
        buf[81] ^= 0xFF;
    }

    let mut cursor = Cursor::new(&buf);
    // Opening might succeed (header and hash table might be fine)
    // but reading + verifying should fail
    let reader_result = ChangeReader::open(&mut cursor);
    if let Ok(mut reader) = reader_result {
        // Try to read all sections — might fail on decompression
        let read_result = reader.read_all_sections();
        if read_result.is_ok() {
            // If sections somehow read, verification should fail
            let verify_result = reader.verify();
            assert!(
                verify_result.is_err(),
                "verification should fail on corrupt data"
            );
        }
        // If reading failed, that's also acceptable — corrupt data was detected
    }
    // If opening failed, that's also acceptable
}

// ── read_all_and_verify ────────────────────────────────────────

#[test]
fn test_read_all_and_verify_minimal() {
    let (buf, expected_hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let (sections, hash) = reader.read_all_and_verify().unwrap();
    assert_eq!(sections.len(), 2);
    assert_eq!(hash, expected_hash);
}

#[test]
fn test_read_all_and_verify_full() {
    let (buf, expected_hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let (sections, hash) = reader.read_all_and_verify().unwrap();
    assert_eq!(sections.len(), 11);
    assert_eq!(hash, expected_hash);
}

// ── Stats ──────────────────────────────────────────────────────

#[test]
fn test_stats_after_reading() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    reader.read_all_sections().unwrap();

    let stats = reader.stats();
    assert_eq!(stats.sections_read, 11);
    assert_eq!(stats.sections_skipped, 0);
    assert_eq!(stats.graph_sections_read, 2);
    assert_eq!(stats.semantic_sections_read, 2);
    assert_eq!(stats.content_chunks_read, 3);
    assert!(stats.total_compressed > 0);
    assert!(stats.total_decompressed > 0);
    assert!(stats.total_bytes_read > 0);
}

#[test]
fn test_stats_display() {
    let stats = ReaderStats {
        sections_read: 5,
        sections_skipped: 3,
        graph_sections_read: 2,
        semantic_sections_read: 1,
        content_chunks_read: 2,
        total_decompressed: 10000,
        total_compressed: 3000,
        total_bytes_read: 4000,
    };
    let display = format!("{}", stats);
    assert!(display.contains("5 sections read"));
    assert!(display.contains("3 skipped"));
    assert!(display.contains("3000 bytes compressed"));
    assert!(display.contains("10000 bytes decompressed"));
}

// ── Remaining Sections Counter ─────────────────────────────────

#[test]
fn test_remaining_sections_decreases() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    assert_eq!(reader.remaining_sections(), 2);

    reader.next_section().unwrap();
    assert_eq!(reader.remaining_sections(), 1);

    reader.next_section().unwrap();
    assert_eq!(reader.remaining_sections(), 0);
}

#[test]
fn test_remaining_sections_with_skip() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let initial = reader.remaining_sections();
    assert_eq!(initial, 11);

    reader.skip_section().unwrap();
    assert_eq!(reader.remaining_sections(), 10);

    reader.next_section().unwrap();
    assert_eq!(reader.remaining_sections(), 9);
}

// ── ReadSection helpers ────────────────────────────────────────

#[test]
fn test_read_section_payload_len() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let section = reader.next_section().unwrap().unwrap();
    assert!(section.payload_len() > 0);
    assert_eq!(section.payload_len(), section.payload.len());
}

#[test]
fn test_read_section_is_hashed() {
    let (buf, _hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let sections = reader.read_all_sections().unwrap();

    // All sections except UNHASHED should be hashed
    for section in &sections {
        if section.section_type == SectionType::Unhashed {
            assert!(!section.is_hashed());
        } else {
            assert!(
                section.is_hashed(),
                "{:?} should be hashed",
                section.section_type
            );
        }
    }
}

// ── End-to-End Roundtrip ───────────────────────────────────────

#[test]
fn test_roundtrip_change_header_content() {
    let original_message = "This is a test change with special chars: 日本語 🚀 <>&";

    let mut buf = Vec::new();
    let self_hash = *blake3::hash(b"roundtrip").as_bytes();
    let hash_table = HashDedupTable::new(self_hash);
    let file_header = FileHeader::builder().hash_table_entries(1).build();

    let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
    writer.write_file_header(&file_header).unwrap();
    writer.write_hash_table(&hash_table).unwrap();
    writer
        .write_change_header(&ChangeHeader::new(original_message))
        .unwrap();
    writer.write_dependencies(&[]).unwrap();
    let write_outcome = writer.finalize().unwrap();

    // Read back
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let section = reader.next_section().unwrap().unwrap();
    let header: ChangeHeader = section.deserialize().unwrap();
    assert_eq!(header.message, original_message);

    // Skip DEPS
    reader.skip_section().unwrap();

    let verified_hash = reader.verify().unwrap();
    assert_eq!(verified_hash, write_outcome.content_hash);
}

#[test]
fn test_roundtrip_content_chunks() {
    let chunk_data = [
        b"The quick brown fox".to_vec(),
        b"jumps over".to_vec(),
        b"the lazy dog".to_vec(),
    ];

    let mut buf = Vec::new();
    let self_hash = *blake3::hash(b"chunks").as_bytes();
    let hash_table = HashDedupTable::new(self_hash);
    let file_header = FileHeader::builder()
        .hash_table_entries(1)
        .contents_chunks(chunk_data.len() as u32)
        .build();

    let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
    writer.write_file_header(&file_header).unwrap();
    writer.write_hash_table(&hash_table).unwrap();
    writer
        .write_change_header(&ChangeHeader::new("Chunks"))
        .unwrap();
    writer.write_dependencies(&[]).unwrap();
    for (i, data) in chunk_data.iter().enumerate() {
        writer.write_content_chunk(i as u32, data).unwrap();
    }
    let write_outcome = writer.finalize().unwrap();

    // Read back
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    let sections = reader.read_all_sections().unwrap();

    let content_sections: Vec<_> = sections
        .iter()
        .filter(|s| s.section_type == SectionType::Content)
        .collect();

    assert_eq!(content_sections.len(), 3);

    for (i, section) in content_sections.iter().enumerate() {
        assert_eq!(section.payload, chunk_data[i]);
        let info = section.content_chunk_info.as_ref().unwrap();
        assert_eq!(info.chunk_index, i as u32);
        let expected_hash = blake3::hash(&chunk_data[i]);
        assert_eq!(info.chunk_hash, *expected_hash.as_bytes());
    }

    let verified_hash = reader.verify().unwrap();
    assert_eq!(verified_hash, write_outcome.content_hash);
}

#[test]
fn test_roundtrip_hash_table_resolution() {
    let self_hash = *blake3::hash(b"self").as_bytes();
    let dep1_hash = *blake3::hash(b"dep1").as_bytes();
    let dep2_hash = *blake3::hash(b"dep2").as_bytes();

    let mut hash_table_write = HashDedupTable::new(self_hash);
    hash_table_write.insert(dep1_hash).unwrap();
    hash_table_write.insert(dep2_hash).unwrap();

    let mut buf = Vec::new();
    let file_header = FileHeader::builder()
        .hash_table_entries(hash_table_write.len() as u32)
        .build();

    let mut writer = ChangeWriter::new(&mut buf, WriterOptions::default());
    writer.write_file_header(&file_header).unwrap();
    writer.write_hash_table(&hash_table_write).unwrap();
    writer
        .write_change_header(&ChangeHeader::new("Deps"))
        .unwrap();
    writer.write_dependencies(&[1, 2]).unwrap();
    writer.finalize().unwrap();

    // Read back and verify hash table
    let mut cursor = Cursor::new(&buf);
    let reader = ChangeReader::open(&mut cursor).unwrap();

    let ht = reader.hash_table();
    assert_eq!(ht.len(), 3);
    assert_eq!(ht.resolve(0).unwrap(), &self_hash);
    assert_eq!(ht.resolve(1).unwrap(), &dep1_hash);
    assert_eq!(ht.resolve(2).unwrap(), &dep2_hash);
}

// ── Multiple Reads at Different Compression Levels ─────────────

#[test]
fn test_different_compression_levels_same_hash() {
    // The hash should be deterministic for the SAME compression level,
    // but DIFFERENT compression levels produce different compressed bytes
    // and therefore different hashes. This is expected behavior.
    //
    // This test verifies that reading works at multiple compression levels.
    for level in [1, 3, 10] {
        let mut buf = Vec::new();
        let self_hash = *blake3::hash(b"level-test").as_bytes();
        let hash_table = HashDedupTable::new(self_hash);
        let file_header = FileHeader::builder()
            .hash_table_entries(1)
            .graph_section_count(1)
            .build();

        let opts = WriterOptions::with_compression_level(level);
        let mut writer = ChangeWriter::new(&mut buf, opts);
        writer.write_file_header(&file_header).unwrap();
        writer.write_hash_table(&hash_table).unwrap();
        writer
            .write_change_header(&ChangeHeader::new("Level test"))
            .unwrap();
        writer.write_dependencies(&[]).unwrap();
        writer.write_graph_section(b"graph data here").unwrap();
        let write_outcome = writer.finalize().unwrap();

        // Read back and verify
        let mut cursor = Cursor::new(&buf);
        let mut reader = ChangeReader::open(&mut cursor).unwrap();
        let (sections, hash) = reader.read_all_and_verify().unwrap();
        assert_eq!(sections.len(), 3); // HEADER + DEPS + GRAPH
        assert_eq!(hash, write_outcome.content_hash);
    }
}

// ── Edge Cases ─────────────────────────────────────────────────

#[test]
fn test_next_section_after_exhausted_returns_none() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    reader.read_all_sections().unwrap();

    // Repeated calls should all return None
    assert!(reader.next_section().unwrap().is_none());
    assert!(reader.next_section().unwrap().is_none());
    assert!(reader.next_section().unwrap().is_none());
}

#[test]
fn test_skip_after_exhausted_returns_false() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    reader.read_all_sections().unwrap();

    assert!(!reader.skip_section().unwrap());
    assert!(!reader.skip_section().unwrap());
}

#[test]
fn test_peek_after_exhausted_returns_none() {
    let (buf, _hash) = write_minimal_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    reader.read_all_sections().unwrap();

    assert_eq!(reader.peek_section_type().unwrap(), None);
    assert_eq!(reader.peek_section_type().unwrap(), None);
}

#[test]
fn test_interleaved_peek_skip_read() {
    let (buf, expected_hash) = write_full_change();
    let mut cursor = Cursor::new(&buf);
    let mut reader = ChangeReader::open(&mut cursor).unwrap();

    // Peek → read
    assert_eq!(
        reader.peek_section_type().unwrap(),
        Some(SectionType::Header)
    );
    let s = reader.next_section().unwrap().unwrap();
    assert_eq!(s.section_type, SectionType::Header);

    // Peek → skip
    assert_eq!(
        reader.peek_section_type().unwrap(),
        Some(SectionType::Dependencies)
    );
    reader.skip_section().unwrap();

    // Read without peek
    let s = reader.next_section().unwrap().unwrap();
    assert_eq!(s.section_type, SectionType::Provenance);

    // Skip without peek
    reader.skip_section().unwrap(); // GRAPH 1

    // Peek twice → read
    reader.peek_section_type().unwrap();
    reader.peek_section_type().unwrap();
    let s = reader.next_section().unwrap().unwrap();
    assert_eq!(s.section_type, SectionType::Graph); // GRAPH 2

    // Skip the rest
    while reader.skip_section().unwrap() {}

    // Verify
    let hash = reader.verify().unwrap();
    assert_eq!(hash, expected_hash);
}

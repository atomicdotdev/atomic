//! Tests for the redb change store module.

use super::*;
use atomic_core::change::{Change, ChangeHeader, Encoding, GraphOp, Local};
use atomic_core::types::{ChangePosition, EdgeFlags, Hash, Position};
use atomic_core::{Atom, Insertion};
use std::io::Cursor;

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

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

fn provenance_event(label: &str) -> atomic_core::change::session::SessionEvent {
    atomic_core::change::session::SessionEvent {
        seq: u64::MAX,
        timestamp: "2026-09-08T16:00:00Z".to_string(),
        event_kind: "tool".to_string(),
        place: None,
        transition: None,
        token_id: label.to_string(),
        token_kind: "artifact".to_string(),
        token_data: format!(r#"{{"label":"{label}"}}"#),
        record_type: Some("tool".to_string()),
    }
}

#[test]
fn provenance_turn_reservation_is_idempotent_and_persistent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.redb");
    let first_id;
    {
        let store = RedbChangeStore::open(&path).unwrap();
        let first = store.reserve_provenance_turn("session-a", 3, 100).unwrap();
        let duplicate = store.reserve_provenance_turn("session-a", 3, 200).unwrap();
        let next = store.reserve_provenance_turn("session-a", 4, 200).unwrap();
        assert_eq!(first, duplicate);
        assert_ne!(first.provenance_id, next.provenance_id);
        assert_eq!(
            store
                .get_provenance_turn_for("session-a", 3)
                .unwrap()
                .unwrap(),
            first
        );
        first_id = first.provenance_id;
    }

    let reopened = RedbChangeStore::open(&path).unwrap();
    assert_eq!(
        reopened
            .get_provenance_turn(first_id)
            .unwrap()
            .unwrap()
            .session_id,
        "session-a"
    );
    assert_ne!(
        reopened
            .reserve_provenance_turn("session-b", 0, 300)
            .unwrap()
            .provenance_id,
        first_id
    );
}

#[test]
fn provenance_events_are_ordered_idempotent_and_fenced() {
    let (_dir, store) = temp_store();
    let turn = store.reserve_provenance_turn("session-a", 0, 1).unwrap();

    let first = store
        .append_provenance_event(
            turn.provenance_id,
            turn.generation,
            "event-a",
            provenance_event("a"),
            2,
        )
        .unwrap();
    let duplicate = store
        .append_provenance_event(
            turn.provenance_id,
            turn.generation,
            "event-a",
            provenance_event("a"),
            3,
        )
        .unwrap();
    assert_eq!(first, duplicate);

    store
        .append_provenance_event(
            turn.provenance_id,
            turn.generation,
            "event-b",
            provenance_event("b"),
            4,
        )
        .unwrap();
    let events = store.load_provenance_events(turn.provenance_id).unwrap();
    assert_eq!(
        events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        vec![0, 1]
    );

    let stopped = store
        .stop_provenance_turn(
            turn.provenance_id,
            turn.generation,
            StopState {
                cause: StopCause::ProcessExited,
                observed_at: 5,
                last_event_seq: Some(1),
                resumable: true,
            },
        )
        .unwrap();
    assert!(matches!(stopped.state, ProvenanceTurnState::Stopped(_)));
    assert!(matches!(
        store.append_provenance_event(
            turn.provenance_id,
            turn.generation,
            "event-c",
            provenance_event("c"),
            6,
        ),
        Err(RedbStoreError::ProvenanceFenced { .. })
    ));
}

#[test]
fn envelope_batch_preserves_order_duplicates_and_reopen() {
    let (dir, store) = temp_store();
    let turn = store.reserve_provenance_turn("batch", 1, 1).unwrap();
    let first = store
        .append_provenance_envelope(turn.provenance_id, turn.generation, "old", b"original", 2)
        .unwrap();
    let batch: &[(&str, &[u8])] = &[
        ("a", b"first a"),
        ("old", b"retry"),
        ("b", b"b"),
        ("a", b"later a"),
    ];
    let ack = store
        .append_provenance_envelopes(turn.provenance_id, turn.generation, batch, 3)
        .unwrap();
    assert_eq!(ack.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 0, 2, 1]);
    assert_eq!(ack[1], first);
    assert_eq!(ack[0], ack[3]);
    assert_eq!(ack[0].envelope, b"first a");
    drop(store);
    let store = RedbChangeStore::open(dir.path().join("test_change_store.redb")).unwrap();
    assert_eq!(
        store.load_provenance_envelopes(turn.provenance_id).unwrap(),
        vec![first, ack[0].clone(), ack[2].clone()]
    );
    // Stale-generation duplicates remain valid retries, even after a lifecycle fence.
    store
        .stop_provenance_turn(
            turn.provenance_id,
            turn.generation,
            StopState {
                cause: StopCause::ProcessExited,
                observed_at: 4,
                last_event_seq: Some(2),
                resumable: true,
            },
        )
        .unwrap();
    assert_eq!(
        store
            .append_provenance_envelopes(turn.provenance_id, turn.generation, batch, 5)
            .unwrap(),
        ack
    );
    assert!(store
        .append_provenance_envelopes(
            turn.provenance_id,
            turn.generation,
            &[("a", b"retry"), ("new", b"new")],
            6
        )
        .is_err());
    assert_eq!(
        store
            .load_provenance_envelopes(turn.provenance_id)
            .unwrap()
            .len(),
        3
    );
    assert!(store
        .append_provenance_envelopes(turn.provenance_id, turn.generation, &[], 7)
        .unwrap()
        .is_empty());
}

#[test]
fn envelope_batch_rolls_back_earlier_events_on_legacy_conflict() {
    let (_dir, store) = temp_store();
    let turn = store
        .reserve_provenance_turn("batch-conflict", 1, 1)
        .unwrap();
    store
        .append_provenance_event(
            turn.provenance_id,
            turn.generation,
            "legacy",
            provenance_event("legacy"),
            2,
        )
        .unwrap();
    let before = store
        .get_provenance_turn_for("batch-conflict", 1)
        .unwrap()
        .unwrap();
    assert!(matches!(
        store.append_provenance_envelopes(
            turn.provenance_id,
            turn.generation,
            &[("new", b"new"), ("legacy", b"invalid")],
            3
        ),
        Err(RedbStoreError::ProvenanceEventConflict { .. })
    ));
    assert!(store
        .load_provenance_envelopes(turn.provenance_id)
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .get_provenance_turn_for("batch-conflict", 1)
            .unwrap()
            .unwrap(),
        before
    );
    let retried = store
        .append_provenance_envelope(turn.provenance_id, turn.generation, "new", b"new", 4)
        .unwrap();
    assert_eq!(
        retried.seq, 1,
        "failed batch must leave neither index entry nor sequence gap"
    );
}

#[test]
fn envelope_batch_sequence_exhaustion_rolls_back_the_entire_batch() {
    let (_dir, store) = temp_store();
    let mut turn = store
        .reserve_provenance_turn("batch-overflow", 1, 1)
        .unwrap();
    turn.next_event_seq = u64::MAX - 1;
    let tx = store.db.begin_write().unwrap();
    {
        let mut table = tx.open_table(tables::PROVENANCE_TURNS).unwrap();
        table
            .insert(
                turn.provenance_id.get(),
                postcard::to_allocvec(&turn).unwrap().as_slice(),
            )
            .unwrap();
    }
    tx.commit().unwrap();
    assert!(matches!(
        store.append_provenance_envelopes(
            turn.provenance_id,
            turn.generation,
            &[("a", b"a"), ("b", b"b")],
            2
        ),
        Err(RedbStoreError::ProvenanceEventSequenceExhausted { .. })
    ));
    assert!(store
        .load_provenance_envelopes(turn.provenance_id)
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .get_provenance_turn_for("batch-overflow", 1)
            .unwrap()
            .unwrap(),
        turn
    );
    assert_eq!(
        store
            .append_provenance_envelope(turn.provenance_id, turn.generation, "a", b"a", 3)
            .unwrap()
            .seq,
        u64::MAX - 1
    );
}

#[test]
fn envelope_batch_and_checkpoint_have_no_partial_frontier() {
    let (_dir, store) = temp_store();
    for iteration in 0..16 {
        let turn = store
            .reserve_provenance_turn(&format!("batch-race-{iteration}"), 1, 1)
            .unwrap();
        let barrier = std::sync::Barrier::new(2);
        let (append, checkpoint) = std::thread::scope(|scope| {
            let append = scope.spawn(|| {
                barrier.wait();
                let names: Vec<_> = (0..16).map(|i| format!("event-{i}")).collect();
                let batch: Vec<_> = names
                    .iter()
                    .map(|id| (id.as_str(), id.as_bytes()))
                    .collect();
                store.append_provenance_envelopes(turn.provenance_id, turn.generation, &batch, 2)
            });
            barrier.wait();
            let checkpoint = store
                .prepare_provenance_checkpoint(
                    turn.provenance_id,
                    turn.generation,
                    ProvenanceCheckpointSource {
                        agent_name: "codex".into(),
                        agent_display_name: "Codex".into(),
                        agent_vendor: "openai".into(),
                        change_hashes: vec![],
                        previous_provenance: None,
                        plan_id: None,
                        ledger_turn_number: 0,
                    },
                    3,
                )
                .unwrap();
            (append.join().unwrap(), checkpoint)
        });
        let count = match append {
            Ok(events) => {
                assert_eq!(events.len(), 16);
                16
            }
            Err(RedbStoreError::ProvenanceFenced { .. }) => 0,
            other => panic!("unexpected batch/checkpoint outcome: {other:?}"),
        };
        assert_eq!(checkpoint.frozen_event_count, count);
        assert_eq!(
            store
                .load_provenance_envelopes(turn.provenance_id)
                .unwrap()
                .len() as u64,
            count
        );
    }
}

#[test]
fn lossless_envelopes_share_sequence_space_and_preserve_legacy_events() {
    let (_dir, store) = temp_store();
    let turn = store.reserve_provenance_turn("session-a", 1, 1).unwrap();

    store
        .append_provenance_event(
            turn.provenance_id,
            turn.generation,
            "legacy",
            provenance_event("legacy"),
            2,
        )
        .unwrap();
    let first = store
        .append_provenance_envelope(
            turn.provenance_id,
            turn.generation,
            "envelope-a",
            br#"{"schema_version":1,"event_id":"envelope-a"}"#,
            3,
        )
        .unwrap();
    let retry = store
        .append_provenance_envelope(
            turn.provenance_id,
            turn.generation,
            "envelope-a",
            br#"{\"schema_version\":1,\"event_id\":\"envelope-a\",\"retry_observed_at\":4}"#,
            4,
        )
        .unwrap();
    assert_eq!(first, retry);
    assert_eq!(first.seq, 1);
    assert_eq!(
        store
            .load_provenance_events(turn.provenance_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store.load_provenance_envelopes(turn.provenance_id).unwrap(),
        vec![first]
    );

    let stopped = store
        .stop_provenance_turn(
            turn.provenance_id,
            turn.generation,
            StopState {
                cause: StopCause::ProcessExited,
                observed_at: 5,
                last_event_seq: Some(1),
                resumable: true,
            },
        )
        .unwrap();
    assert!(matches!(
        store.append_provenance_envelope(
            stopped.provenance_id,
            turn.generation,
            "stale",
            b"stale",
            6,
        ),
        Err(RedbStoreError::ProvenanceFenced { .. })
    ));
}

#[test]
fn stop_resume_and_abandon_preserve_identity_frontier_and_fencing() {
    let (_dir, store) = temp_store();
    let running = store.reserve_provenance_turn("lifecycle", 1, 1).unwrap();
    for index in 0..2 {
        store
            .append_provenance_envelope(
                running.provenance_id,
                running.generation,
                &format!("event-{index}"),
                format!("event-{index}").as_bytes(),
                2 + index,
            )
            .unwrap();
    }

    let stopped = store
        .stop_provenance_turn(
            running.provenance_id,
            running.generation,
            StopState {
                cause: StopCause::ProcessExited,
                observed_at: 5,
                last_event_seq: Some(999),
                resumable: true,
            },
        )
        .unwrap();
    assert_eq!(stopped.provenance_id, running.provenance_id);
    assert_eq!(stopped.generation, running.generation + 1);
    match &stopped.state {
        ProvenanceTurnState::Stopped(stop) => {
            assert_eq!(stop.cause, StopCause::ProcessExited);
            assert_eq!(stop.last_event_seq, Some(1));
            assert!(stop.resumable);
        }
        other => panic!("expected stopped turn, got {other:?}"),
    }
    let stopped_retry = store
        .stop_provenance_turn(
            running.provenance_id,
            running.generation,
            StopState {
                cause: StopCause::ProcessExited,
                observed_at: 9,
                last_event_seq: None,
                resumable: true,
            },
        )
        .unwrap();
    assert_eq!(stopped_retry, stopped);
    assert!(matches!(
        store.append_provenance_envelope(
            running.provenance_id,
            running.generation,
            "zombie",
            b"zombie",
            6,
        ),
        Err(RedbStoreError::ProvenanceFenced { .. })
    ));

    let resumed = store
        .resume_provenance_turn(stopped.provenance_id, stopped.generation, 7)
        .unwrap();
    assert_eq!(resumed.provenance_id, running.provenance_id);
    assert_eq!(resumed.generation, stopped.generation + 1);
    assert!(matches!(resumed.state, ProvenanceTurnState::Running));
    assert!(matches!(
        store.append_provenance_envelope(
            running.provenance_id,
            stopped.generation,
            "old-generation",
            b"old",
            8,
        ),
        Err(RedbStoreError::ProvenanceFenced { .. })
    ));
    store
        .append_provenance_envelope(
            running.provenance_id,
            resumed.generation,
            "resumed",
            b"resumed",
            8,
        )
        .unwrap();

    let abandoned = store
        .abandon_provenance_turn(
            resumed.provenance_id,
            resumed.generation,
            StopState {
                cause: StopCause::UserRequested,
                observed_at: 9,
                last_event_seq: None,
                resumable: true,
            },
        )
        .unwrap();
    match &abandoned.state {
        ProvenanceTurnState::Abandoned(stop) => {
            assert_eq!(stop.cause, StopCause::Abandoned);
            assert_eq!(stop.last_event_seq, Some(2));
            assert!(!stop.resumable);
        }
        other => panic!("expected abandoned turn, got {other:?}"),
    }
    assert!(matches!(
        store.resume_provenance_turn(abandoned.provenance_id, abandoned.generation, 10,),
        Err(RedbStoreError::InvalidProvenanceTransition { .. })
    ));
}

#[test]
fn bound_checkpoint_repairs_only_legacy_ledger_ordinal() {
    let (_dir, store) = temp_store();
    let running = store
        .reserve_provenance_turn("legacy-count", 21, 1)
        .unwrap();
    let source = ProvenanceCheckpointSource {
        agent_name: "opencode".to_string(),
        agent_display_name: "OpenCode".to_string(),
        agent_vendor: "openai".to_string(),
        change_hashes: vec![Hash::of(b"source")],
        previous_provenance: None,
        plan_id: None,
        ledger_turn_number: 20,
    };
    let prepared = store
        .prepare_provenance_checkpoint(running.provenance_id, running.generation, source.clone(), 2)
        .unwrap();
    let hash = Hash::of(b"provenance");
    let turn = atomic_core::change::session::SessionTurn {
        session_id: "legacy-count".to_string(),
        turn_number: 20,
        goal: None,
        provenance_hash: hash,
        change_hashes: source.change_hashes.clone(),
        previous_provenance: None,
        timestamp: 3,
        plan_id: None,
        todos: Vec::new(),
    };
    store
        .bind_provenance_checkpoint_hash(
            running.provenance_id,
            prepared.attempt_generation,
            hash,
            turn,
            3,
        )
        .unwrap();

    let mut corrected_source = source;
    corrected_source.ledger_turn_number = 0;
    let corrected = store
        .prepare_provenance_checkpoint(
            running.provenance_id,
            running.generation,
            corrected_source,
            4,
        )
        .unwrap();
    assert_eq!(corrected.source.ledger_turn_number, 0);
    assert_eq!(corrected.session_turn.unwrap().turn_number, 0);
}

#[test]
fn stopped_provenance_turn_resumes_and_finalizes_once() {
    let (_dir, store) = temp_store();
    let running = store.reserve_provenance_turn("session-a", 0, 1).unwrap();
    let stopped = store
        .stop_provenance_turn(
            running.provenance_id,
            running.generation,
            StopState {
                cause: StopCause::UserRequested,
                observed_at: 2,
                last_event_seq: None,
                resumable: true,
            },
        )
        .unwrap();
    let resumed = store
        .resume_provenance_turn(stopped.provenance_id, stopped.generation, 3)
        .unwrap();
    assert!(matches!(resumed.state, ProvenanceTurnState::Running));

    let stopped_again = store
        .stop_provenance_turn(
            resumed.provenance_id,
            resumed.generation,
            StopState {
                cause: StopCause::UserRequested,
                observed_at: 3,
                last_event_seq: None,
                resumable: true,
            },
        )
        .unwrap();
    let checkpoint = store
        .begin_provenance_checkpoint(stopped_again.provenance_id, stopped_again.generation, 4)
        .unwrap();
    let hash = Hash::of(b"provenance");
    let completed = store
        .bind_final_provenance_hash(checkpoint.provenance_id, checkpoint.generation, hash, 4)
        .unwrap();
    assert!(matches!(completed.state, ProvenanceTurnState::Completed));
    assert_eq!(completed.final_hash, Some(hash));
    assert_eq!(
        store.get_provenance_turn_by_hash(&hash).unwrap().unwrap(),
        completed
    );

    let same = store
        .bind_final_provenance_hash(completed.provenance_id, 0, hash, 5)
        .unwrap();
    assert_eq!(same, completed);
    assert_eq!(
        store.reserve_provenance_turn("session-a", 0, 99).unwrap(),
        completed
    );
    assert_eq!(
        store
            .stop_provenance_turn(
                completed.provenance_id,
                completed.generation,
                StopState {
                    cause: StopCause::SystemShutdown,
                    observed_at: 6,
                    last_event_seq: None,
                    resumable: true,
                },
            )
            .unwrap(),
        completed
    );
    assert_eq!(
        store
            .abandon_provenance_turn(
                completed.provenance_id,
                completed.generation,
                StopState {
                    cause: StopCause::Abandoned,
                    observed_at: 7,
                    last_event_seq: None,
                    resumable: false,
                },
            )
            .unwrap(),
        completed
    );
}

#[test]
fn non_resumable_provenance_stop_rejects_resume() {
    let (_dir, store) = temp_store();
    let running = store.reserve_provenance_turn("session-a", 0, 1).unwrap();
    let stopped = store
        .stop_provenance_turn(
            running.provenance_id,
            running.generation,
            StopState {
                cause: StopCause::LeaseExpired,
                observed_at: 2,
                last_event_seq: None,
                resumable: false,
            },
        )
        .unwrap();

    assert!(matches!(
        store.resume_provenance_turn(stopped.provenance_id, stopped.generation, 3),
        Err(RedbStoreError::InvalidProvenanceTransition { .. })
    ));
}

#[test]
fn final_provenance_hash_is_unique_across_turns() {
    let (_dir, store) = temp_store();
    let mut checkpoints = Vec::new();
    for turn_number in 0..2 {
        let running = store
            .reserve_provenance_turn("session-a", turn_number, 1)
            .unwrap();
        let stopped = store
            .stop_provenance_turn(
                running.provenance_id,
                running.generation,
                StopState {
                    cause: StopCause::UserRequested,
                    observed_at: 2,
                    last_event_seq: None,
                    resumable: true,
                },
            )
            .unwrap();
        checkpoints.push(
            store
                .begin_provenance_checkpoint(stopped.provenance_id, stopped.generation, 3)
                .unwrap(),
        );
    }

    let hash = Hash::of(b"same-provenance");
    store
        .bind_final_provenance_hash(
            checkpoints[0].provenance_id,
            checkpoints[0].generation,
            hash,
            3,
        )
        .unwrap();
    assert!(matches!(
        store.bind_final_provenance_hash(
            checkpoints[1].provenance_id,
            checkpoints[1].generation,
            hash,
            3,
        ),
        Err(RedbStoreError::ProvenanceFinalHashAlreadyBound { .. })
    ));
    assert!(matches!(
        store
            .get_provenance_turn(checkpoints[1].provenance_id)
            .unwrap()
            .unwrap()
            .state,
        ProvenanceTurnState::Checkpointing
    ));
}

//! Compatibility between canonical database objects and legacy export files.

use atomic_core::change::format_v3::{ChangeWriter, FileHeader, HashDedupTable, WriterOptions};
use atomic_core::change::{Change, ChangeHeader};
use atomic_core::pristine::tables::{CHANGE_BYTES, CHANGE_SIGNATURES};
use atomic_core::pristine::MutTxnT;
use atomic_core::types::Hash;
use atomic_repository::{ChangeStore, Repository, DEFAULT_CACHE_CAPACITY};
use redb::ReadableTable;

fn change(message: &str) -> Change {
    Change::new(
        ChangeHeader::new(message),
        vec![],
        b"contents\n".to_vec(),
        vec![],
    )
}

fn unusual_bytes() -> (Hash, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut writer = ChangeWriter::new(&mut bytes, WriterOptions::max_compression());
    writer
        .write_file_header(&FileHeader::builder().hash_table_entries(1).build())
        .unwrap();
    writer
        .write_hash_table(&HashDedupTable::new([17; 32]))
        .unwrap();
    writer
        .write_change_header(&ChangeHeader::new("original encoding"))
        .unwrap();
    writer.write_dependencies(&[]).unwrap();
    let hash = Hash::from_bytes(writer.finalize().unwrap().content_hash);
    let (decoded, verified) = Change::deserialize(&mut bytes.as_slice()).unwrap();
    assert_eq!(verified, hash);
    let mut serialized = Vec::new();
    decoded.serialize(&mut serialized).unwrap();
    assert_ne!(serialized, bytes, "fixture must detect re-serialization");
    (hash, bytes)
}

#[test]
fn exact_canonical_bytes_survive_missing_export_and_readonly_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let (hash, bytes) = unusual_bytes();
    repo.redb_change_store()
        .unwrap()
        .import_v3_bytes(&bytes)
        .unwrap();
    assert!(!repo.change_store().change_path(&hash).exists());
    assert_eq!(repo.change_store().load_change_bytes(&hash).unwrap(), bytes);
    assert_eq!(
        repo.load_change(&hash).unwrap().hashed.header.message,
        "original encoding"
    );
    drop(repo);

    let repo = Repository::open_readonly(dir.path()).unwrap();
    assert_eq!(repo.change_store().load_change_bytes(&hash).unwrap(), bytes);
}

#[test]
fn canonical_replacement_wins_over_stale_export_and_memory_cache() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let mut object = change("updated transcript");
    object.unhashed = Some(serde_json::json!({"agent_turn": {"transcript": "old"}}));
    let hash = repo.save_change(&object).unwrap();
    let old_bytes = std::fs::read(repo.change_store().change_path(&hash)).unwrap();
    assert_eq!(repo.load_change(&hash).unwrap().unhashed, object.unhashed);

    object.unhashed = Some(serde_json::json!({"agent_turn": {"transcript": "new"}}));
    let mut bytes = Vec::new();
    assert_eq!(object.serialize(&mut bytes).unwrap(), hash);
    repo.redb_change_store()
        .unwrap()
        .import_v3_bytes(&bytes)
        .unwrap();
    assert_eq!(
        std::fs::read(repo.change_store().change_path(&hash)).unwrap(),
        old_bytes
    );
    assert_eq!(repo.change_store().load_change_bytes(&hash).unwrap(), bytes);
    assert_eq!(repo.load_change(&hash).unwrap().unhashed, object.unhashed);
}

#[test]
fn removing_optional_sections_clears_their_query_projections() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let mut object = change("optional sections");
    object.unhashed = Some(serde_json::json!({"transcript": "remove me"}));
    let mut unsigned = Vec::new();
    let hash = object.serialize(&mut unsigned).unwrap();
    object.signature = Some(atomic_core::change::signing::sign_change(
        "did:atomic:test",
        &[7; 32],
        &hash,
        1_700_000_000,
    ));
    assert_eq!(repo.save_change(&object).unwrap(), hash);
    let store = repo.redb_change_store().unwrap();
    assert!(store.load_meta(hash.as_bytes()).unwrap().has_signature);
    assert!(store.load_unhashed(hash.as_bytes()).unwrap().is_some());

    object.unhashed = None;
    object.signature = None;
    assert_eq!(repo.save_change(&object).unwrap(), hash);
    let meta = store.load_meta(hash.as_bytes()).unwrap();
    assert!(!meta.has_signature && !meta.has_unhashed);
    assert_eq!(store.load_unhashed(hash.as_bytes()).unwrap(), None);
    assert!(repo.load_change(&hash).unwrap().signature.is_none());
    let read = repo.pristine().read_txn().unwrap();
    assert!(read
        .redb_transaction()
        .open_table(CHANGE_SIGNATURES)
        .unwrap()
        .get(hash.as_bytes())
        .unwrap()
        .is_none());
}

#[test]
fn raw_reads_verify_legacy_files_and_do_not_mask_corrupt_canonical_objects() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let legacy = ChangeStore::new(repo.changes_dir(), DEFAULT_CACHE_CAPACITY).unwrap();
    let hash = legacy.save_change(&change("legacy")).unwrap();
    let bytes = std::fs::read(legacy.change_path(&hash)).unwrap();
    assert_eq!(repo.change_store().load_change_bytes(&hash).unwrap(), bytes);

    let txn = repo.pristine().write_txn().unwrap();
    txn.redb_transaction()
        .open_table(CHANGE_BYTES)
        .unwrap()
        .insert(hash.as_bytes(), b"broken canonical bytes".as_slice())
        .unwrap();
    txn.commit().unwrap();
    assert!(repo.change_store().load_change_bytes(&hash).is_err());
    assert!(repo.load_change(&hash).is_err());
}

#[test]
fn both_git_import_writers_publish_into_their_existing_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let assembled = repo
        .write_import_recorded(
            ChangeHeader::new("assembled import"),
            &[],
            serde_json::json!({"git": "first"}),
            &[],
            false,
            Default::default(),
        )
        .unwrap();
    let graph = repo
        .write_import_graph_change(
            Change::empty(ChangeHeader::new("graph import")),
            &[],
            false,
            Default::default(),
        )
        .unwrap();
    let hashes: Vec<_> = repo
        .get_view_changes(None)
        .unwrap()
        .into_iter()
        .map(|(_, h)| h)
        .collect();
    assert!(hashes.contains(&assembled.hash));
    assert!(hashes.contains(&graph.hash));
    for hash in [assembled.hash, graph.hash] {
        let exported = std::fs::read(repo.change_store().change_path(&hash)).unwrap();
        std::fs::remove_file(repo.change_store().change_path(&hash)).unwrap();
        assert_eq!(
            repo.change_store().load_change_bytes(&hash).unwrap(),
            exported
        );
    }
}

#[test]
fn prepared_objects_reject_a_future_schema_in_a_borrowed_transaction() {
    use atomic_core::pristine::schema::{SCHEMA_VERSION, SCHEMA_VERSION_KEY};
    use atomic_core::pristine::tables::ATOMIC_META;
    use atomic_repository::redb_change_store::PreparedChange;

    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let mut bytes = Vec::new();
    let hash = change("future schema").serialize(&mut bytes).unwrap();
    let prepared = PreparedChange::from_v3_bytes(&bytes).unwrap();
    let db = repo.pristine().shared_database().unwrap();
    let txn = db.begin_write().unwrap();
    txn.open_table(ATOMIC_META)
        .unwrap()
        .insert(
            SCHEMA_VERSION_KEY,
            (SCHEMA_VERSION + 1).to_le_bytes().as_slice(),
        )
        .unwrap();
    assert!(prepared.write(&txn).is_err());
    assert!(txn
        .open_table(CHANGE_BYTES)
        .unwrap()
        .get(hash.as_bytes())
        .unwrap()
        .is_none());
    // Drop rolls back the caller's unsupported schema update too.
}

#[test]
fn failed_export_removal_preserves_the_canonical_object() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let hash = repo
        .save_change(&change("retain after failed deletion"))
        .unwrap();
    let path = repo.change_store().change_path(&hash);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(repo.delete_change(&hash).is_err());
    assert!(repo
        .redb_change_store()
        .unwrap()
        .has_change(hash.as_bytes())
        .unwrap());
    assert!(repo.load_change(&hash).is_ok());
    std::fs::remove_dir(&path).unwrap();
    assert!(repo.delete_change(&hash).unwrap());
    assert!(!repo.has_change(&hash));
    assert!(repo.load_change(&hash).is_err());
}

#[test]
fn readonly_delete_rejects_before_removing_the_export() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let hash = repo.save_change(&change("readonly deletion")).unwrap();
    let path = repo.change_store().change_path(&hash);
    drop(repo);
    let repo = Repository::open_readonly(dir.path()).unwrap();
    assert!(repo.delete_change(&hash).is_err());
    assert!(path.is_file());
    assert!(repo.load_change(&hash).is_ok());
}

use atomic_core::change::{Change, ChangeHeader};
use atomic_core::pristine::{GraphTxnT, VaultEntry, VaultEntryType};
use atomic_core::Hash;
use atomic_repository::repository::VaultEntryFile;
use atomic_repository::{InsertOptions, RecordOptions, Repository};
use std::{fs, io::Cursor};

fn record(repo: &Repository, path: &str) -> Hash {
    let wc = repo.require_working_copy_id().unwrap();
    fs::write(repo.root().join(path), path.as_bytes()).unwrap();
    repo.add(wc, path, Default::default()).unwrap();
    *repo
        .record(wc, ChangeHeader::new(path), RecordOptions::default())
        .unwrap()
        .hash()
}

fn entry() -> VaultEntry {
    let mut entry = VaultEntry::new(
        VaultEntryType::Memory,
        vec![255, 0, b'x', b'\r', b'\n'],
        "{ \"n\": 18446744073709551615,\n  \"label\": \"中文\", \"label\": \"exact source\" }"
            .into(),
        "2024-01-02T03:04:05.000000001+09:30".into(),
    );
    entry.updated_at = "2024-02-03T04:05:06.123400Z".into();
    entry
}

#[test]
fn complete_entry_file_rebuild_maps_reference_and_retains_every_source_field() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = Repository::init(source_dir.path()).unwrap();
    let hash = record(&source, "source.txt");
    let mut original = entry();
    original.introduced_by = source
        .pristine()
        .read_txn()
        .unwrap()
        .get_internal(&hash)
        .unwrap()
        .unwrap()
        .get();
    source
        .vault_store_entry("memory/exact.md", &original)
        .unwrap();
    let file = source
        .vault_export_entry_file("memory/exact.md", "fixture-origin-a")
        .unwrap();
    let revision = file.revision().unwrap();
    let mut change_file = Vec::new();
    source
        .load_change(&hash)
        .unwrap()
        .serialize(&mut change_file)
        .unwrap();
    let transfer = source_dir.path().join("transfer.entry");
    fs::write(&transfer, file.to_bytes().unwrap()).unwrap();
    drop(source);

    // Reopen the producer; no input to the receiver comes from its DB.
    let reopened = Repository::open(source_dir.path()).unwrap();
    assert_eq!(
        reopened
            .vault_export_entry_file("memory/exact.md", "fixture-origin-a")
            .unwrap()
            .to_bytes()
            .unwrap(),
        fs::read(&transfer).unwrap()
    );
    drop(reopened);
    let receiver_dir = tempfile::tempdir().unwrap();
    let receiver = Repository::init(receiver_dir.path()).unwrap();
    record(&receiver, "receiver-only.txt"); // force different local NodeIds
    let (change, transferred_hash) = Change::deserialize(&mut Cursor::new(change_file)).unwrap();
    assert_eq!(transferred_hash, hash);
    receiver.save_change(&change).unwrap();
    receiver
        .insert_change(&hash, InsertOptions::default())
        .unwrap();
    let receiver_id = receiver
        .pristine()
        .read_txn()
        .unwrap()
        .get_internal(&hash)
        .unwrap()
        .unwrap()
        .get();
    assert_ne!(receiver_id, original.introduced_by);
    let bytes = fs::read(transfer).unwrap();
    assert_eq!(receiver.vault_restore_entry_file(&bytes).unwrap(), revision);
    let manifest = serde_json::to_value(receiver.vault_manifest().unwrap()).unwrap();
    assert_eq!(receiver.vault_restore_entry_file(&bytes).unwrap(), revision);
    assert_eq!(
        serde_json::to_value(receiver.vault_manifest().unwrap()).unwrap(),
        manifest
    );
    drop(receiver);

    let receiver = Repository::open(receiver_dir.path()).unwrap();
    let restored = receiver.vault_retrieve("memory/exact.md").unwrap().unwrap();
    let mut expected = original.clone();
    expected.introduced_by = receiver_id;
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    let retained = receiver
        .vault_retained_entry_file(&revision)
        .unwrap()
        .unwrap();
    assert_eq!(retained, bytes);
    let retained = VaultEntryFile::from_bytes(&retained).unwrap();
    assert_eq!(
        serde_json::to_value(retained.entry()).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert_eq!(retained.introduced_by_hash(), Some(hash));
    assert_eq!(retained.origin(), "fixture-origin-a");
    assert!(receiver.vault_delete("memory/exact.md").unwrap());
    assert_eq!(
        receiver
            .vault_retained_entry_file(&revision)
            .unwrap()
            .unwrap(),
        bytes
    );
}

#[test]
fn complete_entry_revision_changes_for_metadata_and_origin_without_body_change() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let mut entry = entry();
    repo.vault_store_entry("memory/a.md", &entry).unwrap();
    let first = repo
        .vault_export_entry_file("memory/a.md", "origin-a")
        .unwrap();
    entry.frontmatter_json.push(' ');
    repo.vault_store_entry("memory/a.md", &entry).unwrap();
    let second = repo
        .vault_export_entry_file("memory/a.md", "origin-a")
        .unwrap();
    assert_eq!(first.entry().content_hash, second.entry().content_hash);
    assert_ne!(first.revision().unwrap(), second.revision().unwrap());
    assert_ne!(
        second.revision().unwrap(),
        repo.vault_export_entry_file("memory/a.md", "origin-b")
            .unwrap()
            .revision()
            .unwrap()
    );
    assert!(second.introduced_by_hash().is_none()); // pending, unrecorded entry
    let receiver_dir = tempfile::tempdir().unwrap();
    let receiver = Repository::init(receiver_dir.path()).unwrap();
    let bytes = second.to_bytes().unwrap();
    receiver.vault_restore_entry_file(&bytes).unwrap();
    assert_eq!(
        serde_json::to_value(receiver.vault_retrieve("memory/a.md").unwrap().unwrap()).unwrap(),
        serde_json::to_value(entry).unwrap()
    );
}

#[test]
fn complete_entry_restore_rejects_missing_reference_and_preserves_local_work() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let hash = record(&repo, "code.txt");
    let mut entry = entry();
    entry.introduced_by = repo
        .pristine()
        .read_txn()
        .unwrap()
        .get_internal(&hash)
        .unwrap()
        .unwrap()
        .get();
    repo.vault_store_entry("memory/a.md", &entry).unwrap();
    let file = repo
        .vault_export_entry_file("memory/a.md", "origin-a")
        .unwrap();
    let receiver_dir = tempfile::tempdir().unwrap();
    let receiver = Repository::init(receiver_dir.path()).unwrap();
    let error = receiver
        .vault_restore_entry_file(&file.to_bytes().unwrap())
        .unwrap_err();
    assert!(error.to_string().contains("introducing change"));
    assert!(!receiver.has_vault().unwrap());
    assert!(receiver
        .vault_retained_entry_file(&file.revision().unwrap())
        .unwrap()
        .is_none());

    // A pending revision needs no code, but cannot overwrite different work.
    entry.introduced_by = 0;
    repo.vault_store_entry("memory/a.md", &entry).unwrap();
    let file = repo
        .vault_export_entry_file("memory/a.md", "origin-a")
        .unwrap();
    let mut local = entry.clone();
    local.updated_at.push_str(" local edit");
    receiver.vault_store_entry("memory/a.md", &local).unwrap();
    assert!(receiver
        .vault_restore_entry_file(&file.to_bytes().unwrap())
        .unwrap_err()
        .to_string()
        .contains("differs from local state"));
    assert_eq!(
        serde_json::to_value(receiver.vault_retrieve("memory/a.md").unwrap().unwrap()).unwrap(),
        serde_json::to_value(local).unwrap()
    );
    assert!(receiver
        .vault_retained_entry_file(&file.revision().unwrap())
        .unwrap()
        .is_none());
}

#[test]
fn complete_entry_file_rejects_corruption_unknown_fields_and_bad_paths() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.vault_store_entry("memory/a.md", &entry()).unwrap();
    let bytes = repo
        .vault_export_entry_file("memory/a.md", "origin-a")
        .unwrap()
        .to_bytes()
        .unwrap();
    for (key, value) in [
        ("schema", serde_json::json!("atomic.vault.entry.file.v99")),
        ("path", serde_json::json!("../escape")),
        ("origin", serde_json::json!("")),
        ("unknown", serde_json::json!(42)),
    ] {
        let mut value_json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value_json[key] = value;
        assert!(VaultEntryFile::from_bytes(&serde_json::to_vec(&value_json).unwrap()).is_err());
    }
    for (key, value) in [
        ("content_bytes", serde_json::json!([1, 2, 3])),
        ("future_field", serde_json::json!(1)),
        ("introduced_by", serde_json::json!(77)),
    ] {
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json["entry"][key] = value;
        assert!(VaultEntryFile::from_bytes(&serde_json::to_vec(&json).unwrap()).is_err());
    }
    let mut bad = entry();
    bad.introduced_by = 999;
    repo.vault_store_entry("memory/unmapped.md", &bad).unwrap();
    assert!(repo
        .vault_export_entry_file("memory/unmapped.md", "origin-a")
        .unwrap_err()
        .to_string()
        .contains("canonical hash"));
}

#[test]
fn complete_entry_files_survive_legacy_database_consolidation() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    repo.vault_store_entry("memory/a.md", &entry()).unwrap();
    let file = repo
        .vault_export_entry_file("memory/a.md", "origin-a")
        .unwrap();
    let bytes = file.to_bytes().unwrap();
    let revision = repo.vault_restore_entry_file(&bytes).unwrap();
    drop(repo);
    let dot = dir.path().join(".atomic");
    fs::rename(
        dot.join(atomic_repository::DATABASE_FILE),
        dot.join(atomic_repository::LEGACY_PRISTINE_FILE),
    )
    .unwrap();
    drop(
        atomic_repository::redb_change_store::RedbChangeStore::open(dot.join("changes.redb"))
            .unwrap(),
    );
    let repo = Repository::open(dir.path()).unwrap();
    assert!(dot.join(atomic_repository::DATABASE_FILE).is_file());
    assert!(!dot.join("changes.redb").exists());
    assert_eq!(
        repo.vault_retained_entry_file(&revision).unwrap().unwrap(),
        bytes
    );
    assert_eq!(
        repo.vault_export_entry_file("memory/a.md", "origin-a")
            .unwrap()
            .to_bytes()
            .unwrap(),
        bytes
    );
}

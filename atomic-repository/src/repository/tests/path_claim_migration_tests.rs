use super::*;

use atomic_core::pristine::{
    encode_position, PathClaimTxnT, INODES, PATH_CLAIMS, PATH_CLAIM_SCHEMA_KEY, PRISTINE_META,
    REV_INODES, REV_TREE,
};
use redb::ReadableMultimapTable;

fn clear_path_claim_schema(database_path: &std::path::Path) {
    let database = redb::Database::open(database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut metadata = write.open_table(PRISTINE_META).unwrap();
        metadata.remove(PATH_CLAIM_SCHEMA_KEY).unwrap();
    }
    let claim_paths = {
        let table = write.open_multimap_table(PATH_CLAIMS).unwrap();
        table
            .iter()
            .unwrap()
            .map(|row| row.unwrap().0.value().to_string())
            .collect::<Vec<_>>()
    };
    {
        let mut table = write.open_multimap_table(PATH_CLAIMS).unwrap();
        for path in claim_paths {
            table.remove_all(path.as_str()).unwrap();
        }
    }
    write.commit().unwrap();
}

#[test]
fn migration_probe_recognizes_legacy_missing_metadata_without_writing() {
    let (temp, repo) = create_temp_repo();
    let database_path = repo.database_path();
    drop(repo);
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    write.delete_table(PRISTINE_META).unwrap();
    write.commit().unwrap();
    drop(database);
    let before = std::fs::read(&database_path).unwrap();
    let repo = Repository::open_readonly_for_native_repair(temp.path()).unwrap();
    assert!(repo.path_claim_migration_required().unwrap());
    drop(repo);
    assert_eq!(std::fs::read(&database_path).unwrap(), before);
    let repo = Repository::open(temp.path()).unwrap();
    assert!(!repo.path_claim_migration_required().unwrap());
}

#[test]
fn writable_open_atomically_backfills_claims_and_removes_reverse_only_rows() {
    let (temp, repo) = create_temp_repo();
    let path = temp.path().join("file.txt");
    std::fs::write(&path, "content\n").unwrap();
    repo.add("file.txt", TrackingOptions::default()).unwrap();
    repo.record(
        ChangeHeader::new("add file"),
        crate::record::RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    drop(repo);

    let database_path = temp.path().join(".atomic/atomic.redb");
    clear_path_claim_schema(&database_path);
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut reverse = write.open_table(REV_TREE).unwrap();
        reverse.insert(u64::MAX - 1, "file.txt").unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open(temp.path()).unwrap();
    let txn = repo.pristine.read_txn().unwrap();
    assert!(txn.path_claim_schema_is_complete().unwrap());
    assert_eq!(txn.get_path_claims("file.txt").unwrap().len(), 1);
    txn.validate_tree_bijection().unwrap();
    assert!(!txn
        .iter_rev_tree_pairs()
        .unwrap()
        .iter()
        .any(|(inode, _)| inode.get() == u64::MAX - 1));
    drop(txn);
    drop(repo);

    Repository::open_readonly(temp.path()).unwrap();
    let db = redb::ReadOnlyDatabase::open(&database_path).unwrap();
    let read = redb::ReadableDatabase::begin_read(&db).unwrap();
    assert_eq!(
        atomic_core::pristine::build_metadata::read_version(
            &read,
            atomic_core::pristine::build_metadata::MIGRATED_WITH_KEY
        )
        .unwrap()
        .as_deref(),
        Some(atomic_core::pristine::build_metadata::BUILD_VERSION),
    );
}

#[test]
fn migration_recovers_missing_inode_binding_from_inode_graph() {
    let (temp, repo) = create_temp_repo();
    let path = temp.path().join("file.txt");
    std::fs::write(&path, "content\n").unwrap();
    repo.add("file.txt", TrackingOptions::default()).unwrap();
    repo.record(
        ChangeHeader::new("add file"),
        crate::record::RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();

    let txn = repo.pristine.read_txn().unwrap();
    let inode = txn.get_inode("file.txt").unwrap().unwrap();
    let position = txn.inode_position(inode).unwrap().unwrap();
    drop(txn);
    drop(repo);

    let database_path = temp.path().join(".atomic/atomic.redb");
    clear_path_claim_schema(&database_path);
    let database = redb::Database::open(&database_path).unwrap();
    let write = database.begin_write().unwrap();
    {
        let mut inodes = write.open_table(INODES).unwrap();
        inodes.remove(inode.get()).unwrap();
        let mut reverse = write.open_table(REV_INODES).unwrap();
        let encoded = encode_position(position.change.get(), position.pos.get());
        reverse.remove(&encoded).unwrap();
    }
    write.commit().unwrap();
    drop(database);

    let repo = Repository::open(temp.path()).unwrap();
    let txn = repo.pristine.read_txn().unwrap();
    assert!(txn.path_claim_schema_is_complete().unwrap());
    assert_eq!(txn.position_inode(position).unwrap(), Some(inode));
    assert_eq!(txn.inode_position(inode).unwrap(), Some(position));
    assert_eq!(txn.get_path_claims("file.txt").unwrap().len(), 1);
    txn.validate_tree_bijection().unwrap();
    drop(txn);
    drop(repo);

    Repository::open_readonly(temp.path()).unwrap();
}

#[test]
fn migration_replays_causal_rename_chain_without_using_cached_tree_path() {
    let (temp, repo) = create_temp_repo();
    let old = temp.path().join("old.txt");
    std::fs::write(&old, "content\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    repo.record(
        ChangeHeader::new("add old"),
        crate::record::RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    std::fs::rename(&old, temp.path().join("new.txt")).unwrap();
    repo.record(
        ChangeHeader::new("rename"),
        crate::record::RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(true),
    )
    .unwrap();
    drop(repo);

    let database_path = temp.path().join(".atomic/atomic.redb");
    clear_path_claim_schema(&database_path);
    let repo = Repository::open(temp.path()).unwrap();
    let txn = repo.pristine.read_txn().unwrap();
    assert_eq!(txn.get_path_claims("old.txt").unwrap().len(), 2);
    assert_eq!(txn.get_path_claims("new.txt").unwrap().len(), 1);
    assert_eq!(txn.get_inode("old.txt").unwrap(), None);
    assert!(txn.get_inode("new.txt").unwrap().is_some());
    txn.validate_tree_bijection().unwrap();
}

#[test]
fn migration_preserves_unrecorded_add_rename_and_delete_history() {
    let (temp, repo) = create_temp_repo();
    let record = |message| {
        *repo
            .record(
                ChangeHeader::new(message),
                crate::RecordOptions::new()
                    .with_all(true)
                    .save_to_store(true)
                    .apply_after_record(true),
            )
            .unwrap()
            .hash()
    };
    std::fs::write(temp.path().join("old.txt"), b"retained\n").unwrap();
    repo.add("old.txt", TrackingOptions::default()).unwrap();
    let add = record("add");
    let inode = repo.get_file_inode("old.txt").unwrap().unwrap();
    std::fs::rename(temp.path().join("old.txt"), temp.path().join("new.txt")).unwrap();
    let rename = record("rename");
    std::fs::remove_file(temp.path().join("new.txt")).unwrap();
    let delete = record("delete");
    for hash in [&delete, &rename, &add] {
        repo.unrecord(hash, crate::UnrecordOptions::default())
            .unwrap();
    }
    drop(repo);
    clear_path_claim_schema(&temp.path().join(".atomic/atomic.redb"));

    let mut repo = Repository::open(temp.path()).unwrap();
    assert!(repo
        .visible_file_paths(repo.current_view())
        .unwrap()
        .is_empty());
    // Reopening must not depend on the former current TREE or working files.
    repo.insert_change(&add, InsertOptions::default()).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("old.txt", repo.current_view())
            .unwrap(),
        Some(b"retained\n".to_vec())
    );
    assert_eq!(repo.get_file_inode("old.txt").unwrap(), Some(inode));
    repo.insert_change(&rename, InsertOptions::default())
        .unwrap();
    assert_eq!(
        repo.get_file_content_on_view("old.txt", repo.current_view())
            .unwrap(),
        None
    );
    assert_eq!(
        repo.get_file_content_on_view("new.txt", repo.current_view())
            .unwrap(),
        Some(b"retained\n".to_vec())
    );
    assert_eq!(repo.get_file_inode("new.txt").unwrap(), Some(inode));
    let view = repo.current_view().to_string();
    repo.create_view_with_identity("retained", ViewScope::Shared, None)
        .unwrap();
    repo.insert_change_rec(&rename, InsertOptions::default().view("retained"))
        .unwrap();
    repo.insert_change(&delete, InsertOptions::default())
        .unwrap();
    assert_eq!(
        repo.get_file_content_on_view("new.txt", &view).unwrap(),
        None
    );
    let wc = repo.require_working_copy_id().unwrap();
    repo.switch_view(wc, "retained").unwrap();
    assert_eq!(
        std::fs::read(temp.path().join("new.txt")).unwrap(),
        b"retained\n"
    );
    repo.switch_view(wc, &view).unwrap();
    assert!(!temp.path().join("new.txt").exists());
    drop(repo);
    let repo = Repository::open(temp.path()).unwrap();
    assert_eq!(
        repo.get_file_content_on_view("new.txt", "retained")
            .unwrap(),
        Some(b"retained\n".to_vec())
    );
    assert_eq!(
        repo.get_file_content_on_view("new.txt", &view).unwrap(),
        None
    );
}

#[test]
fn migration_does_not_treat_a_merely_registered_change_as_applied() {
    let (temp, repo) = create_temp_repo();
    let mut txn = repo.pristine.write_txn().unwrap();
    // Registration alone is not graph ingestion, and no object exists here.
    txn.register_change(&Hash::of(b"registered but never applied"))
        .unwrap();
    txn.commit().unwrap();
    drop(repo);
    clear_path_claim_schema(&temp.path().join(".atomic/atomic.redb"));
    let repo = Repository::open(temp.path()).unwrap();
    assert!(repo
        .pristine
        .read_txn()
        .unwrap()
        .path_claim_schema_is_complete()
        .unwrap());
}

#[test]
fn migrated_orphans_preserve_concurrent_names_in_both_insert_orders() {
    for reverse in [false, true] {
        let (temp, mut repo) = create_temp_repo();
        let mut hashes = Vec::new();
        for (view, content) in [("left", "left content\n"), ("right", "right content\n")] {
            repo.create_view_with_identity(view, ViewScope::Shared, None)
                .unwrap();
            repo.switch_view(view).unwrap();
            std::fs::write(temp.path().join("same.txt"), content).unwrap();
            repo.add("same.txt", TrackingOptions::default()).unwrap();
            let hash = *repo
                .record(
                    ChangeHeader::new(view),
                    crate::RecordOptions::new()
                        .with_all(true)
                        .save_to_store(true)
                        .apply_after_record(true),
                )
                .unwrap()
                .hash();
            hashes.push(hash);
            repo.unrecord(&hash, crate::UnrecordOptions::default())
                .unwrap();
            std::fs::remove_file(temp.path().join("same.txt")).unwrap();
        }
        drop(repo);
        clear_path_claim_schema(&temp.path().join(".atomic/atomic.redb"));
        let repo = Repository::open(temp.path()).unwrap();
        if reverse {
            hashes.reverse();
        }
        for hash in &hashes {
            repo.insert_change(hash, InsertOptions::default()).unwrap();
        }
        repo.materialize(repo.require_working_copy_id().unwrap())
            .unwrap();
        let content = std::fs::read_to_string(temp.path().join("same.txt")).unwrap();
        assert!(
            content.contains("name conflict")
                && content.contains("left content")
                && content.contains("right content"),
            "{content}"
        );
        assert_eq!(
            repo.pristine
                .read_txn()
                .unwrap()
                .get_path_claims("same.txt")
                .unwrap()
                .len(),
            2
        );
        drop(repo);
        // Reconstruct the derived index again from the same canonical history.
        clear_path_claim_schema(&temp.path().join(".atomic/atomic.redb"));
        let repo = Repository::open(temp.path()).unwrap();
        repo.materialize(repo.require_working_copy_id().unwrap())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.path().join("same.txt")).unwrap(),
            content
        );
    }
}

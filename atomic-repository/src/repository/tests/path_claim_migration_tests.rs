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

    let database_path = temp.path().join(".atomic/pristine.redb");
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

    let database_path = temp.path().join(".atomic/pristine.redb");
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

    let database_path = temp.path().join(".atomic/pristine.redb");
    clear_path_claim_schema(&database_path);
    let repo = Repository::open(temp.path()).unwrap();
    let txn = repo.pristine.read_txn().unwrap();
    assert_eq!(txn.get_path_claims("old.txt").unwrap().len(), 2);
    assert_eq!(txn.get_path_claims("new.txt").unwrap().len(), 1);
    assert_eq!(txn.get_inode("old.txt").unwrap(), None);
    assert!(txn.get_inode("new.txt").unwrap().is_some());
    txn.validate_tree_bijection().unwrap();
}

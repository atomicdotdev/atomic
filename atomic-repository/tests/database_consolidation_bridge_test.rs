//! Database consolidation must retain the bridge's capability and preflight
//! fences, and snapshots must share the consolidated journal's handle safely.

use atomic_repository::redb_change_store::RedbChangeStore;
use atomic_repository::{Repository, DATABASE_FILE, LEGACY_PRISTINE_FILE};

fn legacy_repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    drop(Repository::init(directory.path()).unwrap());
    let dot = directory.path().join(".atomic");
    std::fs::rename(dot.join(DATABASE_FILE), dot.join(LEGACY_PRISTINE_FILE)).unwrap();
    directory
}

#[test]
fn unsupported_bridge_capability_refuses_before_legacy_files_are_moved() {
    use atomic_core::pristine::tables::PRISTINE_META;
    use atomic_core::pristine::REQUIRED_CAPABILITY_PREFIX;

    let directory = legacy_repository();
    let dot = directory.path().join(".atomic");
    let key = format!("{REQUIRED_CAPABILITY_PREFIX}future-test-capability");
    {
        let db = redb::Database::open(dot.join(LEGACY_PRISTINE_FILE)).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(PRISTINE_META)
            .unwrap()
            .insert(key.as_str(), 1)
            .unwrap();
        txn.commit().unwrap();
    }
    let error = Repository::open(directory.path()).unwrap_err();
    assert!(
        error.to_string().contains("future-test-capability"),
        "{error}"
    );
    assert!(dot.join(LEGACY_PRISTINE_FILE).is_file());
    assert!(!dot.join(DATABASE_FILE).exists());
    assert!(!dot.join("legacy").exists());
    assert!(!dot.join("atomic.redb.merging").exists());
}

#[test]
fn workspace_preflight_defers_database_migration_to_an_ordinary_open() {
    let directory = legacy_repository();
    let dot = directory.path().join(".atomic");
    let before = std::fs::read(dot.join(LEGACY_PRISTINE_FILE)).unwrap();
    assert!(Repository::open_for_workspace_transaction(directory.path()).is_err());
    assert!(Repository::open_readonly_for_native_repair(directory.path()).is_err());
    assert!(Repository::open_with_budget(
        directory.path(),
        atomic_repository::ReconcileEffectBudget::MetadataOnly
    )
    .is_err());
    assert_eq!(
        std::fs::read(dot.join(LEGACY_PRISTINE_FILE)).unwrap(),
        before
    );
    assert!(!dot.join(DATABASE_FILE).exists());
    drop(Repository::open(directory.path()).unwrap());
    assert!(!dot.join(LEGACY_PRISTINE_FILE).exists());
    assert!(Repository::open_for_workspace_transaction(directory.path()).is_ok());
}

#[test]
fn snapshot_includes_shared_journal_and_store_retains_the_source_lock() {
    let directory = tempfile::tempdir().unwrap();
    let repo = Repository::init(directory.path().join("repo")).unwrap();
    let source = repo.database_path();
    let snapshot = directory.path().join("snapshot.redb");
    let journal = repo.redb_change_store().unwrap();
    journal
        .reserve_provenance_turn("before-snapshot", 1, 1)
        .unwrap();

    repo.pristine().copy_snapshot(&snapshot).unwrap();
    drop(repo);
    assert!(matches!(
        redb::Database::open(&source),
        Err(redb::DatabaseError::DatabaseAlreadyOpen)
    ));
    journal
        .reserve_provenance_turn("after-snapshot", 1, 2)
        .unwrap();

    let copy = RedbChangeStore::open_existing(&snapshot).unwrap();
    assert!(copy
        .get_provenance_turn_for("before-snapshot", 1)
        .unwrap()
        .is_some());
    assert!(copy
        .get_provenance_turn_for("after-snapshot", 1)
        .unwrap()
        .is_none());
    drop(journal);
    let source = RedbChangeStore::open_existing(&source).unwrap();
    assert!(source
        .get_provenance_turn_for("after-snapshot", 1)
        .unwrap()
        .is_some());
}

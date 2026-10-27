use atomic_core::pristine::schema::{SCHEMA_VERSION, SCHEMA_VERSION_KEY};
use atomic_core::pristine::tables::ATOMIC_META;
use atomic_repository::redb_change_store::RedbChangeStore;
use atomic_repository::{Repository, DATABASE_FILE};
use redb::ReadableDatabase;

#[test]
fn owner_store_must_reject_future_repository_schema() {
    let dir = tempfile::tempdir().unwrap();
    drop(Repository::init(dir.path()).unwrap());
    let path = dir.path().join(".atomic").join(DATABASE_FILE);
    {
        let db = redb::Database::open(&path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(ATOMIC_META)
            .unwrap()
            .insert(
                SCHEMA_VERSION_KEY,
                (SCHEMA_VERSION + 1).to_le_bytes().as_slice(),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let normal_error = Repository::open_existing(dir.path()).unwrap_err();
    assert!(normal_error.to_string().contains("newer"), "{normal_error}");
    eprintln!("normal repository open: {normal_error}");
    // This is the exact production owner open path: ensure_database -> open_existing.
    let resolved = atomic_repository::ensure_database(&dir.path().join(".atomic")).unwrap();
    let result = RedbChangeStore::open_existing(&resolved);
    if let Ok(store) = &result {
        let turn = store
            .reserve_provenance_turn("future-schema", 1, 1700000000)
            .unwrap();
        eprintln!(
            "owner accepted future schema and committed turn: {:?}",
            turn.provenance_id
        );
    }
    assert!(
        result.is_err(),
        "owner must reject unsupported schema before opening tables or committing provenance"
    );
}

#[test]
fn pristine_open_must_reject_future_schema_before_creating_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future.redb");
    {
        let db = redb::Database::create(&path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(ATOMIC_META)
            .unwrap()
            .insert(
                SCHEMA_VERSION_KEY,
                (SCHEMA_VERSION + 1).to_le_bytes().as_slice(),
            )
            .unwrap();
        txn.commit().unwrap();
    }
    let error = atomic_core::pristine::Pristine::open(&path)
        .err()
        .expect("future schema must fail");
    eprintln!("Pristine::open rejected: {error}");
    let db = redb::Database::open(&path).unwrap();
    let txn = db.begin_read().unwrap();
    let tables: Vec<_> = txn.list_tables().unwrap().collect();
    eprintln!("table count after rejected open: {}", tables.len());
    assert_eq!(
        tables.len(),
        1,
        "unsupported databases must remain unmodified"
    );
}

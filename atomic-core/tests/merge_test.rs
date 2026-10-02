//! Merging the legacy `pristine.redb` + `changes.redb` pair into one database.

use std::path::Path;

use atomic_core::crdt::tables as crdt;
use atomic_core::pristine::merge::{merged_legacy_dir, LegacyDatabases};
use atomic_core::pristine::schema::{LEGACY_DIR_KEY, SCHEMA_VERSION, SCHEMA_VERSION_KEY};
use atomic_core::pristine::tables::{
    ATOMIC_META, CHANGE_META, EXTERNAL, GRAPH, PROVENANCE_STORE_META, PROVENANCE_TURNS,
};
use atomic_core::pristine::{GraphTxnT, MutTxnT, Pristine, PristineError, ViewTxnT};
use atomic_core::types::Hash;
use redb::{
    Database, MultimapTableDefinition, ReadableDatabase, ReadableMultimapTable, ReadableTable,
    ReadableTableMetadata, TableDefinition,
};

const UNKNOWN: TableDefinition<u64, u64> = TableDefinition::new("unknown_future_table");

/// A legacy pristine with a view, a registered change, graph edges and CRDT rows.
fn legacy_pristine(path: &Path) -> Hash {
    let hash = Hash::of(b"legacy change");
    let pristine = Pristine::open(path).unwrap();
    let mut txn = pristine.write_txn().unwrap();
    let mut view = txn.open_or_create_view("dev").unwrap();
    let id = txn.register_change(&hash).unwrap();
    txn.put_change(&mut view, id, &hash).unwrap();
    txn.update_view(&view).unwrap();
    txn.commit().unwrap();
    drop(pristine);

    let db = Database::open(path).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut graph = txn.open_multimap_table(GRAPH).unwrap();
        graph.insert(&[1u8; 24], &[2u8; 24]).unwrap();
        graph.insert(&[1u8; 24], &[3u8; 24]).unwrap();
        graph.insert(&[4u8; 24], &[5u8; 24]).unwrap();
        let mut trunks = txn.open_table(crdt::TRUNKS).unwrap();
        trunks.insert(&[7u8; 12], b"trunk".as_slice()).unwrap();
    }
    txn.commit().unwrap();
    hash
}

/// A legacy change store holding only provenance journal state, as in production.
fn legacy_changes(path: &Path) {
    let db = Database::create(path).unwrap();
    let txn = db.begin_write().unwrap();
    {
        let mut meta = txn.open_table(PROVENANCE_STORE_META).unwrap();
        meta.insert("schema_version", 1).unwrap();
        meta.insert("next_provenance_id", 3).unwrap();
        let mut turns = txn.open_table(PROVENANCE_TURNS).unwrap();
        turns.insert(1, b"turn one".as_slice()).unwrap();
        turns.insert(2, b"turn two".as_slice()).unwrap();
        txn.open_table(CHANGE_META).unwrap();
    }
    txn.commit().unwrap();
}

fn open_sources(paths: &[&Path]) -> LegacyDatabases {
    let mut legacy = LegacyDatabases::new();
    for path in paths {
        legacy.add(path).unwrap();
    }
    legacy
}

fn table_rows<V>(db: &Database, definition: TableDefinition<u64, V>) -> Vec<(u64, Vec<u8>)>
where
    V: for<'a> redb::Value<SelfType<'a> = &'a [u8]> + 'static,
{
    let txn = db.begin_read().unwrap();
    let table = txn.open_table(definition).unwrap();
    table
        .iter()
        .unwrap()
        .map(|entry| {
            let (key, value) = entry.unwrap();
            (key.value(), value.value().to_vec())
        })
        .collect()
}

fn graph_rows(
    db: &Database,
    definition: MultimapTableDefinition<&[u8; 24], &[u8; 24]>,
) -> Vec<([u8; 24], [u8; 24])> {
    let txn = db.begin_read().unwrap();
    let table = txn.open_multimap_table(definition).unwrap();
    let mut rows = Vec::new();
    for entry in table.iter().unwrap() {
        let (key, values) = entry.unwrap();
        for value in values {
            rows.push((*key.value(), *value.unwrap().value()));
        }
    }
    rows
}

#[test]
fn merge_copies_both_databases_and_records_the_legacy_dir() {
    let dir = tempfile::tempdir().unwrap();
    let pristine = dir.path().join("pristine.redb");
    let changes = dir.path().join("changes.redb");
    let merged = dir.path().join("atomic.redb");
    let hash = legacy_pristine(&pristine);
    legacy_changes(&changes);

    let report = open_sources(&[&pristine, &changes])
        .merge_into(&merged, "1700000000")
        .unwrap();

    assert_eq!(report.tables["graph"].rows, 3);
    assert_eq!(report.tables["provenance_turns"].rows, 2);
    assert_eq!(report.tables["crdt_trunks"].rows, 1);
    assert!(report.tables.contains_key("change_meta"));
    assert!(report.rows() > 6);

    let merged_db = Database::open(&merged).unwrap();
    let pristine_db = Database::open(&pristine).unwrap();
    let changes_db = Database::open(&changes).unwrap();
    assert_eq!(
        graph_rows(&merged_db, GRAPH),
        graph_rows(&pristine_db, GRAPH)
    );
    assert_eq!(
        table_rows(&merged_db, PROVENANCE_TURNS),
        table_rows(&changes_db, PROVENANCE_TURNS)
    );
    {
        let txn = merged_db.begin_read().unwrap();
        let meta = txn.open_table(ATOMIC_META).unwrap();
        assert_eq!(
            meta.get(SCHEMA_VERSION_KEY).unwrap().unwrap().value(),
            SCHEMA_VERSION.to_le_bytes().as_slice()
        );
        assert_eq!(
            meta.get(LEGACY_DIR_KEY).unwrap().unwrap().value(),
            b"1700000000".as_slice()
        );
        let external = txn.open_table(EXTERNAL).unwrap();
        assert_eq!(external.len().unwrap(), 1);
    }
    drop((merged_db, pristine_db, changes_db));

    assert_eq!(
        merged_legacy_dir(&merged).unwrap().as_deref(),
        Some("1700000000")
    );
    let reopened = Pristine::open(&merged).unwrap();
    let txn = reopened.read_txn().unwrap();
    let view = txn
        .get_view("dev")
        .unwrap()
        .expect("view survives the merge");
    assert_eq!(view.change_count, 1);
    assert!(txn.get_internal(&hash).unwrap().is_some());
}

#[test]
fn merge_refuses_unknown_tables_that_hold_data() {
    let dir = tempfile::tempdir().unwrap();
    let pristine = dir.path().join("pristine.redb");
    let merged = dir.path().join("atomic.redb");
    legacy_pristine(&pristine);
    {
        let db = Database::open(&pristine).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(UNKNOWN).unwrap().insert(1, 2).unwrap();
        txn.commit().unwrap();
    }

    let error = open_sources(&[&pristine])
        .merge_into(&merged, "legacy")
        .unwrap_err();

    assert!(
        matches!(&error, PristineError::Merge { message } if message.contains("unknown_future_table")),
        "{error}"
    );
    assert!(!merged.exists(), "a refused merge must not leave a target");
}

#[test]
fn merge_drops_empty_unknown_tables() {
    let dir = tempfile::tempdir().unwrap();
    let pristine = dir.path().join("pristine.redb");
    let merged = dir.path().join("atomic.redb");
    legacy_pristine(&pristine);
    {
        let db = Database::open(&pristine).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(UNKNOWN).unwrap();
        txn.commit().unwrap();
    }

    open_sources(&[&pristine])
        .merge_into(&merged, "legacy")
        .unwrap();

    let db = Database::open(&merged).unwrap();
    let txn = db.begin_read().unwrap();
    assert!(txn.open_table(UNKNOWN).is_err());
}

#[test]
fn merge_leaves_retired_tables_behind() {
    const FILE_MTIMES: TableDefinition<&str, u64> = TableDefinition::new("file_mtimes");
    let dir = tempfile::tempdir().unwrap();
    let pristine = dir.path().join("pristine.redb");
    let merged = dir.path().join("atomic.redb");
    legacy_pristine(&pristine);
    {
        let db = Database::open(&pristine).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(FILE_MTIMES)
            .unwrap()
            .insert("src/main.rs", 1_700_000_000)
            .unwrap();
        txn.commit().unwrap();
    }

    let report = open_sources(&[&pristine])
        .merge_into(&merged, "legacy")
        .unwrap();

    assert_eq!(report.left_behind, vec!["file_mtimes".to_string()]);
    let db = Database::open(&merged).unwrap();
    assert!(db.begin_read().unwrap().open_table(FILE_MTIMES).is_err());
}

#[test]
fn merge_refuses_a_table_present_in_both_sources() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("pristine.redb");
    let second = dir.path().join("other.redb");
    let merged = dir.path().join("atomic.redb");
    legacy_pristine(&first);
    legacy_pristine(&second);

    let error = open_sources(&[&first, &second])
        .merge_into(&merged, "legacy")
        .unwrap_err();

    assert!(
        matches!(&error, PristineError::Merge { message } if message.contains("more than one")),
        "{error}"
    );
}

#[test]
fn merge_refuses_an_existing_target() {
    let dir = tempfile::tempdir().unwrap();
    let pristine = dir.path().join("pristine.redb");
    let merged = dir.path().join("atomic.redb");
    legacy_pristine(&pristine);
    std::fs::write(&merged, b"not a database").unwrap();

    let error = open_sources(&[&pristine])
        .merge_into(&merged, "legacy")
        .unwrap_err();

    assert!(matches!(error, PristineError::Merge { .. }), "{error}");
    assert_eq!(std::fs::read(&merged).unwrap(), b"not a database");
}

#[test]
fn legacy_sources_cannot_be_opened_while_another_handle_holds_them() {
    let dir = tempfile::tempdir().unwrap();
    let pristine = dir.path().join("pristine.redb");
    legacy_pristine(&pristine);
    let holder = Pristine::open(&pristine).unwrap();

    let error = LegacyDatabases::new().add(&pristine).unwrap_err();

    assert!(
        matches!(&error, PristineError::Database(inner)
            if matches!(inner.as_ref(), redb::DatabaseError::DatabaseAlreadyOpen)),
        "{error}"
    );
    drop(holder);
}

#[test]
fn directly_created_databases_have_no_legacy_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic.redb");
    drop(Pristine::open(&path).unwrap());

    assert_eq!(merged_legacy_dir(&path).unwrap(), None);
}

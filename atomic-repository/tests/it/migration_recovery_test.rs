//! Recovery must preserve acknowledged writes to legacy databases.

use atomic_core::pristine::merge::LegacyDatabases;
use atomic_core::pristine::tables::ATOMIC_META;
use atomic_core::pristine::{MutTxnT, Pristine};
use atomic_repository::{Repository, DATABASE_FILE, LEGACY_PRISTINE_FILE};

#[test]
fn interrupted_publish_must_not_hide_new_legacy_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    drop(Repository::init(&root).unwrap());
    let dot = root.join(".atomic");
    let legacy_path = dot.join(LEGACY_PRISTINE_FILE);
    std::fs::rename(dot.join(DATABASE_FILE), &legacy_path).unwrap();
    {
        let db = redb::Database::open(&legacy_path).unwrap();
        let txn = db.begin_write().unwrap();
        txn.delete_table(ATOMIC_META).unwrap();
        txn.commit().unwrap();
    }

    // Reach exactly the durable state after database.rs:87-88, before
    // retire_legacy_files(). A crash now releases the legacy file locks.
    {
        let mut legacy = LegacyDatabases::new();
        legacy.add(&legacy_path).unwrap();
        let scratch = dot.join("atomic.redb.merging");
        legacy.merge_into(&scratch, "review-crash").unwrap();
        std::fs::rename(scratch, dot.join(DATABASE_FILE)).unwrap();
    }

    // An older CLI still discovers pristine.redb and successfully commits.
    // Its write operation is represented here through the same Pristine API.
    {
        let pristine = Pristine::open_existing(&legacy_path).unwrap();
        let mut txn = pristine.write_txn().unwrap();
        txn.open_or_create_view("committed-after-crash").unwrap();
        txn.commit().unwrap();
    }

    // Refuse this divergent pair and retain both copies for reconciliation.
    let error = Repository::open(&root).unwrap_err();
    assert!(
        error.to_string().contains("changed after migration"),
        "{error}"
    );
    assert!(legacy_path.is_file());
    assert!(dot.join(DATABASE_FILE).is_file());
    let source = Pristine::open_existing(&legacy_path).unwrap();
    let txn = source.read_txn().unwrap();
    assert!(
        atomic_core::pristine::ViewTxnT::get_view(&txn, "committed-after-crash")
            .unwrap()
            .is_some()
    );
}

//! Exclusive maintenance operations used by the repository database owner.

use std::path::{Path, PathBuf};

use redb::ReadableDatabase;
use serde::{Deserialize, Serialize};

/// File sizes before compaction and after the compacted database is closed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DatabaseCompaction {
    pub database: PathBuf,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub reclaimed_bytes: u64,
}

/// Compact an existing database without creating tables or migrating its layout.
///
/// The owner must first drain its store leases and prevent new ones until this
/// returns. redb's file lock excludes other processes; a busy file is reported
/// as a database-open error so the owner can apply its usual wait budget.
pub(super) fn compact_database(path: &Path) -> anyhow::Result<DatabaseCompaction> {
    let mut database = redb::Builder::new().open(path)?;
    atomic_core::pristine::schema::check_schema_version(&database.begin_read()?)?;
    let before_bytes = std::fs::metadata(path)?.len();
    database.compact()?;
    // Closing redb persists allocator metadata and can change the file length.
    // Include that final write in the reported size, while the owner's lease
    // gate still prevents new requests from reopening the database.
    drop(database);
    let after_bytes = std::fs::metadata(path)?.len();
    Ok(DatabaseCompaction {
        database: path.to_path_buf(),
        before_bytes,
        after_bytes,
        reclaimed_bytes: before_bytes.saturating_sub(after_bytes),
    })
}

pub(super) fn is_database_busy(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<redb::DatabaseError>(),
        Some(redb::DatabaseError::DatabaseAlreadyOpen)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::{Database, ReadableTableMetadata, TableDefinition};

    const DATA: TableDefinition<u64, &[u8]> = TableDefinition::new("compaction_data");

    #[test]
    fn compaction_reclaims_space_and_preserves_surviving_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.redb");
        let db = Database::create(&path).unwrap();
        let value = vec![0x5a; 4096];
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(DATA).unwrap();
            for key in 0..1024 {
                table.insert(key, value.as_slice()).unwrap();
            }
        }
        write.commit().unwrap();
        // Keep the old pages pinned through deletion so the fixture has
        // reclaimable space without depending on the allocator's layout.
        let reader = db.begin_read().unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(DATA).unwrap();
            for key in 1..1024 {
                table.remove(key).unwrap();
            }
        }
        write.commit().unwrap();
        drop(reader);
        drop(db);

        let before = std::fs::metadata(&path).unwrap().len();
        let report = compact_database(&path).unwrap();
        assert_eq!(report.before_bytes, before);
        assert!(report.after_bytes < report.before_bytes, "{report:?}");
        assert_eq!(report.after_bytes, std::fs::metadata(&path).unwrap().len());
        assert_eq!(report.reclaimed_bytes, before - report.after_bytes);
        let db = redb::Builder::new().open_read_only(&path).unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(read.list_tables().unwrap().count(), 1);
        let table = read.open_table(DATA).unwrap();
        assert_eq!(table.len().unwrap(), 1);
        assert_eq!(table.get(0).unwrap().unwrap().value(), value);
        drop(table);
        drop(read);
        drop(db);
        // A repeated maintenance request remains valid.
        compact_database(&path).unwrap();
    }

    #[test]
    fn compaction_does_not_create_a_missing_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.redb");
        assert!(compact_database(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn compaction_respects_existing_writer_and_reader_handles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.redb");
        let db = Database::create(&path).unwrap();
        assert!(is_database_busy(&compact_database(&path).unwrap_err()));
        drop(db);
        let db = redb::Builder::new().open_read_only(&path).unwrap();
        assert!(is_database_busy(&compact_database(&path).unwrap_err()));
        drop(db);
        compact_database(&path).unwrap();
    }

    #[test]
    fn compaction_rejects_a_future_repository_schema() {
        use atomic_core::pristine::{schema::SCHEMA_VERSION, tables::ATOMIC_META};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.redb");
        let db = Database::create(&path).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut meta = write.open_table(ATOMIC_META).unwrap();
            meta.insert(
                atomic_core::pristine::schema::SCHEMA_VERSION_KEY,
                (SCHEMA_VERSION + 1).to_le_bytes().as_slice(),
            )
            .unwrap();
        }
        write.commit().unwrap();
        drop(db);
        let error = compact_database(&path).unwrap_err().to_string();
        assert!(error.contains("schema"), "{error}");
    }

    #[test]
    fn compaction_preserves_persistent_savepoints_and_reports_the_blocker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.redb");
        let db = Database::create(&path).unwrap();
        let write = db.begin_write().unwrap();
        let savepoint = write.persistent_savepoint().unwrap();
        write.commit().unwrap();
        drop(db);

        assert!(matches!(
            compact_database(&path)
                .unwrap_err()
                .downcast::<redb::CompactionError>()
                .unwrap(),
            redb::CompactionError::PersistentSavepointExists
        ));
        let db = redb::Builder::new().open(&path).unwrap();
        let write = db.begin_write().unwrap();
        assert_eq!(
            write
                .list_persistent_savepoints()
                .unwrap()
                .collect::<Vec<_>>(),
            vec![savepoint]
        );
    }
}

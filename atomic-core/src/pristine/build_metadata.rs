//! Build identities recorded when a repository database is created or migrated.

use redb::{ReadableTable, TableError, WriteTransaction};

use super::{tables::ATOMIC_META, PristineResult};

/// The build that created this database, if recorded at creation time.
pub const CREATED_WITH_KEY: &str = "created_with";
/// The build that last completed a recorded database migration.
pub const MIGRATED_WITH_KEY: &str = "migrated_with";
/// The build that last ran the normal writable table-initialization open.
pub const LAST_WRITABLE_OPEN_WITH_KEY: &str = "last_writable_open_with";

/// Identity of the build containing the repository implementation.
pub const BUILD_VERSION: &str = env!("ATOMIC_BUILD_VERSION");

pub(crate) fn record_open(txn: &WriteTransaction, is_new: bool) -> PristineResult<()> {
    let mut meta = txn.open_table(ATOMIC_META)?;
    if is_new && meta.get(CREATED_WITH_KEY)?.is_none() {
        meta.insert(CREATED_WITH_KEY, BUILD_VERSION.as_bytes())?;
    }
    meta.insert(LAST_WRITABLE_OPEN_WITH_KEY, BUILD_VERSION.as_bytes())?;
    Ok(())
}

/// Record a completed migration in the same transaction as its schema changes.
pub fn record_migration(txn: &WriteTransaction) -> PristineResult<()> {
    let mut meta = txn.open_table(ATOMIC_META)?;
    meta.insert(MIGRATED_WITH_KEY, BUILD_VERSION.as_bytes())?;
    Ok(())
}

/// Read a recorded build identity without creating metadata in legacy databases.
pub fn read_version(txn: &redb::ReadTransaction, key: &str) -> PristineResult<Option<String>> {
    let table = match txn.open_table(ATOMIC_META) {
        Ok(table) => table,
        Err(TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(value) = table.get(key)? else {
        return Ok(None);
    };
    let value =
        std::str::from_utf8(value.value()).map_err(|_| super::PristineError::Inconsistent {
            message: format!("repository build metadata {key} is not UTF-8"),
        })?;
    Ok(Some(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::ReadableDatabase;

    #[test]
    fn creation_is_immutable_and_writable_open_updates_only_its_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.redb");
        drop(super::super::Pristine::open(&path).unwrap());
        let db = redb::ReadOnlyDatabase::open(&path).unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(
            read_version(&read, CREATED_WITH_KEY).unwrap().as_deref(),
            Some(BUILD_VERSION)
        );
        assert_eq!(
            read_version(&read, LAST_WRITABLE_OPEN_WITH_KEY)
                .unwrap()
                .as_deref(),
            Some(BUILD_VERSION)
        );
        drop(read);
        drop(db);
        let db = redb::Database::open(&path).unwrap();
        let write = db.begin_write().unwrap();
        {
            let mut table = write.open_table(ATOMIC_META).unwrap();
            table
                .insert(CREATED_WITH_KEY, b"old-creator".as_slice())
                .unwrap();
            table
                .insert(LAST_WRITABLE_OPEN_WITH_KEY, b"old-writer".as_slice())
                .unwrap();
        }
        write.commit().unwrap();
        drop(db);

        drop(super::super::Pristine::open_readonly(&path).unwrap());
        drop(super::super::Pristine::open_existing(&path).unwrap());
        let db = redb::ReadOnlyDatabase::open(&path).unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(
            read_version(&read, LAST_WRITABLE_OPEN_WITH_KEY)
                .unwrap()
                .as_deref(),
            Some("old-writer")
        );
        drop(read);
        drop(db);

        drop(super::super::Pristine::open(&path).unwrap());
        let db = redb::ReadOnlyDatabase::open(&path).unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(
            read_version(&read, CREATED_WITH_KEY).unwrap().as_deref(),
            Some("old-creator")
        );
        assert_eq!(
            read_version(&read, LAST_WRITABLE_OPEN_WITH_KEY)
                .unwrap()
                .as_deref(),
            Some(BUILD_VERSION)
        );
        assert_eq!(read_version(&read, MIGRATED_WITH_KEY).unwrap(), None);
    }

    #[test]
    fn legacy_unknown_creator_is_not_invented_and_failed_migration_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.redb");
        drop(super::super::Pristine::open(&path).unwrap());
        let db = redb::Database::open(&path).unwrap();
        let write = db.begin_write().unwrap();
        write
            .open_table(ATOMIC_META)
            .unwrap()
            .remove(CREATED_WITH_KEY)
            .unwrap();
        write.commit().unwrap();
        let write = db.begin_write().unwrap();
        record_migration(&write).unwrap();
        write.abort().unwrap();
        drop(db);
        drop(super::super::Pristine::open(&path).unwrap());
        let db = redb::ReadOnlyDatabase::open(&path).unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(read_version(&read, CREATED_WITH_KEY).unwrap(), None);
        assert_eq!(read_version(&read, MIGRATED_WITH_KEY).unwrap(), None);
    }

    #[test]
    fn creating_a_missing_additive_table_records_the_migrating_build() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomic.redb");
        drop(super::super::Pristine::open(&path).unwrap());
        let db = redb::Database::open(&path).unwrap();
        let write = db.begin_write().unwrap();
        write
            .delete_table(super::super::tables::PENDING_UNTRACKS)
            .unwrap();
        write.commit().unwrap();
        drop(db);
        drop(super::super::Pristine::open(&path).unwrap());
        let db = redb::ReadOnlyDatabase::open(&path).unwrap();
        let read = db.begin_read().unwrap();
        assert_eq!(
            read_version(&read, MIGRATED_WITH_KEY).unwrap().as_deref(),
            Some(BUILD_VERSION)
        );
    }
}

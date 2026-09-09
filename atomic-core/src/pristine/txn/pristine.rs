//! Pristine database handle
//!
//! This module provides the main `Pristine` struct which manages the redb
//! database and provides methods for creating transactions.
//!
//! # Read-Only Mode
//!
//! For commands that only need to read data (like `status`, `diff`, `log`),
//! use `Pristine::open_readonly()` which doesn't acquire a write lock and
//! can run concurrently with other readers. A writable handle still requires
//! exclusive process access; redb 4.2 does not allow readers alongside it.
//!
//! ```ignore
//! // For read-only operations (status, diff, log, change)
//! let pristine = Pristine::open_readonly("path/to/pristine")?;
//! let txn = pristine.read_txn()?;
//!
//! // For read-write operations (record, add, apply)
//! let pristine = Pristine::open("path/to/pristine")?;
//! let mut txn = pristine.write_txn()?;
//! ```

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::{Builder, Database, ReadOnlyDatabase, ReadableDatabase, ReadableTable};

use crate::pristine::error::{PristineError, PristineResult};
use crate::pristine::tables::*;

use super::helpers::deserialize_view_state;
use super::read::ReadTxn;
use super::write::WriteTxn;

/// Return `max_id + 1`, or error if the ID space is exhausted.
fn next_id(max_id: u64) -> PristineResult<u64> {
    max_id.checked_add(1).ok_or(PristineError::IdSpaceExhausted)
}

fn legacy_upgrade_error(error: impl std::fmt::Display) -> PristineError {
    PristineError::Io(std::io::Error::other(format!(
        "failed to upgrade legacy redb database: {error}"
    )))
}

fn upgrade_legacy_database(path: &Path) -> PristineResult<()> {
    let mut legacy = redb_2_6::Database::open(path).map_err(legacy_upgrade_error)?;
    legacy.upgrade().map_err(legacy_upgrade_error)?;
    Ok(())
}

fn open_database(path: &Path, create: bool, cache_bytes: usize) -> PristineResult<Database> {
    let open = || {
        let mut builder = Builder::new();
        builder.set_cache_size(cache_bytes);
        if create {
            builder.create(path)
        } else {
            builder.open(path)
        }
    };

    match open() {
        Ok(database) => Ok(database),
        Err(redb::DatabaseError::UpgradeRequired(_)) => {
            upgrade_legacy_database(path)?;
            Ok(open()?)
        }
        Err(error) => Err(error.into()),
    }
}

/// The pristine database handle
///
/// This is the main entry point for all database operations. It wraps a redb
/// database and provides methods for creating transactions.
///
/// # Example
///
/// ```ignore
/// let pristine = Pristine::open("path/to/pristine")?;
///
/// // Read-only access
/// let read_txn = pristine.read_txn()?;
/// let view = read_txn.get_view("main")?;
///
/// // Read-write access
/// let mut write_txn = pristine.write_txn()?;
/// let view = write_txn.open_or_create_view("main")?;
/// write_txn.commit()?;
/// ```
pub struct Pristine {
    db: PristineDatabase,
    /// Counter for allocating node IDs
    pub(crate) next_node_id: AtomicU64,
    /// Counter for allocating view IDs
    pub(crate) next_view_id: AtomicU64,
    /// Counter for allocating inodes
    pub(crate) next_inode: AtomicU64,
}

enum PristineDatabase {
    Writable(Database),
    ReadOnly(ReadOnlyDatabase),
}

impl PristineDatabase {
    fn begin_read(&self) -> Result<redb::ReadTransaction, redb::TransactionError> {
        match self {
            Self::Writable(db) => db.begin_read(),
            Self::ReadOnly(db) => db.begin_read(),
        }
    }
}

impl Pristine {
    /// Open or create a pristine database at the given path
    ///
    /// This will create all necessary tables if they don't exist.
    pub fn open<P: AsRef<Path>>(path: P) -> PristineResult<Self> {
        // Use 8 GB cache for machines with plenty of RAM.  The default
        // redb cache is 1 GB which causes excessive page eviction when
        // the GRAPH table grows beyond that during large imports.
        let cache_bytes = 8 * 1024 * 1024 * 1024; // 8 GiB
        let db = open_database(path.as_ref(), true, cache_bytes)?;

        // Initialize all tables
        let write_txn = db.begin_write()?;
        {
            // ID mapping tables
            write_txn.open_table(EXTERNAL)?;
            write_txn.open_table(INTERNAL)?;
            write_txn.open_table(NODE_TYPES)?;

            // Graph tables
            write_txn.open_multimap_table(GRAPH)?;
            write_txn.open_multimap_table(INODE_GRAPH)?;

            // View tables
            write_txn.open_table(VIEWS)?;
            write_txn.open_table(VIEW_CHANGES)?;
            write_txn.open_table(REV_VIEW_CHANGES)?;
            write_txn.open_table(CONFLICTS)?;

            // Tree tables
            write_txn.open_table(TREE)?;
            write_txn.open_table(REV_TREE)?;
            write_txn.open_table(INODES)?;
            write_txn.open_table(REV_INODES)?;
            write_txn.open_table(DIRECTORIES)?;

            // File index cache (mtime + size + content hash for fast status detection)
            write_txn.open_table(FILE_INDEX)?;

            // Dependency tables
            write_txn.open_multimap_table(DEPS)?;
            write_txn.open_multimap_table(REV_DEPS)?;
            write_txn.open_multimap_table(CHANGE_DEPS)?;
            write_txn.open_multimap_table(REV_CHANGE_DEPS)?;
            write_txn.open_table(CHANGE_DEPS_INDEXED)?;

            // State tables
            write_txn.open_table(STATES)?;
            write_txn.open_table(MERKLE_CHAIN)?;

            // Tag record tables
            write_txn.open_table(TAG_RECORDS)?;
            write_txn.open_table(TAG_NAME_INDEX)?;

            // Git SHA index (git commit SHA → entity_id)
            write_txn.open_table(GIT_SHA_INDEX)?;

            // Session tables (provenance-derived session data, any agent)
            write_txn.open_table(SESSION_EVENTS)?;
            write_txn.open_table(SESSION_TODOS)?;
            write_txn.open_table(SESSION_PHASES)?;
            write_txn.open_table(SESSION_INTENTS)?;
            write_txn.open_table(SESSIONS)?;
            write_txn.open_table(SESSION_TURNS)?;
            write_txn.open_table(SESSION_PROVENANCE)?;
            write_txn.open_table(SESSION_MANIFESTS)?;
            write_txn.open_table(SESSION_HEADS)?;
        }
        write_txn.commit()?;

        // Determine the next available IDs by scanning existing data.
        // Errors are propagated (not silently skipped) so that open()
        // fails fast on corrupted data rather than underestimating the
        // max ID and reusing an already-allocated slot.
        let read_txn = db.begin_read()?;

        let next_node_id = {
            let table = read_txn.open_table(EXTERNAL)?;
            let mut max_id = 0u64;
            for result in table.iter()? {
                let (k, _) = result?;
                max_id = max_id.max(k.value());
            }
            AtomicU64::new(next_id(max_id)?)
        };

        let next_view_id = {
            let table = read_txn.open_table(VIEWS)?;
            let mut max_id = 0u64;
            for result in table.iter()? {
                let (_, value) = result?;
                let state = deserialize_view_state(value.value())?;
                max_id = max_id.max(state.id);
            }
            AtomicU64::new(next_id(max_id)?)
        };

        let next_inode = {
            let table = read_txn.open_table(INODES)?;
            let mut max_id = 0u64;
            for result in table.iter()? {
                let (k, _) = result?;
                max_id = max_id.max(k.value());
            }
            AtomicU64::new(next_id(max_id)?)
        };

        Ok(Self {
            db: PristineDatabase::Writable(db),
            next_node_id,
            next_view_id,
            next_inode,
        })
    }

    /// Open an existing pristine database without the table-init write lock.
    ///
    /// Unlike [`open`](Self::open), this method skips the `begin_write()` +
    /// table-initialization transaction.  It assumes all tables already exist
    /// (true for any database previously created by `open` or `init`).
    ///
    /// The returned `Pristine` supports [`write_txn`](Self::write_txn) and holds
    /// redb's exclusive process lock for its lifetime. Skipping table setup
    /// avoids a redundant write transaction, but does not permit another
    /// process to open this database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database file doesn't exist, is corrupted,
    /// or the ID-scan read transaction fails.
    pub fn open_existing<P: AsRef<Path>>(path: P) -> PristineResult<Self> {
        let cache_bytes = 8 * 1024 * 1024 * 1024; // 8 GiB
        let db = open_database(path.as_ref(), false, cache_bytes)?;
        Self::scan_ids(PristineDatabase::Writable(db))
    }

    /// Open an existing pristine database in read-only mode
    ///
    /// This method opens the database without acquiring a write lock, allowing
    /// concurrent read access from multiple processes. It's suitable for
    /// read-only operations like `status`, `diff`, `log`, and `change`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The database file doesn't exist
    /// - The database is corrupted
    /// - Read access cannot be obtained
    ///
    /// # Example
    ///
    /// ```ignore
    /// let pristine = Pristine::open_readonly("path/to/pristine.redb")?;
    /// let txn = pristine.read_txn()?;
    /// // Read operations...
    /// ```
    pub fn open_readonly<P: AsRef<Path>>(path: P) -> PristineResult<Self> {
        let cache_bytes = 8 * 1024 * 1024 * 1024; // 8 GiB
        let mut builder = Builder::new();
        builder.set_cache_size(cache_bytes);
        let db = match builder.open_read_only(path.as_ref()) {
            Ok(db) => db,
            Err(redb::DatabaseError::UpgradeRequired(_) | redb::DatabaseError::RepairAborted) => {
                // Migration and crash recovery need a writable handle. Close
                // it cleanly to persist allocator state before acquiring the
                // shared read lock. Never retry ordinary lock contention here.
                drop(open_database(path.as_ref(), false, cache_bytes)?);
                builder.open_read_only(path.as_ref())?
            }
            Err(error) => return Err(error.into()),
        };
        Self::scan_ids(PristineDatabase::ReadOnly(db))
    }

    /// Scan existing tables for the next available IDs.
    ///
    /// Shared implementation for `open_existing` and `open_readonly` — both
    /// skip the table-init write transaction and only need a read pass to
    /// discover the max allocated node, view, and inode IDs.
    fn scan_ids(db: PristineDatabase) -> PristineResult<Self> {
        let read_txn = db.begin_read()?;

        let next_node_id = {
            let table = read_txn.open_table(EXTERNAL)?;
            let mut max_id = 0u64;
            for result in table.iter()? {
                let (k, _) = result?;
                max_id = max_id.max(k.value());
            }
            AtomicU64::new(next_id(max_id)?)
        };

        let next_view_id = {
            let table = read_txn.open_table(VIEWS)?;
            let mut max_id = 0u64;
            for result in table.iter()? {
                let (_, value) = result?;
                let state = deserialize_view_state(value.value())?;
                max_id = max_id.max(state.id);
            }
            AtomicU64::new(next_id(max_id)?)
        };

        let next_inode = {
            let table = read_txn.open_table(INODES)?;
            let mut max_id = 0u64;
            for result in table.iter()? {
                let (k, _) = result?;
                max_id = max_id.max(k.value());
            }
            AtomicU64::new(next_id(max_id)?)
        };

        Ok(Self {
            db,
            next_node_id,
            next_view_id,
            next_inode,
        })
    }

    /// Begin a read-only transaction
    ///
    /// Read transactions can run concurrently with other read transactions
    /// and with a single write transaction.
    pub fn read_txn(&self) -> PristineResult<ReadTxn> {
        let txn = self.db.begin_read()?;
        Ok(ReadTxn::new(txn))
    }

    /// Begin a read-write transaction
    ///
    /// Only one write transaction can be active at a time. The transaction
    /// must be explicitly committed with `commit()` or it will be rolled back
    /// when dropped.
    pub fn write_txn(&self) -> PristineResult<WriteTxn<'_>> {
        let PristineDatabase::Writable(db) = &self.db else {
            return Err(PristineError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cannot write through a read-only pristine handle",
            )));
        };
        let txn = db.begin_write()?;
        Ok(WriteTxn::new(
            txn,
            &self.next_node_id,
            &self.next_view_id,
            &self.next_inode,
        ))
    }

    /// Allocate a new node ID
    ///
    /// This is thread-safe and guaranteed to return unique IDs.
    pub fn alloc_node_id(&self) -> u64 {
        self.next_node_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Get the current next node ID (for testing/debugging)
    pub fn peek_next_node_id(&self) -> u64 {
        self.next_node_id.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_pristine_open() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        // Should be able to create transactions
        let _read = pristine.read_txn().unwrap();
        let _write = pristine.write_txn().unwrap();
    }

    #[test]
    fn read_only_handles_share_access_and_reject_writes() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pristine");
        drop(Pristine::open(&path).unwrap());
        let first = Pristine::open_readonly(&path).unwrap();
        let second = Pristine::open_readonly(&path).unwrap();
        assert!(first.read_txn().is_ok());
        assert!(second.read_txn().is_ok());
        assert!(
            matches!(first.write_txn(), Err(PristineError::Io(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
        );
        assert!(Pristine::open_existing(&path).is_err());
        drop(first);
        drop(second);
        assert!(Pristine::open_existing(&path).is_ok());
    }

    #[test]
    fn read_only_open_upgrades_legacy_v2_database() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("pristine");
        {
            let db = redb_2_6::Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            txn.open_table(redb_2_6::TableDefinition::<u64, &[u8; 32]>::new("external"))
                .unwrap()
                .insert(7, &[42u8; 32])
                .unwrap();
            txn.open_table(redb_2_6::TableDefinition::<&str, &[u8]>::new("views"))
                .unwrap();
            txn.open_table(redb_2_6::TableDefinition::<u64, &[u8; 16]>::new("inodes"))
                .unwrap();
            txn.commit().unwrap();
        }
        let db = Pristine::open_readonly(&path).unwrap();
        assert_eq!(db.peek_next_node_id(), 8);
        let txn = db.read_txn().unwrap();
        let table = txn.txn.open_table(EXTERNAL).unwrap();
        assert_eq!(table.get(7).unwrap().unwrap().value(), &[42u8; 32]);
    }

    #[test]
    fn test_open_upgrades_legacy_v2_database() {
        use redb_2_6::Database as LegacyDatabase;

        const LEGACY_EXTERNAL: redb_2_6::TableDefinition<u64, &[u8; 32]> =
            redb_2_6::TableDefinition::new("external");

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let expected_hash = [42u8; 32];

        {
            let legacy = LegacyDatabase::create(&db_path).unwrap();
            let txn = legacy.begin_write().unwrap();
            {
                let mut external = txn.open_table(LEGACY_EXTERNAL).unwrap();
                external.insert(7, &expected_hash).unwrap();
            }
            txn.commit().unwrap();
        }

        let pristine = Pristine::open(&db_path).unwrap();
        assert_eq!(pristine.peek_next_node_id(), 8);

        let txn = pristine.read_txn().unwrap();
        let external = txn.txn.open_table(EXTERNAL).unwrap();
        assert_eq!(external.get(7).unwrap().unwrap().value(), &expected_hash);
    }

    #[test]
    fn test_pristine_reopen() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");

        // Create and populate
        {
            let pristine = Pristine::open(&db_path).unwrap();
            assert_eq!(pristine.peek_next_node_id(), 1);

            // Allocate some IDs
            pristine.alloc_node_id();
            pristine.alloc_node_id();
            pristine.alloc_node_id();
        }

        // Reopen - counters won't persist unless we actually write data
        // This is expected - the counters are derived from the data
        {
            let pristine = Pristine::open(&db_path).unwrap();
            // Since we didn't actually write any changes, counter resets
            assert_eq!(pristine.peek_next_node_id(), 1);
        }
    }

    #[test]
    fn test_alloc_node_id() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        let id1 = pristine.alloc_node_id();
        let id2 = pristine.alloc_node_id();
        let id3 = pristine.alloc_node_id();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn test_concurrent_read_transactions() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("pristine");
        let pristine = Pristine::open(&db_path).unwrap();

        // Multiple read transactions should work concurrently
        let _read1 = pristine.read_txn().unwrap();
        let _read2 = pristine.read_txn().unwrap();
        let _read3 = pristine.read_txn().unwrap();
    }
}

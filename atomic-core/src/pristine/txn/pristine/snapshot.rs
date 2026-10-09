//! Copy committed database bytes through redb's locked handle. Opening a second
//! handle to the file for `fs::copy` fails under Windows' mandatory file locks.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;

use redb::backends::FileBackend;
use redb::{DatabaseError, StorageBackend};

use super::{Pristine, PristineDatabase, PristineResult};

/// Share access to redb's existing handle without acquiring or releasing a
/// second lock. Redb alone calls `close`, including on a failed database open.
#[derive(Clone, Debug)]
pub(super) struct SnapshotBackend(Arc<FileBackend>);

impl SnapshotBackend {
    pub(super) fn open(path: &Path, create: bool) -> Result<Self, DatabaseError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .truncate(false)
            .open(path)?;
        // create_with_backend also initializes empty files. Existing-only
        // opens must retain their refusal to initialize an empty database.
        if !create && file.metadata()?.len() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "existing pristine database is empty",
            )
            .into());
        }
        Ok(Self(Arc::new(FileBackend::new(file)?)))
    }
}

impl StorageBackend for SnapshotBackend {
    fn len(&self) -> io::Result<u64> {
        self.0.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.0.read(offset, out)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.0.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.0.write(offset, data)
    }

    fn close(&self) -> io::Result<()> {
        self.0.close()
    }
}

impl Pristine {
    /// Copy all committed database state to a new file, retaining the source's
    /// process lock and excluding writers for the entire copy. Pristine commits
    /// use immediate durability, so all committed bytes are already on disk.
    /// The copy can be opened independently while this database remains open.
    ///
    /// Do not call while holding a write transaction on this pristine. The
    /// destination must not exist; on I/O failure it may contain a partial copy.
    /// Read-only pristine handles cannot create snapshots.
    pub fn copy_snapshot(&self, destination: &Path) -> PristineResult<()> {
        let _writer = self.db.writable_db()?.begin_write()?;
        let PristineDatabase::Writable(_, backend) = &self.db else {
            unreachable!("writable_db rejects read-only pristines");
        };
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(destination)?;
        let len = backend.len()?;
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut offset = 0;
        while offset < len {
            let count = (len - offset).min(buffer.len() as u64) as usize;
            backend.read(offset, &mut buffer[..count])?;
            output.write_all(&buffer[..count])?;
            offset += count as u64;
        }
        output.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pristine::MutTxnT;
    use redb::{Database, MultimapTableDefinition, TableDefinition};
    use std::fs::File;

    const VALUES: TableDefinition<&str, u64> = TableDefinition::new("snapshot_values");
    const LINKS: MultimapTableDefinition<u64, u64> = MultimapTableDefinition::new("snapshot_links");

    #[test]
    fn snapshot_waits_for_a_writer_and_captures_its_commit() {
        use std::sync::mpsc::{channel, RecvTimeoutError};
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let source = Arc::new(Pristine::open(directory.path().join("source.redb")).unwrap());
        let destination = directory.path().join("copy.redb");
        let pending = source.db.writable_db().unwrap().begin_write().unwrap();
        pending
            .open_table(VALUES)
            .unwrap()
            .insert("committed", 42)
            .unwrap();
        let (started_send, started_recv) = channel();
        let (done_send, done_recv) = channel();
        let worker = {
            let source = Arc::clone(&source);
            let destination = destination.clone();
            std::thread::spawn(move || {
                started_send.send(()).unwrap();
                let result = source
                    .copy_snapshot(&destination)
                    .map_err(|error| error.to_string());
                done_send.send(result).unwrap();
            })
        };
        started_recv.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(
            done_recv.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        ));
        assert!(!destination.exists());
        pending.commit().unwrap();
        done_recv
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        worker.join().unwrap();

        let copy = Pristine::open_existing(&destination).unwrap();
        assert_eq!(
            copy.db
                .begin_read()
                .unwrap()
                .open_table(VALUES)
                .unwrap()
                .get("committed")
                .unwrap()
                .unwrap()
                .value(),
            42
        );
    }

    #[test]
    fn snapshot_preserves_tables_without_releasing_source_lock() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.redb");
        let copy_path = directory.path().join("copy.redb");
        let source = Pristine::open(&source_path).unwrap();
        let write = source.db.writable_db().unwrap().begin_write().unwrap();
        write
            .open_table(VALUES)
            .unwrap()
            .insert("value", 42)
            .unwrap();
        {
            let mut links = write.open_multimap_table(LINKS).unwrap();
            links.insert(1, 2).unwrap();
            links.insert(1, 3).unwrap();
        }
        write.commit().unwrap();

        source.copy_snapshot(&copy_path).unwrap();
        assert!(matches!(
            Database::open(&source_path),
            Err(DatabaseError::DatabaseAlreadyOpen)
        ));
        let copy = Pristine::open_existing(&copy_path).unwrap();
        let read = copy.db.begin_read().unwrap();
        assert_eq!(
            read.open_table(VALUES)
                .unwrap()
                .get("value")
                .unwrap()
                .unwrap()
                .value(),
            42
        );
        let links: Vec<_> = read
            .open_multimap_table(LINKS)
            .unwrap()
            .get(1)
            .unwrap()
            .map(|value| value.unwrap().value())
            .collect();
        assert_eq!(links, vec![2, 3]);
        drop(read);

        // Mutations of the copy never reach the live source, and source writes
        // after the copy cannot change its captured state.
        let write = copy.db.writable_db().unwrap().begin_write().unwrap();
        write
            .open_table(VALUES)
            .unwrap()
            .insert("copy-only", 7)
            .unwrap();
        write.commit().unwrap();
        let write = source.db.writable_db().unwrap().begin_write().unwrap();
        write
            .open_table(VALUES)
            .unwrap()
            .insert("value", 43)
            .unwrap();
        write.commit().unwrap();
        assert!(source
            .db
            .begin_read()
            .unwrap()
            .open_table(VALUES)
            .unwrap()
            .get("copy-only")
            .unwrap()
            .is_none());
        drop(copy);
        let reopened = Pristine::open_existing(&copy_path).unwrap();
        assert_eq!(
            reopened
                .db
                .begin_read()
                .unwrap()
                .open_table(VALUES)
                .unwrap()
                .get("value")
                .unwrap()
                .unwrap()
                .value(),
            42
        );
        assert!(matches!(
            Database::open(&source_path),
            Err(DatabaseError::DatabaseAlreadyOpen)
        ));
        drop(source);
        Pristine::open_existing(&source_path).unwrap();
    }

    #[test]
    fn snapshot_never_overwrites_an_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.redb");
        let source = Pristine::open(&source_path).unwrap();
        let destination = directory.path().join("existing");
        std::fs::write(&destination, b"keep existing data").unwrap();
        assert!(source.copy_snapshot(&destination).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"keep existing data");
        assert!(source.copy_snapshot(&source_path).is_err());
        source.write_txn().unwrap().commit().unwrap();
        drop(source);
        Pristine::open_existing(&source_path).unwrap();
    }

    #[test]
    fn readonly_handles_refuse_snapshot_without_creating_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("source.redb");
        drop(Pristine::open(&source_path).unwrap());
        let source = Pristine::open_readonly(&source_path).unwrap();
        let destination = directory.path().join("copy.redb");
        assert!(source.copy_snapshot(&destination).is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn existing_only_open_does_not_initialize_an_empty_database() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("empty.redb");
        File::create_new(&source_path).unwrap();
        assert!(Pristine::open_existing(&source_path).is_err());
        assert_eq!(std::fs::metadata(source_path).unwrap().len(), 0);
    }
}

//! Repository-facing FILE_INDEX_V2 access and legacy backfill.

use super::*;

use atomic_core::output::project_inode_attributes;
use atomic_core::pristine::{
    FileIndexTimestamp, FileIndexV2Entry, FileIndexV2Key, FileIndexV2MutTxnT, FileIndexV2TxnT,
};

/// Result of a legacy FILE_INDEX to FILE_INDEX_V2 backfill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileIndexV2BackfillOutcome {
    pub migrated: usize,
    pub missing_on_disk: usize,
}

impl Repository {
    /// Read one V2 index row. `None` means the caller must rebuild or verify.
    pub fn file_index_v2(
        &self,
        working_copy: WorkingCopyId,
        path: &RepoPath,
    ) -> Result<Option<FileIndexV2Entry>, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(map_file_index_error)?;
        txn.get_file_index_v2(working_copy, path.as_bytes())
            .map_err(map_file_index_error)
    }

    /// Read multiple V2 rows in input order using one pristine table open.
    pub fn file_index_v2_batch(
        &self,
        working_copy: WorkingCopyId,
        paths: &[RepoPath],
    ) -> Result<Vec<Option<FileIndexV2Entry>>, RepositoryError> {
        let keys = paths
            .iter()
            .map(|path| FileIndexV2Key::new(working_copy, path.as_bytes().to_vec()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_file_index_error)?;
        let txn = self.pristine.read_txn().map_err(map_file_index_error)?;
        txn.get_file_index_v2_batch(&keys)
            .map_err(map_file_index_error)
    }

    /// Iterate one working copy's V2 rows in raw path byte order.
    pub fn iter_file_index_v2(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Vec<(RepoPath, FileIndexV2Entry)>, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(map_file_index_error)?;
        txn.iter_file_index_v2(working_copy)
            .map_err(map_file_index_error)?
            .into_iter()
            .map(|(path, entry)| {
                RepoPath::new(path)
                    .map(|path| (path, entry))
                    .map_err(map_project_tree_error)
            })
            .collect()
    }

    /// Insert or replace V2 rows in one write transaction.
    pub fn put_file_index_v2_batch(
        &self,
        working_copy: WorkingCopyId,
        entries: &[(RepoPath, FileIndexV2Entry)],
    ) -> Result<(), RepositoryError> {
        let entries = entries
            .iter()
            .map(|(path, entry)| {
                Ok((
                    FileIndexV2Key::new(working_copy, path.as_bytes().to_vec())?,
                    entry.clone(),
                ))
            })
            .collect::<Result<Vec<_>, atomic_core::pristine::PristineError>>()
            .map_err(map_file_index_error)?;
        let mut txn = self.pristine.write_txn().map_err(map_file_index_error)?;
        txn.put_file_index_v2_batch(&entries)
            .map_err(map_file_index_error)?;
        txn.commit().map_err(map_file_index_error)
    }

    /// Apply V2 clean-lease replacements and deletions atomically.
    pub(crate) fn update_file_index_v2_transaction(
        &self,
        working_copy: WorkingCopyId,
        updates: &[(RepoPath, FileIndexV2Entry)],
        deletions: &[RepoPath],
    ) -> Result<(), RepositoryError> {
        if updates.is_empty() && deletions.is_empty() {
            return Ok(());
        }
        // A read-only pristine cannot persist the derived index; the next
        // writable status recomputes it. Failing closed here would turn
        // ordinary read-only opens (status/log/diff) into hard errors.
        if self.pristine.is_read_only() {
            return Ok(());
        }
        let updates = updates
            .iter()
            .map(|(path, entry)| {
                Ok((
                    FileIndexV2Key::new(working_copy, path.as_bytes().to_vec())?,
                    entry.clone(),
                ))
            })
            .collect::<Result<Vec<_>, atomic_core::pristine::PristineError>>()
            .map_err(map_file_index_error)?;
        let deletions = deletions
            .iter()
            .map(|path| FileIndexV2Key::new(working_copy, path.as_bytes().to_vec()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_file_index_error)?;
        let mut txn = self.pristine.write_txn().map_err(map_file_index_error)?;
        txn.put_file_index_v2_batch(&updates)
            .map_err(map_file_index_error)?;
        txn.del_file_index_v2_batch(&deletions)
            .map_err(map_file_index_error)?;
        txn.commit().map_err(map_file_index_error)
    }

    /// Delete V2 rows in one write transaction.
    pub fn delete_file_index_v2_batch(
        &self,
        working_copy: WorkingCopyId,
        paths: &[RepoPath],
    ) -> Result<(), RepositoryError> {
        let keys = paths
            .iter()
            .map(|path| FileIndexV2Key::new(working_copy, path.as_bytes().to_vec()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_file_index_error)?;
        let mut txn = self.pristine.write_txn().map_err(map_file_index_error)?;
        txn.del_file_index_v2_batch(&keys)
            .map_err(map_file_index_error)?;
        txn.commit().map_err(map_file_index_error)
    }

    /// Backfill scoped legacy FILE_INDEX rows without modifying legacy storage.
    ///
    /// Legacy hashes remain the content identities. Filesystem identity and
    /// nanosecond timestamps are observed with `symlink_metadata`; canonical
    /// mode and kind come from the graph projection for the working copy's
    /// desired view. Missing files are skipped and reported. All graph/legacy
    /// reads finish before the single V2 write transaction begins.
    pub fn backfill_file_index_v2(
        &self,
        working_copy: WorkingCopyId,
        index_write_time: FileIndexTimestamp,
        conversion_policy: Hash,
    ) -> Result<FileIndexV2BackfillOutcome, RepositoryError> {
        let candidates = {
            let txn = self.pristine.read_txn().map_err(map_file_index_error)?;
            let record = txn
                .get_working_copy(working_copy)
                .map_err(map_file_index_error)?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "working copy {working_copy} is not registered"
                    ))
                })?;
            let view = txn
                .get_view_by_id(record.desired_view)
                .map_err(map_file_index_error)?
                .ok_or_else(|| {
                    RepositoryError::Database(format!(
                        "working copy {working_copy} references missing view {}",
                        record.desired_view
                    ))
                })?;
            let visibility = graph_visibility_closure(&txn, &view)?;
            let legacy = txn.iter_file_index().map_err(map_file_index_error)?;
            let scoped_prefix = format!("{working_copy}\0");
            let mut legacy_by_path = std::collections::BTreeMap::new();
            for (key, seconds, nanoseconds, size, content_id) in &legacy {
                if !key.contains('\0') {
                    legacy_by_path
                        .insert(key.clone(), (*seconds, *nanoseconds, *size, *content_id));
                }
            }
            for (key, seconds, nanoseconds, size, content_id) in legacy {
                if let Some(path) = key.strip_prefix(&scoped_prefix) {
                    legacy_by_path
                        .insert(path.to_string(), (seconds, nanoseconds, size, content_id));
                }
            }
            let mut candidates = Vec::with_capacity(legacy_by_path.len());
            for (path, (_seconds, _nanoseconds, _size, content_id)) in legacy_by_path {
                let inode = txn
                    .get_inode(&path)
                    .map_err(map_file_index_error)?
                    .ok_or_else(|| {
                        RepositoryError::Database(format!(
                            "legacy FILE_INDEX path '{path}' has no graph inode"
                        ))
                    })?;
                let position = txn
                    .inode_position(inode)
                    .map_err(map_file_index_error)?
                    .ok_or_else(|| {
                        RepositoryError::Database(format!(
                            "legacy FILE_INDEX path '{path}' has no graph position"
                        ))
                    })?;
                let projection =
                    project_inode_attributes(&txn, position, visibility.attribute_visibility())
                        .map_err(map_file_index_error)?;
                if projection.is_conflicted() {
                    return Err(RepositoryError::Database(format!(
                        "legacy FILE_INDEX path '{path}' has conflicted canonical attributes"
                    )));
                }
                candidates.push((
                    RepoPath::from_bytes(path.as_bytes()).map_err(map_project_tree_error)?,
                    content_id,
                    projection.materialization,
                ));
            }
            candidates
        };

        let mut outcome = FileIndexV2BackfillOutcome::default();
        let mut entries = Vec::with_capacity(candidates.len());
        for (path, content_id, materialization) in candidates {
            let native_path = self
                .root
                .join(path.to_native().map_err(map_project_tree_error)?);
            let metadata = match std::fs::symlink_metadata(&native_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    outcome.missing_on_disk += 1;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            entries.push((
                path,
                entry_from_metadata(
                    &metadata,
                    content_id,
                    materialization.mode,
                    materialization.kind,
                    index_write_time,
                    conversion_policy,
                )?,
            ));
        }
        outcome.migrated = entries.len();
        self.put_file_index_v2_batch(working_copy, &entries)?;
        Ok(outcome)
    }
}

#[cfg(unix)]
fn entry_from_metadata(
    metadata: &std::fs::Metadata,
    content_id: Hash,
    canonical_mode: u16,
    canonical_kind: atomic_core::change::InodeKind,
    racy_after: FileIndexTimestamp,
    conversion_policy: Hash,
) -> Result<FileIndexV2Entry, RepositoryError> {
    use std::os::unix::fs::MetadataExt;

    FileIndexV2Entry::complete(
        metadata.dev(),
        metadata.ino(),
        FileIndexTimestamp::new(metadata.mtime(), metadata.mtime_nsec() as u32)
            .map_err(map_file_index_error)?,
        FileIndexTimestamp::new(metadata.ctime(), metadata.ctime_nsec() as u32)
            .map_err(map_file_index_error)?,
        metadata.size(),
        content_id,
        canonical_mode,
        canonical_kind,
        racy_after,
        conversion_policy,
    )
    .map_err(map_file_index_error)
}

#[cfg(not(unix))]
fn entry_from_metadata(
    _metadata: &std::fs::Metadata,
    _content_id: Hash,
    _canonical_mode: u16,
    _canonical_kind: atomic_core::change::InodeKind,
    _racy_after: FileIndexTimestamp,
    _conversion_policy: Hash,
) -> Result<FileIndexV2Entry, RepositoryError> {
    Err(RepositoryError::Database(
        "FILE_INDEX_V2 filesystem identity backfill is unsupported on this platform".into(),
    ))
}

fn map_file_index_error(error: atomic_core::pristine::PristineError) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

fn map_project_tree_error(error: ProjectTreeError) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::record::RecordOptions;
    use crate::status::{FileStatus, StatusOptions};
    use atomic_core::change::ChangeHeader;
    use atomic_core::pristine::TreeTxnT;
    use tempfile::tempdir;

    #[test]
    fn status_cold_backfills_v2_and_warm_status_stays_clean() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        std::fs::write(dir.path().join("tracked.txt"), b"canonical").unwrap();
        repo.add(working_copy, "tracked.txt", TrackingOptions::default())
            .unwrap();
        repo.record(
            working_copy,
            ChangeHeader::new("seed"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

        let path = RepoPath::from_bytes(b"tracked.txt").unwrap();
        assert!(repo.file_index_v2(working_copy, &path).unwrap().is_none());
        let cold = repo
            .status(working_copy, StatusOptions::tracked_only())
            .unwrap();
        assert!(cold.entries().is_empty());
        assert!(repo.file_index_v2(working_copy, &path).unwrap().is_some());
        let warm = repo
            .status(working_copy, StatusOptions::tracked_only())
            .unwrap();
        assert!(warm.entries().is_empty());
    }

    #[test]
    fn status_rehashes_same_size_content_with_restored_mtime() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        let native = dir.path().join("tracked.txt");
        std::fs::write(&native, b"abcdefgh").unwrap();
        repo.add(working_copy, "tracked.txt", TrackingOptions::default())
            .unwrap();
        repo.record(
            working_copy,
            ChangeHeader::new("seed"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
        repo.status(working_copy, StatusOptions::tracked_only())
            .unwrap();
        let original_mtime = std::fs::metadata(&native).unwrap().modified().unwrap();

        std::fs::write(&native, b"ABCDEFGH").unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&native)
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(original_mtime))
            .unwrap();

        let status = repo
            .status(working_copy, StatusOptions::tracked_only())
            .unwrap();
        assert_eq!(
            status
                .get(std::path::Path::new("tracked.txt"))
                .unwrap()
                .status(),
            FileStatus::Modified
        );
    }

    #[test]
    fn legacy_backfill_observes_disk_reopens_and_preserves_legacy_row() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let working_copy = repo.require_working_copy_id().unwrap();
        std::fs::write(dir.path().join("tracked.txt"), b"same-size").unwrap();
        repo.add(working_copy, "tracked.txt", TrackingOptions::default())
            .unwrap();
        repo.record(
            working_copy,
            ChangeHeader::new("seed"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();

        {
            let mut txn = repo.pristine.write_txn().unwrap();
            let scoped = txn
                .get_working_copy_file_index(working_copy, "tracked.txt")
                .unwrap()
                .unwrap();
            txn.put_file_index("tracked.txt", scoped.0, scoped.1, scoped.2, &scoped.3)
                .unwrap();
            txn.del_working_copy_file_index(working_copy, "tracked.txt")
                .unwrap();
            txn.commit().unwrap();
        }
        let legacy_before = {
            let txn = repo.pristine.read_txn().unwrap();
            txn.get_file_index("tracked.txt").unwrap().unwrap()
        };
        let boundary = FileIndexTimestamp::new(i64::MAX - 1, 999_999_999).unwrap();
        let policy = Hash::of(b"conversion policy");
        let outcome = repo
            .backfill_file_index_v2(working_copy, boundary, policy)
            .unwrap();
        assert_eq!(outcome.migrated, 1);
        assert_eq!(outcome.missing_on_disk, 0);

        let path = RepoPath::from_bytes(b"tracked.txt").unwrap();
        let row = repo.file_index_v2(working_copy, &path).unwrap().unwrap();
        assert!(row.is_complete());
        assert_eq!(row.content_id, Some(legacy_before.3));
        assert_eq!(row.size, Some(9));
        assert_eq!(row.conversion_policy, Some(policy));
        assert_eq!(
            row.canonical_kind,
            Some(atomic_core::change::InodeKind::Regular)
        );
        assert_eq!(row.canonical_mode, Some(0o644));

        let legacy_after = {
            let txn = repo.pristine.read_txn().unwrap();
            txn.get_file_index("tracked.txt").unwrap().unwrap()
        };
        assert_eq!(legacy_after, legacy_before);

        drop(repo);
        let reopened = Repository::open(dir.path()).unwrap();
        assert_eq!(
            reopened.file_index_v2(working_copy, &path).unwrap(),
            Some(row)
        );
    }
}

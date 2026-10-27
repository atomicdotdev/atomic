//! Repository-facing FILE_INDEX_V2 access and legacy backfill.

use super::*;

use atomic_core::output::project_inode_attributes;
use atomic_core::pristine::{
    FileIndexTimestamp, FileIndexV2Entry, FileIndexV2Key, FileIndexV2MutTxnT, FileIndexV2TxnT,
    WorkingCopyTxnT,
};

/// Result of a legacy FILE_INDEX to FILE_INDEX_V2 backfill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileIndexV2BackfillOutcome {
    pub migrated: usize,
    pub missing_on_disk: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileIndexV2SeedOutcome {
    pub seeded: usize,
    /// Canonical files without a stable, clean filesystem observation.
    pub uncached: usize,
}

impl Repository {
    /// Establish V2 clean leases at the final verified import boundary.
    ///
    /// `project` must be the canonical projection just produced by this
    /// repository and compared to the imported Git tree. The proof is checked
    /// against that manifest, and its effective change identity is checked
    /// again under the index publication transaction. This is not a substitute
    /// for proving the graph/Git equivalence at the caller's import boundary.
    ///
    /// Filesystem bytes are independently cleaned and compared with the supplied
    /// canonical bytes, never used as canonical authority. Dirty, absent,
    /// replaced, racy or unstable entries lose any prior lease. No graph content
    /// is reconstructed and no working file is modified here.
    pub fn seed_verified_import_file_index_v2(
        &self,
        working_copy: WorkingCopyId,
        project: &ProjectTree,
        proof: &VerifiedProspectiveEquivalence,
    ) -> Result<FileIndexV2SeedOutcome, RepositoryError> {
        self.seed_verified_import_file_index_v2_with_provider(
            working_copy,
            project,
            proof,
            &crate::change_source::RealFilesystemProvider,
        )
    }

    fn seed_verified_import_file_index_v2_with_provider(
        &self,
        working_copy: WorkingCopyId,
        project: &ProjectTree,
        proof: &VerifiedProspectiveEquivalence,
        provider: &dyn crate::change_source::FilesystemProvider,
    ) -> Result<FileIndexV2SeedOutcome, RepositoryError> {
        use crate::change_source::{
            conversion_policy_fingerprint, now_timestamp, verify_candidates_with_provider,
            CanonicalTrackedPath, ChangeSourceRequest, ScanChangeSource,
        };
        let invalid = |message: &str| RepositoryError::InvalidOperation {
            message: message.into(),
        };
        self.validate_working_copy(working_copy)?;
        if proof.manifest_root() != &project.manifest.root()
            || proof.expected_git_tree() != &project.git.root
        {
            return Err(invalid(
                "V2 import seed manifest does not match verified import proof",
            ));
        }
        let baseline = self.working_copy_record(working_copy)?;
        let txn = self.pristine.read_txn().map_err(map_file_index_error)?;
        let view = ViewTxnT::get_view_by_id(&txn, baseline.desired_view)?
            .ok_or_else(|| invalid("V2 import seed view disappeared"))?;
        if view.state != baseline.desired_state
            || effective_projection_identity(&txn, &view)?.set_id != project.manifest.set_id
        {
            return Err(invalid("V2 import seed projection is stale"));
        }
        drop(txn);

        let mut expected = std::collections::BTreeMap::new();
        let mut tracked = Vec::new();
        for entry in &project.manifest.entries {
            if entry.kind == atomic_core::change::InodeKind::Gitlink {
                continue; // Status does not verify submodule directories as file bytes.
            }
            // ProjectTree uses escaped canonical identities; V2 keys and the
            // filesystem verifier use raw paths, as ordinary status does.
            let identity = std::str::from_utf8(entry.path.as_bytes())
                .map_err(|_| invalid("V2 import seed has a noncanonical path identity"))?;
            let path = RepoPath::new(unescape_repo_path(identity).map_err(map_project_tree_error)?)
                .map_err(map_project_tree_error)?;
            if expected
                .insert(path.clone(), Hash::of(&entry.repository_bytes))
                .is_some()
            {
                return Err(invalid("V2 import seed has duplicate physical paths"));
            }
            // Include tracked private/excluded entries too: status checks them
            // even though they are omitted from the Git tree projection.
            tracked.push(CanonicalTrackedPath {
                path,
                canonical_mode: entry.mode,
                canonical_kind: entry.kind,
            });
        }
        tracked.sort_by(|a, b| a.path.cmp(&b.path));
        let paths: Vec<_> = tracked.iter().map(|entry| entry.path.clone()).collect();
        let policy = conversion_policy_fingerprint(&self.root, &paths)
            .map_err(|error| RepositoryError::Output(error.to_string()))?;
        let request = || ChangeSourceRequest {
            root: &self.root,
            tracked_paths: &paths,
            previous_token: None,
        };
        let observed = ScanChangeSource
            .changes_with_provider(request(), provider)
            .map_err(|error| RepositoryError::Output(error.to_string()))?;
        let before: std::collections::BTreeMap<_, _> = observed
            .candidates
            .iter()
            .map(|candidate| (candidate.path.clone(), candidate.observation.clone()))
            .collect();
        let filter = crate::content_filter::GitAttributesFilter::for_repository(&self.root);
        // Start cold deliberately: a pre-import V2 row may describe a different
        // canonical state even when the current disk metadata is unchanged.
        let verified = verify_candidates_with_provider(
            observed,
            &tracked,
            &Default::default(),
            policy,
            now_timestamp(),
            &filter,
            provider,
            |path| {
                expected.get(path).copied().ok_or_else(|| {
                    crate::change_source::ChangeSourceError::CanonicalContent {
                        path: path.escaped(),
                        message: "path absent from verified import manifest".into(),
                    }
                })
            },
        )
        .map_err(|error| RepositoryError::Output(error.to_string()))?;
        let after = ScanChangeSource
            .changes_with_provider(request(), provider)
            .map_err(|error| RepositoryError::Output(error.to_string()))?;
        let stable: std::collections::BTreeSet<_> = after
            .candidates
            .iter()
            .filter(|candidate| before.get(&candidate.path) == Some(&candidate.observation))
            .map(|candidate| candidate.path.clone())
            .collect();
        if conversion_policy_fingerprint(&self.root, &paths)
            .map_err(|error| RepositoryError::Output(error.to_string()))?
            != policy
        {
            return Err(invalid(
                "V2 import seed conversion policy changed during verification",
            ));
        }
        let now = now_timestamp();
        let updates: Vec<_> = verified
            .index_updates
            .into_iter()
            .filter(|(path, row)| stable.contains(path) && !row.is_racy(now))
            .collect();
        let seeded: std::collections::BTreeSet<_> =
            updates.iter().map(|(path, _)| path.as_bytes()).collect();
        let mut txn = self.pristine.write_txn().map_err(map_file_index_error)?;
        let current = txn
            .get_working_copy(working_copy)?
            .ok_or_else(|| invalid("V2 import seed working copy disappeared"))?;
        if current.desired_view != baseline.desired_view
            || current.desired_state != baseline.desired_state
        {
            return Err(invalid(
                "V2 import seed working copy changed during verification",
            ));
        }
        let view = ViewTxnT::get_view_by_id(&txn, baseline.desired_view)?
            .ok_or_else(|| invalid("V2 import seed view disappeared"))?;
        if view.state != baseline.desired_state
            || effective_projection_identity(&txn, &view)?.set_id != project.manifest.set_id
        {
            return Err(invalid(
                "V2 import seed projection changed during verification",
            ));
        }
        let mut deletions: std::collections::BTreeSet<Vec<u8>> = paths
            .iter()
            .filter(|path| !seeded.contains(path.as_bytes()))
            .map(|path| path.as_bytes().to_vec())
            .collect();
        // Retire stale rows, including paths no longer in the canonical view.
        for (path, _) in txn.iter_file_index_v2(working_copy)? {
            if !seeded.contains(path.as_slice()) {
                deletions.insert(path);
            }
        }
        let updates = updates
            .iter()
            .map(|(path, row)| {
                Ok((
                    FileIndexV2Key::new(working_copy, path.as_bytes().to_vec())?,
                    row.clone(),
                ))
            })
            .collect::<Result<Vec<_>, atomic_core::pristine::PristineError>>()?;
        let deletions = deletions
            .into_iter()
            .map(|path| FileIndexV2Key::new(working_copy, path))
            .collect::<Result<Vec<_>, _>>()?;
        txn.put_file_index_v2_batch(&updates)?;
        txn.del_file_index_v2_batch(&deletions)?;
        txn.commit()?;
        Ok(FileIndexV2SeedOutcome {
            seeded: updates.len(),
            uncached: paths.len() - updates.len(),
        })
    }

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

    fn seed_fixture(
        files: &[(&str, &[u8])],
    ) -> (
        tempfile::TempDir,
        Repository,
        ProjectTree,
        VerifiedProspectiveEquivalence,
    ) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        for (path, bytes) in files {
            std::fs::write(dir.path().join(path), bytes).unwrap();
            repo.add(wc, path, TrackingOptions::default()).unwrap();
        }
        repo.record(
            wc,
            ChangeHeader::new("seed canonical files"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
        let keys: Vec<_> = repo
            .iter_file_index_v2(wc)
            .unwrap()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        repo.delete_file_index_v2_batch(wc, &keys).unwrap();
        let project = repo
            .project_tree(
                repo.current_view(),
                &ConversionPolicy::new(atomic_core::operation::GitHashAlgorithm::Sha1),
            )
            .unwrap();
        let proof = verify_prospective_equivalence(&project, &project.git.root).unwrap();
        (dir, repo, project, proof)
    }

    #[test]
    fn verified_import_seed_populates_v2_before_first_status_including_private_paths() {
        let (_dir, repo, project, proof) = seed_fixture(&[
            ("clean.txt", b"canonical\n"),
            ("100%.txt", b"literal percent\n"),
            (".atomicignore", b".atomic\n.git\n"),
            ("empty", b""),
            ("binary", b"\0\xff\0"),
        ]);
        let wc = repo.require_working_copy_id().unwrap();
        assert!(repo.iter_file_index_v2(wc).unwrap().is_empty());
        let outcome = repo
            .seed_verified_import_file_index_v2(wc, &project, &proof)
            .unwrap();
        assert_eq!(
            outcome,
            FileIndexV2SeedOutcome {
                seeded: 5,
                uncached: 0
            }
        );
        assert_eq!(repo.iter_file_index_v2(wc).unwrap().len(), 5);
        let status = repo.status(wc, StatusOptions::default()).unwrap();
        assert!(status.is_clean());
        assert_eq!(status.change_source_metrics().unwrap().content_reads, 0);
        assert_eq!(status.change_source_metrics().unwrap().valid_lease_hits, 5);
    }

    #[test]
    fn verified_import_seed_never_blesses_dirty_missing_mode_or_type_changes() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, repo, project, proof) = seed_fixture(&[
            ("clean", b"canonical"),
            ("dirty", b"canonical"),
            ("missing", b"canonical"),
            ("mode", b"canonical"),
            ("kind", b"canonical"),
        ]);
        let wc = repo.require_working_copy_id().unwrap();
        repo.seed_verified_import_file_index_v2(wc, &project, &proof)
            .unwrap();
        std::fs::write(dir.path().join("dirty"), b"dirty now").unwrap();
        std::fs::remove_file(dir.path().join("missing")).unwrap();
        std::fs::set_permissions(
            dir.path().join("mode"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::remove_file(dir.path().join("kind")).unwrap();
        std::os::unix::fs::symlink("clean", dir.path().join("kind")).unwrap();
        let result = repo
            .seed_verified_import_file_index_v2(wc, &project, &proof)
            .unwrap();
        assert_eq!(
            result,
            FileIndexV2SeedOutcome {
                seeded: 1,
                uncached: 4
            }
        );
        let rows = repo.iter_file_index_v2(wc).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.as_bytes(), b"clean");
        let status = repo.status(wc, StatusOptions::default()).unwrap();
        for (path, expected) in [
            ("dirty", FileStatus::Modified),
            ("missing", FileStatus::Deleted),
            ("mode", FileStatus::PermissionsChanged),
            ("kind", FileStatus::TypeChanged),
        ] {
            assert_eq!(
                status.get(std::path::Path::new(path)).unwrap().status(),
                expected
            );
        }
    }

    struct MutatingSeedProvider {
        mutate: std::path::PathBuf,
        policy: bool,
        replace: bool,
        advance: Option<(
            std::sync::Arc<atomic_core::pristine::Pristine>,
            String,
            Hash,
        )>,
        done: std::sync::atomic::AtomicBool,
    }

    impl crate::change_source::FilesystemProvider for MutatingSeedProvider {
        fn observe(
            &self,
            root: &Path,
            path: &RepoPath,
            reason: crate::change_source::ChangeCandidateReason,
        ) -> Result<crate::change_source::ChangeCandidate, crate::change_source::ChangeSourceError>
        {
            crate::change_source::FilesystemProvider::observe(
                &crate::change_source::RealFilesystemProvider,
                root,
                path,
                reason,
            )
        }
        fn canonical_content_id(
            &self,
            file: &crate::change_source::ObservedFile,
            path: &RepoPath,
            filter: &dyn crate::ContentFilter,
        ) -> Result<crate::change_source::CanonicalContentId, crate::change_source::ChangeSourceError>
        {
            let result = crate::change_source::FilesystemProvider::canonical_content_id(
                &crate::change_source::RealFilesystemProvider,
                file,
                path,
                filter,
            )?;
            if !self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                if let Some((pristine, parent, hash)) = &self.advance {
                    let mut txn = pristine.write_txn().unwrap();
                    let id = txn.register_change(hash).unwrap();
                    txn.put_change_deps(id, &[]).unwrap();
                    let mut view = txn.get_view(parent).unwrap().unwrap();
                    txn.put_change(&mut view, id, hash).unwrap();
                    txn.update_view(&view).unwrap();
                    txn.commit().unwrap();
                } else if self.policy {
                    let mut bytes = std::fs::read(&self.mutate).unwrap();
                    bytes.extend_from_slice(b"\n# changed during seed\n");
                    std::fs::write(&self.mutate, bytes).unwrap();
                } else if self.replace {
                    let replacement = self.mutate.with_extension("replacement");
                    std::fs::write(&replacement, b"canonical").unwrap();
                    std::fs::rename(replacement, &self.mutate).unwrap();
                } else {
                    std::fs::write(&self.mutate, b"changed during seed").unwrap();
                }
            }
            Ok(result)
        }
    }

    #[test]
    fn verified_import_seed_detects_worktree_and_policy_mutation_during_hashing() {
        for (policy, replace) in [(false, false), (false, true), (true, false)] {
            let (dir, repo, project, proof) = seed_fixture(&[("clean", b"canonical")]);
            let wc = repo.require_working_copy_id().unwrap();
            let provider = MutatingSeedProvider {
                mutate: if policy {
                    repo.config_path()
                } else {
                    dir.path().join("clean")
                },
                policy,
                replace,
                advance: None,
                done: std::sync::atomic::AtomicBool::new(false),
            };
            let result = repo
                .seed_verified_import_file_index_v2_with_provider(wc, &project, &proof, &provider);
            if policy {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("conversion policy changed"));
            } else {
                assert_eq!(
                    result.unwrap(),
                    FileIndexV2SeedOutcome {
                        seeded: 0,
                        uncached: 1
                    }
                );
            }
            assert!(repo.iter_file_index_v2(wc).unwrap().is_empty());
        }
    }

    #[test]
    fn verified_import_seed_rejects_mismatched_proof_and_stale_graph_projection() {
        let (dir, repo, project, proof) = seed_fixture(&[("clean", b"canonical")]);
        let wc = repo.require_working_copy_id().unwrap();
        let mut other = project.clone();
        other.manifest.entries[0].repository_bytes = b"forged".to_vec().into();
        assert!(repo
            .seed_verified_import_file_index_v2(wc, &other, &proof)
            .is_err());
        assert!(repo.iter_file_index_v2(wc).unwrap().is_empty());
        std::fs::write(dir.path().join("clean"), b"new canonical").unwrap();
        repo.record(
            wc,
            ChangeHeader::new("advance"),
            RecordOptions::new()
                .with_all(true)
                .save_to_store(true)
                .apply_after_record(true),
        )
        .unwrap();
        let rows = repo.iter_file_index_v2(wc).unwrap();
        assert!(repo
            .seed_verified_import_file_index_v2(wc, &project, &proof)
            .unwrap_err()
            .to_string()
            .contains("stale"));
        assert_eq!(repo.iter_file_index_v2(wc).unwrap(), rows);
    }

    #[test]
    fn verified_import_seed_rejects_parent_change_with_unchanged_child_merkle() {
        use atomic_core::change::Change;
        let (_dir, mut repo, _, _) = seed_fixture(&[("clean", b"canonical")]);
        let wc = repo.require_working_copy_id().unwrap();
        let parent_name = repo.current_view().to_string();
        repo.create_draft_view("child", &parent_name).unwrap();
        repo.set_current_view(wc, "child").unwrap();
        let policy = ConversionPolicy::new(atomic_core::operation::GitHashAlgorithm::Sha1);
        let project = repo.project_tree("child", &policy).unwrap();
        let proof = verify_prospective_equivalence(&project, &project.git.root).unwrap();
        let before = repo.view_identity("child").unwrap();
        // A metadata-only ancestor change changes effective identity without
        // altering either the child's own Merkle or the physical file bytes.
        let hash = repo
            .save_change(&Change::empty(ChangeHeader::new("parent advance")))
            .unwrap();
        let provider = MutatingSeedProvider {
            mutate: repo.root().join("clean"),
            policy: false,
            replace: false,
            advance: Some((repo.pristine.clone(), parent_name, hash)),
            done: std::sync::atomic::AtomicBool::new(false),
        };
        let result =
            repo.seed_verified_import_file_index_v2_with_provider(wc, &project, &proof, &provider);
        let after = repo.view_identity("child").unwrap();
        assert_eq!(before.merkle, after.merkle);
        assert_ne!(before.set_id, after.set_id);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("projection changed during verification"));
        assert!(repo.iter_file_index_v2(wc).unwrap().is_empty());
    }

    #[test]
    fn verified_import_seed_leaves_future_mtime_uncached() {
        let (dir, repo, project, proof) = seed_fixture(&[("clean", b"canonical")]);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("clean"))
            .unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(120)),
        )
        .unwrap();
        let wc = repo.require_working_copy_id().unwrap();
        assert_eq!(
            repo.seed_verified_import_file_index_v2(wc, &project, &proof)
                .unwrap(),
            FileIndexV2SeedOutcome {
                seeded: 0,
                uncached: 1
            }
        );
        assert!(repo.iter_file_index_v2(wc).unwrap().is_empty());
    }

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
        let metrics = warm.change_source_metrics().unwrap();
        assert_eq!(
            metrics.content_reads, 0,
            "warm status must not re-read clean content when watchers fall back to a scan"
        );
        assert_eq!(metrics.valid_lease_hits, 1);
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

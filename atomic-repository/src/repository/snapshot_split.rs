//! Snapshot split: reassemble a snapshot into an exact index-selected
//! durable change plus a new-durable→worktree remainder (RFC §10.3.2).
//!
//! # RFC §19 Q2 — inseparable-operation commit policy: APPROVED allow-as-incomplete (owner, 2026-09-14)
//!
//! The owner deferred this decision on 2026-09-13 ("lets flag this to come
//! back to. i dont know how to handle it currently"). Until it is resolved:
//!
//! - The typed refusals below (`SplitSnapshotError::Refused`) are **split
//!   refusals, not a commit policy**. They are not, and must not be read as,
//!   the answer to RFC §19 Q2 ("reject the commit" vs. "allow it only as
//!   synthesized/incomplete for review").
//! - No managed capture of an inseparable operation is implemented, and no
//!   reject/allow default is installed anywhere in the capture path.
//! - Neither outcome may approximate a path-level split or claim exact
//!   coverage; a verified commit-time capture (`atomic_agent::turn::capture`)
//!   is evidence only and never classifies a turn as
//!   `ManagedGitCommitCaptured`.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use atomic_core::change::InodeKind;
use atomic_core::operation::{
    ActorRef, MetadataTarget, MetadataTransition, MetadataValue, OperationKind, RepoStateRef,
    ViewStateRef,
};
use thiserror::Error;

use super::*;

pub const INDEX_MANIFEST_VERSION: u32 = 1;

/// Minimal, versioned description of the exact repository state selected for the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexManifest {
    pub version: u32,
    pub entries: Vec<IndexManifestEntry>,
    pub index_message: String,
    pub remainder_message: String,
}

impl IndexManifest {
    pub fn new(entries: Vec<IndexManifestEntry>) -> Self {
        Self {
            version: INDEX_MANIFEST_VERSION,
            entries,
            index_message: "Record staged snapshot selection".to_string(),
            remainder_message: "Retain unstaged snapshot remainder".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexManifestEntry {
    pub path: String,
    pub source_path: Option<String>,
    pub state: IndexEntryState,
}

impl IndexManifestEntry {
    pub fn present(path: impl Into<String>, repository_bytes: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            source_path: None,
            state: IndexEntryState::Present {
                repository_bytes: Some(repository_bytes),
                mode: 0o644,
                kind: InodeKind::Regular,
            },
        }
    }

    pub fn delete(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            source_path: None,
            state: IndexEntryState::Deleted,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexEntryState {
    /// Present at `path`. `None` preserves the baseline repository bytes and changes only attrs/path.
    Present {
        repository_bytes: Option<Vec<u8>>,
        mode: u16,
        kind: InodeKind,
    },
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitSnapshotOutcome {
    pub baseline_view: String,
    pub snapshot_view: String,
    pub snapshot: Hash,
    pub index: Hash,
    pub remainder: Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotSplitRefusal {
    UnsupportedVersion { found: u32 },
    EmptySelection,
    InvalidPath { path: String },
    DuplicatePath { path: String },
    DuplicateRenameSource { path: String },
    MissingRenameSource { path: String },
    RenameTargetExists { path: String },
    FilterRoundTrip { path: String },
    UnsupportedUnit { path: String, reason: String },
    SnapshotChanged { snapshot: Hash },
    EmptyRemainder,
}

impl std::fmt::Display for SnapshotSplitRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion { found } => write!(
                formatter,
                "unsupported index manifest version {found}; expected {INDEX_MANIFEST_VERSION}"
            ),
            Self::EmptySelection => write!(formatter, "index manifest selects no changes"),
            Self::InvalidPath { path } => write!(formatter, "unsafe index path '{path}'"),
            Self::DuplicatePath { path } => write!(formatter, "duplicate index path '{path}'"),
            Self::DuplicateRenameSource { path } => {
                write!(
                    formatter,
                    "rename source '{path}' is selected more than once"
                )
            }
            Self::MissingRenameSource { path } => {
                write!(
                    formatter,
                    "rename source '{path}' does not exist in the baseline"
                )
            }
            Self::RenameTargetExists { path } => {
                write!(
                    formatter,
                    "rename target '{path}' already exists in the baseline"
                )
            }
            Self::FilterRoundTrip { path } => write!(
                formatter,
                "repository bytes for '{path}' do not survive the configured smudge/clean boundary"
            ),
            Self::UnsupportedUnit { path, reason } => {
                write!(formatter, "cannot safely separate '{path}': {reason}")
            }
            Self::SnapshotChanged { snapshot } => write!(
                formatter,
                "working copy no longer matches snapshot {}",
                snapshot.to_base32()
            ),
            Self::EmptyRemainder => write!(
                formatter,
                "index selection consumes the complete snapshot; use snapshot promotion"
            ),
        }
    }
}

#[derive(Debug, Error)]
pub enum SplitSnapshotError {
    #[error("snapshot split refused: {0}")]
    Refused(SnapshotSplitRefusal),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Record(#[from] RecordError),
}

struct ScratchRepository {
    root: PathBuf,
    repository: Repository,
    working_copy: WorkingCopyId,
}

impl Drop for ScratchRepository {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Repository {
    /// Split the current working-copy snapshot into staged durable state and a durable remainder.
    pub fn split_snapshot(
        &self,
        working_copy: WorkingCopyId,
        manifest: IndexManifest,
    ) -> Result<SplitSnapshotOutcome, SplitSnapshotError> {
        self.validate_index_manifest(&manifest)?;
        let state = self.ensure_snapshot_view(working_copy)?;
        let operation_lock = self.try_lock_operation(working_copy)?;
        if let OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: atomic_core::operation::OperationScope::WorkingCopy(working_copy)
                    .to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            }
            .into());
        }
        let snapshot = state.head.ok_or_else(|| {
            SplitSnapshotError::Refused(SnapshotSplitRefusal::UnsupportedUnit {
                path: state.view.clone(),
                reason: "the private view does not contain a snapshot".to_string(),
            })
        })?;
        self.verify_snapshot_matches_worktree(working_copy, snapshot)?;

        let scratch = self.prepare_split_scratch(&state.baseline_view)?;
        apply_index_manifest(&scratch.root, &manifest)?;
        refresh_scratch_git_index(&self.root, &scratch.root)?;
        let index_outcome = scratch
            .repository
            .record(
                scratch.working_copy,
                ChangeHeader::new(&manifest.index_message),
                split_record_options(),
            )
            .map_err(map_scratch_record_error)?;

        replace_worktree(&self.root, &scratch.root)?;
        refresh_scratch_git_index(&self.root, &scratch.root)?;
        let remainder_outcome = scratch
            .repository
            .record(
                scratch.working_copy,
                ChangeHeader::new(&manifest.remainder_message),
                split_record_options(),
            )
            .map_err(|error| match error {
                RecordError::NothingToRecord => {
                    SplitSnapshotError::Refused(SnapshotSplitRefusal::EmptyRemainder)
                }
                other => map_scratch_record_error(other),
            })?;
        let remainder_outcome = require_dependency(
            &scratch.repository,
            remainder_outcome,
            *index_outcome.hash(),
        )?;

        ensure_no_snapshot_dependencies(index_outcome.change(), self)?;
        ensure_no_snapshot_dependencies(remainder_outcome.change(), &scratch.repository)?;
        self.publish_split(
            working_copy,
            &state.baseline_view,
            &state.view,
            snapshot,
            &scratch.repository,
            &index_outcome,
            &remainder_outcome,
            &operation_lock,
        )?;

        Ok(SplitSnapshotOutcome {
            baseline_view: state.baseline_view,
            snapshot_view: state.view,
            snapshot,
            index: *index_outcome.hash(),
            remainder: *remainder_outcome.hash(),
        })
    }

    fn validate_index_manifest(&self, manifest: &IndexManifest) -> Result<(), SplitSnapshotError> {
        if manifest.version != INDEX_MANIFEST_VERSION {
            return Err(SplitSnapshotError::Refused(
                SnapshotSplitRefusal::UnsupportedVersion {
                    found: manifest.version,
                },
            ));
        }
        if manifest.entries.is_empty() {
            return Err(SplitSnapshotError::Refused(
                SnapshotSplitRefusal::EmptySelection,
            ));
        }
        let mut targets = HashSet::new();
        let mut sources = HashSet::new();
        for entry in &manifest.entries {
            validate_relative_path(&entry.path)?;
            if !targets.insert(entry.path.clone()) {
                return Err(SplitSnapshotError::Refused(
                    SnapshotSplitRefusal::DuplicatePath {
                        path: entry.path.clone(),
                    },
                ));
            }
            if let Some(source) = &entry.source_path {
                validate_relative_path(source)?;
                if !sources.insert(source.clone()) {
                    return Err(SplitSnapshotError::Refused(
                        SnapshotSplitRefusal::DuplicateRenameSource {
                            path: source.clone(),
                        },
                    ));
                }
                if matches!(entry.state, IndexEntryState::Deleted) {
                    return Err(SplitSnapshotError::Refused(
                        SnapshotSplitRefusal::UnsupportedUnit {
                            path: entry.path.clone(),
                            reason: "a deletion cannot also declare a rename source".to_string(),
                        },
                    ));
                }
            }
        }
        Ok(())
    }

    fn verify_snapshot_matches_worktree(
        &self,
        working_copy: WorkingCopyId,
        snapshot: Hash,
    ) -> Result<(), SplitSnapshotError> {
        let root =
            std::env::temp_dir().join(format!("atomic-snapshot-verify-{}", WorkingCopyId::new()));
        let result = (|| -> Result<(), SplitSnapshotError> {
            self.materialize_view_to(&Self::snapshot_view_name(working_copy), &root)?;
            if repository_worktrees_match(&root, &self.root)? {
                Ok(())
            } else {
                Err(SplitSnapshotError::Refused(
                    SnapshotSplitRefusal::SnapshotChanged { snapshot },
                ))
            }
        })();
        let _ = std::fs::remove_dir_all(root);
        result
    }

    /// Whether the working copy's active snapshot already covers the current
    /// worktree content (CB-7B evidence freshness check).
    pub(super) fn snapshot_covers_worktree(
        &self,
        working_copy: WorkingCopyId,
        snapshot: Hash,
    ) -> bool {
        self.verify_snapshot_matches_worktree(working_copy, snapshot).is_ok()
    }

    fn prepare_split_scratch(
        &self,
        baseline_view: &str,
    ) -> Result<ScratchRepository, SplitSnapshotError> {
        let root =
            std::env::temp_dir().join(format!("atomic-snapshot-split-{}", WorkingCopyId::new()));
        let scratch_dot = root.join(DOT_DIR);
        std::fs::create_dir_all(scratch_dot.join("changes")).map_err(RepositoryError::Io)?;
        std::fs::create_dir_all(scratch_dot.join(WORKSPACES_DIR)).map_err(RepositoryError::Io)?;
        std::fs::copy(
            self.dot_dir.join("pristine.redb"),
            scratch_dot.join("pristine.redb"),
        )
        .map_err(RepositoryError::Io)?;
        copy_optional_file(
            &self.dot_dir.join("config.toml"),
            &scratch_dot.join("config.toml"),
        )?;
        std::fs::write(scratch_dot.join("current_view"), baseline_view)
            .map_err(RepositoryError::Io)?;
        copy_change_store_tree(
            self.change_store().changes_dir(),
            &scratch_dot.join("changes"),
        )?;

        let repository = Repository::open(&root)?;
        let working_copy = repository.require_working_copy_id()?;
        if repository.current_view() != baseline_view {
            return Err(SplitSnapshotError::Refused(
                SnapshotSplitRefusal::UnsupportedUnit {
                    path: baseline_view.to_string(),
                    reason: format!(
                        "scratch repository opened on '{}' instead of the durable baseline",
                        repository.current_view()
                    ),
                },
            ));
        }
        repository.materialize(working_copy)?;
        refresh_scratch_git_index(&self.root, &root)?;

        Ok(ScratchRepository {
            root,
            repository,
            working_copy,
        })
    }

    fn publish_split(
        &self,
        working_copy: WorkingCopyId,
        baseline_view: &str,
        snapshot_view: &str,
        snapshot: Hash,
        scratch: &Repository,
        index: &RecordOutcome,
        remainder: &RecordOutcome,
        operation_lock: &super::locks::WorkingCopyOperationLockGuard,
    ) -> Result<(), SplitSnapshotError> {
        let (before_state, after_state, transitions) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let baseline = txn
                .get_view(baseline_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: baseline_view.to_string(),
                })?;
            let private = txn
                .get_view(snapshot_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: snapshot_view.to_string(),
                })?;
            let snapshot_id = txn
                .get_internal(&snapshot)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: snapshot.to_base32(),
                })?;
            let snapshot_sequence = txn
                .get_change_seq(&private, snapshot_id)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ChangeNotInView {
                    hash: snapshot.to_base32(),
                    view: snapshot_view.to_string(),
                })?;
            let before_record = txn
                .get_working_copy(working_copy)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?;
            let after_baseline_state = baseline.state.next(index.hash());
            let mut after_record = before_record.clone();
            after_record.desired_state = after_baseline_state;
            (
                RepoStateRef {
                    view: Some(ViewStateRef {
                        name: baseline_view.to_string(),
                        state: baseline.state,
                        set_id: None,
                    }),
                    working_copy: Some(super::operation::working_copy_state_ref(before_record)),
                    git: None,
                },
                RepoStateRef {
                    view: Some(ViewStateRef {
                        name: baseline_view.to_string(),
                        state: after_baseline_state,
                        set_id: None,
                    }),
                    working_copy: Some(super::operation::working_copy_state_ref(after_record)),
                    git: None,
                },
                vec![
                    MetadataTransition {
                        target: MetadataTarget::ViewChange {
                            view: snapshot_view.to_string(),
                            change: snapshot,
                        },
                        expected_old: MetadataValue::Sequence(snapshot_sequence),
                        expected_new: MetadataValue::Absent,
                    },
                    MetadataTransition {
                        target: MetadataTarget::ViewChange {
                            view: baseline_view.to_string(),
                            change: *index.hash(),
                        },
                        expected_old: MetadataValue::Absent,
                        expected_new: MetadataValue::Sequence(baseline.change_count),
                    },
                    MetadataTransition {
                        target: MetadataTarget::ViewChange {
                            view: snapshot_view.to_string(),
                            change: *remainder.hash(),
                        },
                        expected_old: MetadataValue::Absent,
                        expected_new: MetadataValue::Sequence(0),
                    },
                ],
            )
        };

        let operation = self.prepare_metadata_operation(
            operation_lock,
            OperationKind::Record,
            None,
            before_state,
            after_state,
            transitions,
            vec![snapshot, *index.hash(), *remainder.hash()],
            ActorRef::System {
                name: "repository-split-snapshot".to_string(),
            },
            super::operation::current_operation_timestamp_ms(),
        )?;

        let publish_result = (|| -> Result<(), SplitSnapshotError> {
            copy_change_object(scratch, self, index.hash())?;
            copy_change_object(scratch, self, remainder.hash())?;
            self.write_split_recorded(index, remainder, baseline_view, snapshot_view, snapshot)?;
            self.refresh_working_copy_desired_state(working_copy)?;
            self.finalize_operation_verified(operation_lock, operation.id())?;
            Ok(())
        })();
        if publish_result.is_err() {
            let _ = self.abort_prepared_metadata_operation(operation_lock, &operation);
        }
        publish_result
    }
}

fn split_record_options() -> RecordOptions {
    RecordOptions::new()
        .with_all(true)
        .include_untracked(true)
        .save_to_store(true)
        .apply_after_record(true)
        .sync_vault(false)
        .enrich_kg(false)
}

fn map_scratch_record_error(error: RecordError) -> SplitSnapshotError {
    match error {
        RecordError::NothingToRecord => {
            SplitSnapshotError::Refused(SnapshotSplitRefusal::EmptySelection)
        }
        other => SplitSnapshotError::Refused(SnapshotSplitRefusal::UnsupportedUnit {
            path: "index".to_string(),
            reason: other.to_string(),
        }),
    }
}

fn validate_relative_path(path: &str) -> Result<(), SplitSnapshotError> {
    let candidate = Path::new(path);
    if path.is_empty()
        || candidate.is_absolute()
        || candidate.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
        || matches!(candidate.components().next(), Some(Component::Normal(first)) if first == ".atomic" || first == ".git")
    {
        return Err(SplitSnapshotError::Refused(
            SnapshotSplitRefusal::InvalidPath {
                path: path.to_string(),
            },
        ));
    }
    Ok(())
}

fn copy_optional_file(source: &Path, destination: &Path) -> Result<(), SplitSnapshotError> {
    if source.is_file() {
        std::fs::copy(source, destination).map_err(RepositoryError::Io)?;
    }
    Ok(())
}

fn copy_change_store_tree(source: &Path, destination: &Path) -> Result<(), SplitSnapshotError> {
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry =
            entry.map_err(|error| RepositoryError::Io(std::io::Error::other(error.to_string())))?;
        let relative = entry.path().strip_prefix(source).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: error.to_string(),
            }
        })?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).map_err(RepositoryError::Io)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(RepositoryError::Io)?;
            }
            if std::fs::hard_link(entry.path(), &target).is_err() {
                std::fs::copy(entry.path(), &target).map_err(RepositoryError::Io)?;
            }
        }
    }
    Ok(())
}

fn copy_change_object(
    source: &Repository,
    destination: &Repository,
    hash: &Hash,
) -> Result<(), RepositoryError> {
    let source_path = source.change_store().change_path(hash);
    let destination_path = destination.change_store().change_path(hash);
    if destination_path.is_file() {
        return Ok(());
    }
    if let Some(parent) = destination_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source_path, destination_path)?;
    Ok(())
}

fn require_dependency(
    repository: &Repository,
    outcome: RecordOutcome,
    dependency: Hash,
) -> Result<RecordOutcome, SplitSnapshotError> {
    if outcome.change().dependencies().contains(&dependency) {
        return Ok(outcome);
    }
    let mut change = outcome.change().clone();
    change.hashed.dependencies.push(dependency);
    change.hashed.dependencies.sort();
    change.hashed.dependencies.dedup();
    let mut bytes = Vec::new();
    let hash = change
        .serialize(&mut bytes)
        .map_err(|error| RecordError::ChangeStore(error.to_string()))?;
    let (change, verified) = Change::deserialize(&mut bytes.as_slice())
        .map_err(|error| RecordError::ChangeStore(error.to_string()))?;
    if hash != verified {
        return Err(SplitSnapshotError::Refused(
            SnapshotSplitRefusal::UnsupportedUnit {
                path: dependency.to_base32(),
                reason: "remainder dependency reserialization changed its hash".to_string(),
            },
        ));
    }
    repository.save_change_bytes(&hash, &bytes, &change)?;
    let mut rebuilt = RecordOutcome::new(change, hash, outcome.stats().clone());
    for path in outcome.recorded_files() {
        rebuilt.add_recorded_file(path.clone());
    }
    for path in outcome.deleted_files() {
        rebuilt.add_deleted_file(path.clone());
    }
    rebuilt.set_v3_bytes(bytes);
    rebuilt.set_saved(true);
    Ok(rebuilt)
}

fn ensure_no_snapshot_dependencies(
    change: &Change,
    repository: &Repository,
) -> Result<(), SplitSnapshotError> {
    let mut pending = change.dependencies().to_vec();
    let mut seen = HashSet::new();
    while let Some(hash) = pending.pop() {
        if !seen.insert(hash) {
            continue;
        }
        let dependency = repository.load_change(&hash)?;
        if dependency.kind().is_snapshot() {
            return Err(SplitSnapshotError::Refused(
                SnapshotSplitRefusal::UnsupportedUnit {
                    path: hash.to_base32(),
                    reason: "output depends on a snapshot change".to_string(),
                },
            ));
        }
        pending.extend(dependency.dependencies().iter().copied());
    }
    Ok(())
}

fn apply_index_manifest(root: &Path, manifest: &IndexManifest) -> Result<(), SplitSnapshotError> {
    let mut entries = manifest.entries.clone();
    entries.sort_by_key(|entry| entry.path != ".gitattributes");
    for entry in &entries {
        let target = root.join(&entry.path);
        if let Some(source_path) = &entry.source_path {
            let source = root.join(source_path);
            if !source.exists() && std::fs::symlink_metadata(&source).is_err() {
                return Err(SplitSnapshotError::Refused(
                    SnapshotSplitRefusal::MissingRenameSource {
                        path: source_path.clone(),
                    },
                ));
            }
            if target != source && (target.exists() || std::fs::symlink_metadata(&target).is_ok()) {
                return Err(SplitSnapshotError::Refused(
                    SnapshotSplitRefusal::RenameTargetExists {
                        path: entry.path.clone(),
                    },
                ));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(RepositoryError::Io)?;
            }
            std::fs::rename(&source, &target).map_err(RepositoryError::Io)?;
        }

        match &entry.state {
            IndexEntryState::Deleted => remove_path(&target)?,
            IndexEntryState::Present {
                repository_bytes,
                mode,
                kind,
            } => {
                let repository_bytes = match repository_bytes {
                    Some(bytes) => bytes.clone(),
                    None => repository_bytes_for_existing(root, &entry.path)?,
                };
                write_repository_entry(root, &entry.path, &repository_bytes, *mode, *kind)?;
            }
        }
    }
    Ok(())
}

fn repository_bytes_for_existing(root: &Path, path: &str) -> Result<Vec<u8>, SplitSnapshotError> {
    use crate::content_filter::ContentFilter;
    let absolute = root.join(path);
    let raw = crate::content_filter::read_working_bytes(&absolute).map_err(RepositoryError::Io)?;
    let filter = crate::content_filter::GitAttributesFilter::for_repository(root);
    filter
        .clean(Path::new(path), &raw)
        .map(|filtered| filtered.bytes)
        .map_err(|error| {
            SplitSnapshotError::Refused(SnapshotSplitRefusal::UnsupportedUnit {
                path: path.to_string(),
                reason: error.to_string(),
            })
        })
}

fn write_repository_entry(
    root: &Path,
    path: &str,
    repository_bytes: &[u8],
    mode: u16,
    kind: InodeKind,
) -> Result<(), SplitSnapshotError> {
    use crate::content_filter::ContentFilter;
    let absolute = root.join(path);
    remove_path(&absolute)?;
    if let Some(parent) = absolute.parent() {
        std::fs::create_dir_all(parent).map_err(RepositoryError::Io)?;
    }

    match kind {
        InodeKind::Regular => {
            let filter = crate::content_filter::GitAttributesFilter::for_repository(root);
            let working = filter
                .smudge(Path::new(path), repository_bytes)
                .map_err(|error| {
                    SplitSnapshotError::Refused(SnapshotSplitRefusal::UnsupportedUnit {
                        path: path.to_string(),
                        reason: error.to_string(),
                    })
                })?;
            std::fs::write(&absolute, &working.bytes).map_err(RepositoryError::Io)?;
            let round_trip = filter
                .clean(Path::new(path), &working.bytes)
                .map_err(|error| {
                    SplitSnapshotError::Refused(SnapshotSplitRefusal::UnsupportedUnit {
                        path: path.to_string(),
                        reason: error.to_string(),
                    })
                })?;
            if round_trip.bytes != repository_bytes {
                return Err(SplitSnapshotError::Refused(
                    SnapshotSplitRefusal::FilterRoundTrip {
                        path: path.to_string(),
                    },
                ));
            }
        }
        InodeKind::Symlink => create_symlink(&absolute, repository_bytes)?,
        InodeKind::Gitlink => {
            if !repository_bytes.is_empty() {
                return Err(SplitSnapshotError::Refused(
                    SnapshotSplitRefusal::UnsupportedUnit {
                        path: path.to_string(),
                        reason: "gitlink repository bytes must be empty".to_string(),
                    },
                ));
            }
            std::fs::create_dir_all(&absolute).map_err(RepositoryError::Io)?;
        }
    }
    set_mode(&absolute, mode)?;
    Ok(())
}

fn repository_worktrees_match(
    expected_repository: &Path,
    actual_worktree: &Path,
) -> Result<bool, SplitSnapshotError> {
    use crate::content_filter::ContentFilter;
    let filter = crate::content_filter::GitAttributesFilter::for_repository(actual_worktree);
    for entry in walkdir::WalkDir::new(expected_repository).follow_links(false) {
        let entry =
            entry.map_err(|error| RepositoryError::Io(std::io::Error::other(error.to_string())))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(expected_repository)
            .map_err(|error| RepositoryError::InvalidOperation {
                message: error.to_string(),
            })?;
        let actual = actual_worktree.join(relative);
        let actual_bytes = match crate::content_filter::read_working_bytes(&actual) {
            Ok(bytes) => bytes,
            Err(_) => return Ok(false),
        };
        let filtered = filter.clean(relative, &actual_bytes).map_err(|error| {
            SplitSnapshotError::Refused(SnapshotSplitRefusal::UnsupportedUnit {
                path: relative.display().to_string(),
                reason: error.to_string(),
            })
        })?;
        if filtered.bytes != std::fs::read(entry.path()).map_err(RepositoryError::Io)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn replace_worktree(source: &Path, destination: &Path) -> Result<(), SplitSnapshotError> {
    for entry in std::fs::read_dir(destination).map_err(RepositoryError::Io)? {
        let entry = entry.map_err(RepositoryError::Io)?;
        if entry.file_name() != ".atomic" {
            remove_path(&entry.path())?;
        }
    }
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry =
            entry.map_err(|error| RepositoryError::Io(std::io::Error::other(error.to_string())))?;
        let relative = entry.path().strip_prefix(source).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: error.to_string(),
            }
        })?;
        if relative.as_os_str().is_empty()
            || matches!(relative.components().next(), Some(Component::Normal(first)) if first == ".atomic" || first == ".git")
        {
            continue;
        }
        let target = destination.join(relative);
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(RepositoryError::Io)?;
        if metadata.file_type().is_dir() {
            std::fs::create_dir_all(&target).map_err(RepositoryError::Io)?;
        } else if metadata.file_type().is_symlink() {
            let bytes = crate::content_filter::read_working_bytes(entry.path())
                .map_err(RepositoryError::Io)?;
            create_symlink(&target, &bytes)?;
        } else if metadata.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(RepositoryError::Io)?;
            }
            std::fs::copy(entry.path(), &target).map_err(RepositoryError::Io)?;
        } else {
            return Err(SplitSnapshotError::Refused(
                SnapshotSplitRefusal::UnsupportedUnit {
                    path: relative.display().to_string(),
                    reason: "unsupported filesystem entry kind".to_string(),
                },
            ));
        }
        #[cfg(unix)]
        if !metadata.file_type().is_symlink() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &target,
                std::fs::Permissions::from_mode(metadata.permissions().mode()),
            )
            .map_err(RepositoryError::Io)?;
        }
    }
    Ok(())
}

fn refresh_scratch_git_index(
    source_root: &Path,
    scratch_root: &Path,
) -> Result<(), SplitSnapshotError> {
    if !source_root.join(".git").exists() {
        return Ok(());
    }
    if !scratch_root.join(".git").exists() {
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(scratch_root)
            .status()
            .map_err(RepositoryError::Io)?;
        if !status.success() {
            return Err(SplitSnapshotError::Refused(
                SnapshotSplitRefusal::UnsupportedUnit {
                    path: ".git".to_string(),
                    reason:
                        "cannot initialize scratch Git metadata for opaque-content classification"
                            .to_string(),
                },
            ));
        }
    }
    let status = std::process::Command::new("git")
        .arg("add")
        .arg("-A")
        .current_dir(scratch_root)
        .status()
        .map_err(RepositoryError::Io)?;
    if !status.success() {
        return Err(SplitSnapshotError::Refused(
            SnapshotSplitRefusal::UnsupportedUnit {
                path: ".git".to_string(),
                reason: "cannot refresh scratch Git index".to_string(),
            },
        ));
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<(), SplitSnapshotError> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path).map_err(RepositoryError::Io)?;
    } else {
        std::fs::remove_file(path).map_err(RepositoryError::Io)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(path: &Path, bytes: &[u8]) -> Result<(), SplitSnapshotError> {
    use std::os::unix::ffi::OsStringExt;
    std::os::unix::fs::symlink(OsString::from_vec(bytes.to_vec()), path)
        .map_err(RepositoryError::Io)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_symlink(_path: &Path, _bytes: &[u8]) -> Result<(), SplitSnapshotError> {
    Err(SplitSnapshotError::Refused(
        SnapshotSplitRefusal::UnsupportedUnit {
            path: "symlink".to_string(),
            reason: "symlink staging is unsupported on this platform".to_string(),
        },
    ))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u16) -> Result<(), SplitSnapshotError> {
    use std::os::unix::fs::PermissionsExt;
    if !std::fs::symlink_metadata(path)
        .map_err(RepositoryError::Io)?
        .file_type()
        .is_symlink()
    {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode as u32))
            .map_err(RepositoryError::Io)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(path: &Path, mode: u16) -> Result<(), SplitSnapshotError> {
    let mut permissions = std::fs::metadata(path)
        .map_err(RepositoryError::Io)?
        .permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    std::fs::set_permissions(path, permissions).map_err(RepositoryError::Io)?;
    Ok(())
}

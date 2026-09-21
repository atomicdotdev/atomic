//! CB-11A five-layer staging observation.
//!
//! The five layers are distinct relations over the same path universe:
//!
//! 1. **durable** — Atomic graph tracking (`TREE`), projected as the repository
//!    manifest ([`Repository::project_tree`]).
//! 2. **manifest** — the durable projection root, carried as evidence so
//!    callers can prove layer 1/2 agreement without a second projection.
//! 3. **index** — the primary Git index (staged layer, baseline → index).
//! 4. **worktree** — the physical worktree (unstaged layer, index → worktree).
//! 5. **snapshot** — the pending baseline→worktree snapshot change. A snapshot
//!    is the *complete* patch, never staged content.
//!
//! The observation is strictly read-only. Index movement (`git add`,
//! `git reset`, `git rm --cached`, alternate `GIT_INDEX_FILE`) is mirrored into
//! [`StagingState`] only and never mutates durable `TREE`.
//!
//! Only the index manifest projects to Git stage 0; the durable manifest and
//! the snapshot are separate layers that are reported, not conflated.

use std::fs;
use std::path::Path;

use atomic_core::operation::{GitHashAlgorithm, GitObjectId};

use super::git_observation::{observe_git_index, observe_worktree, ObservationError};
use super::project_tree::{git_object_id, GitObjectKind};
use super::*;

/// Version of the staging-state observation format.
pub const STAGING_STATE_VERSION: u8 = 1;

/// Extended index flag: content is *not* staged; the entry records intent.
pub const INTENT_TO_ADD_FLAG: u16 = 0x2000;
/// Extended index flag: the worktree copy is not compared against the index.
pub const SKIP_WORKTREE_FLAG: u16 = 0x4000;
/// Classic index flag: the worktree copy is exempt from change detection.
pub const ASSUME_VALID_FLAG: u16 = 0x8000;

/// A single-column status code in the Git-inspired subset (RFC §9.2).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum StageCode {
    /// Unmodified in this layer comparison.
    Unmodified,
    /// Content or mode changed (Git short format reports mode changes as M).
    Modified,
    /// Path exists in the newer layer only.
    Added,
    /// Path exists in the older layer only.
    Deleted,
    /// Kind changed (regular ↔ symlink ↔ gitlink).
    TypeChanged,
    /// Unmerged: index holds stages 1-3 for the path.
    Unmerged,
    /// Untracked: absent from baseline and index, present in the worktree.
    Untracked,
}

impl StageCode {
    /// The single-character glyph used by `status --git` two-column output.
    pub fn glyph(self) -> char {
        match self {
            Self::Unmodified => ' ',
            Self::Modified => 'M',
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::TypeChanged => 'T',
            Self::Unmerged => 'U',
            Self::Untracked => '?',
        }
    }
}

/// One path's relation across the five layers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagingEntry {
    pub path: RepoPath,
    /// Baseline (Git HEAD tree) → index.
    pub x: StageCode,
    /// Index → worktree.
    pub y: StageCode,
    /// Baseline tree entry mode, when the path exists in the baseline.
    pub baseline_mode: Option<u32>,
    /// Stage-0 index entry mode, when the path exists in the index.
    pub index_mode: Option<u32>,
    /// Worktree unix mode, when the path exists physically.
    pub worktree_mode: Option<u32>,
    /// Whether the path is durably tracked by Atomic (`TREE`).
    pub durable_tracked: bool,
    pub intent_to_add: bool,
    pub skip_worktree: bool,
    pub assume_unchanged: bool,
    pub unmerged: bool,
}

/// Git-informed notices that decorate staging output without claiming status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StagingNotice {
    /// A Git sequence/merge/rebase operation is in progress.
    GitOperationInProgress { state: String },
    /// The index holds unmerged (stage 1-3) entries.
    UnmergedIndexEntries { paths: Vec<String> },
    /// Sparse-index directory entries are present and intentionally unexpanded.
    SparseIndexEntries { paths: Vec<String> },
    /// Entries flagged skip-worktree / assume-unchanged are exempt from
    /// worktree comparison.
    FlaggedEntries {
        skip_worktree: Vec<String>,
        assume_unchanged: Vec<String>,
    },
    /// Index entries marked intent-to-add: the content is *not* staged.
    IntentToAdd { paths: Vec<String> },
    /// The observed index is not the repository's default primary index.
    AlternateIndex { path: String },
    /// HEAD commit is a committed conflict snapshot (`atomic-conflict` header).
    CommittedConflictSnapshot { head: String },
}

/// Versioned, read-only mirror of the five staging/tracking layers.
#[derive(Clone, Debug)]
pub struct StagingState {
    pub version: u8,
    /// Git HEAD tree OID (the staged-layer baseline), when HEAD exists.
    pub baseline_tree: Option<GitObjectId>,
    /// Exact stage-0 index tree OID, when computable without writes.
    pub index_tree: Option<GitObjectId>,
    /// Durable repository-manifest root (layer 1/2 evidence). Absent for a
    /// Git-only observation where the durable layer was not evaluated.
    pub durable_manifest_root: Option<ManifestRoot>,
    /// Pending snapshot change hash, when one exists.
    pub snapshot: Option<atomic_core::Hash>,
    /// Pending durable remainder hash, when one exists.
    pub remainder: Option<atomic_core::Hash>,
    pub entries: Vec<StagingEntry>,
    pub notices: Vec<StagingNotice>,
}

impl StagingState {
    /// Entries whose staged (X) column is not Unmodified.
    pub fn staged(&self) -> impl Iterator<Item = &StagingEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.x != StageCode::Unmodified)
    }

    /// Entries whose unstaged (Y) column is not Unmodified.
    pub fn unstaged(&self) -> impl Iterator<Item = &StagingEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.y != StageCode::Unmodified)
    }

    /// Entries reported as untracked (`??`).
    pub fn untracked(&self) -> impl Iterator<Item = &StagingEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.x == StageCode::Untracked && entry.y == StageCode::Untracked)
    }

    /// True when no staged, unstaged, or untracked entry exists.
    pub fn is_clean(&self) -> bool {
        self.entries.iter().all(|entry| {
            entry.x == StageCode::Unmodified
                && (entry.y == StageCode::Unmodified
                    || entry.skip_worktree
                    || entry.assume_unchanged)
        })
    }
}

/// Baseline (HEAD tree) entry observed without writing the object database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BaselineEntry {
    pub path: RepoPath,
    pub mode: u32,
    pub oid: GitObjectId,
}

/// Read-only errors from the staging observation.
#[derive(Debug, thiserror::Error)]
pub enum StagingError {
    #[error("cannot observe Git index: {0}")]
    Index(#[from] ObservationError),
    #[error("cannot read Git HEAD tree: {0}")]
    HeadTree(String),
    #[error("repository error: {0}")]
    Repository(String),
}

struct RawLayer {
    baseline: Vec<BaselineEntry>,
    baseline_tree: Option<GitObjectId>,
    index: GitIndexState,
    worktree: WorktreeObservation,
    repository_state: Option<String>,
    head_commit_message: Option<String>,
    head_commit_oid: Option<String>,
    index_path: Option<std::path::PathBuf>,
    alternate_index_used: bool,
}

fn kind_of_mode(mode: u32) -> u8 {
    match mode & 0o170000 {
        0o120000 => b'l',
        0o160000 => b'g',
        _ => b'r',
    }
}

/// Observe the Git baseline (HEAD tree), repository state, and primary index
/// without mutation. `GIT_INDEX_FILE` selects an alternate primary index;
/// libgit2 itself ignores that variable, so it is resolved explicitly here.
fn observe_raw_layers(root: &Path, policy: &ConversionPolicy) -> Result<RawLayer, StagingError> {
    let repository = git2::Repository::discover(root).map_err(|error| {
        StagingError::HeadTree(format!(
            "cannot open Git repository at '{}': {error}",
            root.display()
        ))
    })?;

    let (head_commit_oid, head_commit_message, baseline, baseline_tree) = match repository.head() {
        Ok(head) => match head.peel_to_commit() {
            Ok(commit) => {
                let oid = commit.id().to_string();
                let message = commit.message().unwrap_or_default().to_string();
                let tree = commit.tree().map_err(|error| {
                    StagingError::HeadTree(format!("cannot read HEAD tree: {error}"))
                })?;
                let tree_oid = GitObjectId::new(
                    policy.object_format,
                    tree.id().as_bytes().to_vec(),
                )
                .map_err(|_| {
                    StagingError::HeadTree(format!(
                        "HEAD tree id does not match algorithm {:?}",
                        policy.object_format
                    ))
                })?;
                let mut baseline = Vec::new();
                collect_baseline_tree(
                    &repository,
                    &tree,
                    &policy.object_format,
                    &mut Vec::new(),
                    &mut baseline,
                )?;
                baseline.sort_by(|left, right| left.path.cmp(&right.path));
                (Some(oid), Some(message), baseline, Some(tree_oid))
            }
            Err(_) => (None, None, Vec::new(), None),
        },
        Err(_) => (None, None, Vec::new(), None),
    };

    let repository_state = Some(
        match repository.state() {
            git2::RepositoryState::Clean => "Clean",
            git2::RepositoryState::Merge => "Merge",
            git2::RepositoryState::Revert => "Revert",
            git2::RepositoryState::RevertSequence => "RevertSequence",
            git2::RepositoryState::CherryPick => "CherryPick",
            git2::RepositoryState::CherryPickSequence => "CherryPickSequence",
            git2::RepositoryState::Bisect => "Bisect",
            git2::RepositoryState::Rebase => "Rebase",
            git2::RepositoryState::RebaseInteractive => "RebaseInteractive",
            git2::RepositoryState::RebaseMerge => "RebaseMerge",
            git2::RepositoryState::ApplyMailbox => "ApplyMailbox",
            git2::RepositoryState::ApplyMailboxOrRebase => "ApplyMailboxOrRebase",
        }
        .to_string(),
    );

    let default_index_path = repository.path().join("index");
    let alternate_index_path = std::env::var_os("GIT_INDEX_FILE")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| fs::canonicalize(&path).unwrap_or(path));
    let alternate_index_used = alternate_index_path.is_some();
    let index_path = alternate_index_path.unwrap_or(default_index_path);
    let index = if alternate_index_used {
        super::git_observation::observe_git_index_path(root, &index_path, policy)?
    } else {
        observe_git_index(root, policy)?
    };
    let worktree = observe_worktree(root, Some(&index), &PassthroughFilter, policy)?;

    Ok(RawLayer {
        baseline,
        baseline_tree,
        index,
        worktree,
        repository_state,
        head_commit_message,
        head_commit_oid,
        index_path: Some(index_path),
        alternate_index_used,
    })
}

fn collect_baseline_tree(
    repository: &git2::Repository,
    tree: &git2::Tree<'_>,
    algorithm: &GitHashAlgorithm,
    prefix: &mut Vec<u8>,
    output: &mut Vec<BaselineEntry>,
) -> Result<(), StagingError> {
    for entry in tree.iter() {
        let name = entry.name_bytes();
        let mut path = prefix.clone();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(name);
        if entry.kind() == Some(git2::ObjectType::Tree) {
            let child = repository.find_tree(entry.id()).map_err(|error| {
                StagingError::HeadTree(format!("cannot read subtree {}: {error}", entry.id()))
            })?;
            collect_baseline_tree(repository, &child, algorithm, &mut path, output)?;
        } else {
            let oid = GitObjectId::new(*algorithm, entry.id().as_bytes().to_vec()).map_err(|_| {
                StagingError::HeadTree(format!(
                    "HEAD tree object id does not match algorithm {algorithm:?}"
                ))
            })?;
            output.push(BaselineEntry {
                path: RepoPath::from_bytes(&path)
                    .map_err(|error| StagingError::HeadTree(error.to_string()))?,
                mode: entry.filemode() as u32,
                oid,
            });
        }
    }
    Ok(())
}

/// Identity content filter: repository bytes equal working bytes.
struct PassthroughFilter;

impl crate::content_filter::ContentFilter for PassthroughFilter {
    fn clean(
        &self,
        _path: &Path,
        bytes: &[u8],
    ) -> Result<crate::content_filter::FilteredContent, crate::content_filter::ContentFilterError>
    {
        Ok(crate::content_filter::FilteredContent {
            bytes: bytes.to_vec(),
            warnings: Vec::new(),
        })
    }

    fn smudge(
        &self,
        _path: &Path,
        bytes: &[u8],
    ) -> Result<crate::content_filter::FilteredContent, crate::content_filter::ContentFilterError>
    {
        Ok(crate::content_filter::FilteredContent {
            bytes: bytes.to_vec(),
            warnings: Vec::new(),
        })
    }
}

/// Observe the five staging layers for one working copy.
///
/// Read-only: no Git index/object-database/ref writes, no durable `TREE`
/// mutation, no materialization.
pub fn observe_staging_state(
    repository: &Repository,
    working_copy: atomic_core::WorkingCopyId,
    policy: &ConversionPolicy,
) -> Result<StagingState, StagingError> {
    let root = repository.root().to_path_buf();
    let raw = observe_raw_layers(&root, policy)?;

    let view_name = repository
        .desired_view_name(working_copy)
        .map_err(|error| StagingError::Repository(error.to_string()))?;
    let project = repository
        .project_tree(&view_name, policy)
        .map_err(|error| StagingError::Repository(error.to_string()))?;
    let durable_manifest_root = Some(project.manifest.root());

    let snapshot_status = repository
        .snapshot_status(working_copy)
        .map_err(|error| StagingError::Repository(error.to_string()))?;

    let durable: std::collections::BTreeSet<Vec<u8>> = project
        .manifest
        .included_entries()
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();

    build_staging_state(
        raw,
        durable,
        durable_manifest_root,
        snapshot_status.snapshot,
        snapshot_status.remainder,
        policy,
    )
}

/// Observe the Git layers (baseline/index/worktree) only, without evaluating
/// the durable Atomic layer. Used when the workspace boundary returned a
/// remediation and command bodies must not run: the Git-informed picture is
/// still reportable, the durable relation is simply not claimed.
pub fn observe_git_staging_state(
    root: &Path,
    policy: &ConversionPolicy,
) -> Result<StagingState, StagingError> {
    let raw = observe_raw_layers(root, policy)?;
    build_staging_state(raw, std::collections::BTreeSet::new(), None, None, None, policy)
}

#[allow(clippy::too_many_arguments)]
fn build_staging_state(
    raw: RawLayer,
    durable: std::collections::BTreeSet<Vec<u8>>,
    durable_manifest_root: Option<ManifestRoot>,
    snapshot: Option<atomic_core::Hash>,
    remainder: Option<atomic_core::Hash>,
    policy: &ConversionPolicy,
) -> Result<StagingState, StagingError> {
    let index_zero: std::collections::BTreeMap<Vec<u8>, &GitIndexEntry> = raw
        .index
        .entries
        .iter()
        .filter(|entry| entry.stage == 0)
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect();
    let unmerged_paths: std::collections::BTreeSet<Vec<u8>> = raw
        .index
        .entries
        .iter()
        .filter(|entry| entry.stage != 0)
        .map(|entry| entry.path.as_bytes().to_vec())
        .collect();

    let worktree: std::collections::BTreeMap<Vec<u8>, &WorktreeEntry> = raw
        .worktree
        .entries
        .iter()
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect();

    let baseline_by_path: std::collections::BTreeMap<Vec<u8>, &BaselineEntry> = raw
        .baseline
        .iter()
        .map(|entry| (entry.path.as_bytes().to_vec(), entry))
        .collect();

    let mut entries: Vec<StagingEntry> = Vec::new();
    let mut sparse_paths = Vec::new();
    let mut intent_paths = Vec::new();
    let mut skip_worktree_paths = Vec::new();
    let mut assume_unchanged_paths = Vec::new();
    let mut covered: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();
    let mut staged_deleted: std::collections::BTreeSet<Vec<u8>> = std::collections::BTreeSet::new();

    // Stage-0 index entries drive the primary classification. Sparse-index
    // directory entries are directory manifests, not file entries: they are
    // never expanded here and a missing worktree path is NOT a deletion.
    for (path, index_entry) in &index_zero {
        if index_entry.sparse_directory {
            sparse_paths.push(String::from_utf8_lossy(path).into_owned());
            continue;
        }
        covered.insert(path.clone());
        let baseline_entry = baseline_by_path.get(path).copied();
        let worktree_entry = worktree.get(path).copied();

        let x = if index_entry.intent_to_add {
            // Intent-to-add records intent only; no content is staged.
            StageCode::Unmodified
        } else {
            match baseline_entry {
                None => StageCode::Added,
                Some(baseline) => {
                    let content_changed = index_entry
                        .oid
                        .as_ref()
                        .map(|oid| oid != &baseline.oid)
                        .unwrap_or(true);
                    let kind_changed =
                        kind_of_mode(index_entry.mode) != kind_of_mode(baseline.mode);
                    if kind_changed {
                        StageCode::TypeChanged
                    } else if content_changed || index_entry.mode != baseline.mode {
                        StageCode::Modified
                    } else {
                        StageCode::Unmodified
                    }
                }
            }
        };

        let y = if unmerged_paths.contains(path) {
            StageCode::Unmerged
        } else if index_entry.skip_worktree || index_entry.assume_unchanged {
            // Flagged entries are exempt from worktree comparison.
            StageCode::Unmodified
        } else {
            match worktree_entry {
                None => StageCode::Deleted,
                Some(worktree) => {
                    if index_entry.intent_to_add {
                        StageCode::Added
                    } else {
                        compare_index_to_worktree(index_entry, worktree, policy)
                    }
                }
            }
        };

        if index_entry.intent_to_add {
            intent_paths.push(String::from_utf8_lossy(path).into_owned());
        }
        if index_entry.skip_worktree {
            skip_worktree_paths.push(String::from_utf8_lossy(path).into_owned());
        }
        if index_entry.assume_unchanged {
            assume_unchanged_paths.push(String::from_utf8_lossy(path).into_owned());
        }

        entries.push(StagingEntry {
            path: index_entry.path.clone(),
            x,
            y,
            baseline_mode: baseline_entry.map(|entry| entry.mode),
            index_mode: Some(index_entry.mode),
            worktree_mode: worktree_entry.and_then(|entry| entry.mode).map(u32::from),
            durable_tracked: durable.contains(path),
            intent_to_add: index_entry.intent_to_add,
            skip_worktree: index_entry.skip_worktree,
            assume_unchanged: index_entry.assume_unchanged,
            unmerged: false,
        });
    }

    // Unmerged paths (stages 1-3 present).
    for path in &unmerged_paths {
        if let Some(entry) = entries
            .iter_mut()
            .find(|entry| entry.path.as_bytes() == path.as_slice())
        {
            entry.x = StageCode::Unmerged;
            entry.y = StageCode::Unmerged;
            entry.unmerged = true;
            continue;
        }
        covered.insert(path.clone());
        let baseline_entry = baseline_by_path.get(path).copied();
        entries.push(StagingEntry {
            path: RepoPath::from_bytes(path)
                .map_err(|error| StagingError::Repository(error.to_string()))?,
            x: StageCode::Unmerged,
            y: StageCode::Unmerged,
            baseline_mode: baseline_entry.map(|entry| entry.mode),
            index_mode: None,
            worktree_mode: worktree.get(path).and_then(|entry| entry.mode).map(u32::from),
            durable_tracked: durable.contains(path),
            intent_to_add: false,
            skip_worktree: false,
            assume_unchanged: false,
            unmerged: true,
        });
    }

    // Baseline paths deleted from the index (staged deletions). When the
    // worktree still holds the path it is additionally reported untracked.
    for (path, baseline_entry) in &baseline_by_path {
        if covered.contains(path) || index_zero.contains_key(path) {
            continue;
        }
        covered.insert(path.clone());
        staged_deleted.insert(path.clone());
        let worktree_entry = worktree.get(path).copied();
        entries.push(StagingEntry {
            path: baseline_entry.path.clone(),
            x: StageCode::Deleted,
            y: StageCode::Unmodified,
            baseline_mode: Some(baseline_entry.mode),
            index_mode: None,
            worktree_mode: worktree_entry.and_then(|entry| entry.mode).map(u32::from),
            durable_tracked: durable.contains(path),
            intent_to_add: false,
            skip_worktree: false,
            assume_unchanged: false,
            unmerged: false,
        });
    }

    // Worktree-only paths are untracked, unless the policy excludes them.
    for (path, worktree_entry) in &worktree {
        if covered.contains(path) {
            if staged_deleted.contains(path) {
                // Staged deletion with a recreated worktree file: the file is
                // reported as untracked in addition to the staged deletion.
                entries.push(StagingEntry {
                    path: worktree_entry.path.clone(),
                    x: StageCode::Untracked,
                    y: StageCode::Untracked,
                    baseline_mode: None,
                    index_mode: None,
                    worktree_mode: worktree_entry.mode.map(u32::from),
                    durable_tracked: durable.contains(path),
                    intent_to_add: false,
                    skip_worktree: false,
                    assume_unchanged: false,
                    unmerged: false,
                });
            }
            continue;
        }
        if matches!(
            worktree_entry.disposition,
            ManifestDisposition::Excluded(_)
        ) {
            continue;
        }
        covered.insert(path.clone());
        entries.push(StagingEntry {
            path: worktree_entry.path.clone(),
            x: StageCode::Untracked,
            y: StageCode::Untracked,
            baseline_mode: None,
            index_mode: None,
            worktree_mode: worktree_entry.mode.map(u32::from),
            durable_tracked: durable.contains(path),
            intent_to_add: false,
            skip_worktree: false,
            assume_unchanged: false,
            unmerged: false,
        });
    }

    // Deterministic order: by path; a staged deletion sorts before the
    // untracked recreation of the same path (stable sort preserves push order).
    entries.sort_by(|left, right| left.path.cmp(&right.path));

    let mut notices = Vec::new();
    if let Some(state) = &raw.repository_state {
        if state != "Clean" {
            notices.push(StagingNotice::GitOperationInProgress {
                state: state.clone(),
            });
        }
    }
    if !unmerged_paths.is_empty() {
        notices.push(StagingNotice::UnmergedIndexEntries {
            paths: unmerged_paths
                .iter()
                .map(|path| String::from_utf8_lossy(path).into_owned())
                .collect(),
        });
    }
    if !sparse_paths.is_empty() {
        notices.push(StagingNotice::SparseIndexEntries { paths: sparse_paths });
    } else if raw.index.sparse_index {
        // The index used sparse-directory entries that were observed through
        // read-only in-memory expansion; the covered paths are exactly the
        // skip-worktree entries the expansion produced. Sparse absence is
        // never a deletion.
        notices.push(StagingNotice::SparseIndexEntries {
            paths: skip_worktree_paths.clone(),
        });
    }
    if !skip_worktree_paths.is_empty() || !assume_unchanged_paths.is_empty() {
        notices.push(StagingNotice::FlaggedEntries {
            skip_worktree: skip_worktree_paths,
            assume_unchanged: assume_unchanged_paths,
        });
    }
    if !intent_paths.is_empty() {
        notices.push(StagingNotice::IntentToAdd { paths: intent_paths });
    }
    if raw.alternate_index_used {
        if let Some(index_path) = &raw.index_path {
            notices.push(StagingNotice::AlternateIndex {
                path: index_path.display().to_string(),
            });
        }
    }
    if let (Some(message), Some(oid)) = (&raw.head_commit_message, &raw.head_commit_oid) {
        if message.contains("atomic-conflict") {
            notices.push(StagingNotice::CommittedConflictSnapshot {
                head: oid.clone(),
            });
        }
    }

    Ok(StagingState {
        version: STAGING_STATE_VERSION,
        baseline_tree: raw.baseline_tree,
        index_tree: raw.index.tree,
        durable_manifest_root,
        snapshot,
        remainder,
        entries,
        notices,
    })
}

/// Compare one stage-0 index entry against its worktree observation.
fn compare_index_to_worktree(
    index_entry: &GitIndexEntry,
    worktree: &WorktreeEntry,
    policy: &ConversionPolicy,
) -> StageCode {
    let index_kind = kind_of_mode(index_entry.mode);
    let worktree_kind = match worktree.physical_kind {
        PhysicalKind::Symlink => b'l',
        PhysicalKind::Directory => b'g',
        PhysicalKind::Regular => b'r',
        PhysicalKind::Other => b'o',
    };
    if index_kind != worktree_kind {
        return StageCode::TypeChanged;
    }

    // Content: compare the index blob OID with the cleaned repository content.
    let content_changed = match &worktree.repository_bytes_after_clean {
        Some(bytes) => {
            let worktree_oid = git_object_id(policy.object_format, GitObjectKind::Blob, bytes)
                .ok()
                .map(|oid| oid.as_bytes().to_vec());
            let index_oid = index_entry.oid.as_ref().map(|oid| oid.as_bytes().to_vec());
            worktree_oid != index_oid
        }
        None => true,
    };
    if content_changed {
        return StageCode::Modified;
    }

    if policy.platform.executable_bit && index_kind == b'r' {
        let worktree_exec = worktree.mode.map(|mode| mode & 0o100 != 0).unwrap_or(false);
        let index_exec = index_entry.mode & 0o111 != 0;
        if worktree_exec != index_exec {
            return StageCode::Modified;
        }
    }

    StageCode::Unmodified
}

/// Lowercase hex rendering of a Git object identity (for status output).
pub fn git_object_id_hex(oid: &GitObjectId) -> String {
    super::git_observation::git_object_id_hex(oid)
}

/// Git-compatible reversible quoting for a raw path.
///
/// Paths containing special bytes are wrapped in double quotes with C-style
/// escapes so the displayed path can always be mapped back to raw bytes.
pub fn quote_path(path: &[u8]) -> String {
    let needs_quotes = path
        .iter()
        .any(|byte| !matches!(byte, b' '..=b'~') || *byte == b'"' || *byte == b'\\');
    if !needs_quotes {
        return String::from_utf8_lossy(path).into_owned();
    }
    let mut quoted = String::with_capacity(path.len() + 2);
    quoted.push('"');
    for byte in path {
        match *byte {
            b'\\' => quoted.push_str("\\\\"),
            b'"' => quoted.push_str("\\\""),
            b'\n' => quoted.push_str("\\n"),
            b'\t' => quoted.push_str("\\t"),
            b'\r' => quoted.push_str("\\r"),
            0x07 => quoted.push_str("\\a"),
            0x08 => quoted.push_str("\\b"),
            0x0b => quoted.push_str("\\v"),
            0x0c => quoted.push_str("\\f"),
            b' '..=b'~' => quoted.push(*byte as char),
            other => quoted.push_str(&format!("\\{:03o}", other)),
        }
    }
    quoted.push('"');
    quoted
}

/// Format one entry as a `git status --short`-comparable two-column line.
pub fn format_two_column(entry: &StagingEntry) -> String {
    format!(
        "{}{} {}",
        entry.x.glyph(),
        entry.y.glyph(),
        quote_path(entry.path.as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_code_glyphs_match_git_subset() {
        assert_eq!(StageCode::Unmodified.glyph(), ' ');
        assert_eq!(StageCode::Modified.glyph(), 'M');
        assert_eq!(StageCode::Added.glyph(), 'A');
        assert_eq!(StageCode::Deleted.glyph(), 'D');
        assert_eq!(StageCode::TypeChanged.glyph(), 'T');
        assert_eq!(StageCode::Unmerged.glyph(), 'U');
        assert_eq!(StageCode::Untracked.glyph(), '?');
    }

    #[test]
    fn quote_path_matches_git_c_quoting_for_special_bytes() {
        assert_eq!(quote_path(b"plain.txt"), "plain.txt");
        assert_eq!(quote_path(b"with space.txt"), "with space.txt");
        assert_eq!(quote_path(b"with\"quote"), "\"with\\\"quote\"");
        assert_eq!(quote_path(b"back\\slash"), "\"back\\\\slash\"");
        assert_eq!(quote_path(&[0x41, 0x00, 0x42]), "\"A\\000B\"");
        assert_eq!(quote_path(&[0xe2, 0x82, 0xac]), "\"\\342\\202\\254\"");
    }
}

//! Strictly read-only observation of a colocated Git repository.
//!
//! This module deliberately avoids every libgit2 API that writes the index,
//! object database, refs, configuration, or working tree. The exact index tree
//! OID is computed by encoding tree objects in memory and hashing those bytes;
//! the independent canonical index digest remains explicitly non-Git metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use git2::{ErrorCode, ObjectType, Oid, Repository as GitRepository, RepositoryState};

use super::is_wip_ref;

const INDEX_DIGEST_DOMAIN: &[u8] = b"atomic:git-index-observation:v1\0";
const REFS_DIGEST_DOMAIN: &[u8] = b"atomic:git-refs-observation:v1\0";

/// A complete read-only Git observation, including the explicit no-Git case.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitObservation {
    /// The inspected Atomic root is not a Git repository.
    NoGit { root: PathBuf },
    /// Git administrative and repository state observed without mutation.
    Repository(Box<GitRepositoryObservation>),
}

/// Read-only state of one Git repository/worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRepositoryObservation {
    pub paths: GitAdminPaths,
    pub head: HeadObservation,
    pub head_tree_oid: Option<Oid>,
    pub index: IndexObservation,
    pub locks: LockObservation,
    pub operation: OperationObservation,
    pub refs: Vec<RefObservation>,
    pub refs_digest: CanonicalRefsDigest,
}

/// Administrative paths resolved through libgit2 rather than `.git/` assumptions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitAdminPaths {
    /// The physical working tree, absent for a bare repository.
    pub worktree_root: Option<PathBuf>,
    /// Per-worktree administrative directory (`git rev-parse --git-dir`).
    pub worktree_git_dir: PathBuf,
    /// Shared repository administrative directory (`--git-common-dir`).
    pub common_dir: PathBuf,
    /// Primary index selected by libgit2, including linked-worktree indexes.
    pub index_path: PathBuf,
}

/// HEAD state without collapsing detached, unborn, or missing-target states.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeadObservation {
    Attached {
        symref: String,
        oid: Oid,
    },
    Detached {
        oid: Oid,
    },
    Unborn {
        symref: String,
    },
    /// A symbolic HEAD whose target is absent in a non-empty repository.
    MissingTarget {
        symref: String,
    },
}

impl HeadObservation {
    pub fn oid(&self) -> Option<Oid> {
        match self {
            Self::Attached { oid, .. } | Self::Detached { oid } => Some(*oid),
            Self::Unborn { .. } | Self::MissingTarget { .. } => None,
        }
    }

    pub fn symref(&self) -> Option<&str> {
        match self {
            Self::Attached { symref, .. }
            | Self::Unborn { symref }
            | Self::MissingTarget { symref } => Some(symref),
            Self::Detached { .. } => None,
        }
    }
}

/// A deterministic digest of semantic index entries.
///
/// This is explicitly not a Git tree OID. It covers every entry's raw path,
/// stage, object ID, mode, and flags in canonical sorted order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalIndexDigest(pub String);

/// A deterministic digest of the observed refs and their direct/symbolic targets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalRefsDigest(pub String);

/// Whether the exact Git tree OID could be computed without writing objects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexTreeAvailability {
    ComputedReadOnly,
    Unavailable(IndexTreeUnavailable),
}

/// Typed reasons an index does not denote one stage-0 Git tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexTreeUnavailable {
    NonZeroStages { stages: Vec<u8> },
    UnsupportedObjectFormat { object_format: String },
    UnsupportedEntry { path: Vec<u8>, mode: u32 },
}

/// Stage-aware Git index state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexObservation {
    pub path: PathBuf,
    pub exists: bool,
    pub version: u32,
    pub entries: Vec<IndexEntryObservation>,
    pub canonical_digest: CanonicalIndexDigest,
    /// Exact Git tree OID computed from stage-0 entries without touching the ODB.
    pub tree_oid: Option<Oid>,
    pub tree_availability: IndexTreeAvailability,
    pub head_equivalence: IndexHeadEquivalence,
}

/// One raw Git index entry, including stages 1-3 during conflicts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexEntryObservation {
    pub path: Vec<u8>,
    pub stage: u8,
    pub oid: Oid,
    pub mode: u32,
    pub flags: u16,
    pub flags_extended: u16,
}

/// Read-only comparison between the index entries and the current HEAD tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexHeadEquivalence {
    Equal {
        head_tree: Oid,
    },
    Different {
        head_tree: Oid,
        missing_from_index: Vec<Vec<u8>>,
        added_to_index: Vec<Vec<u8>>,
        changed: Vec<Vec<u8>>,
    },
    NotApplicable,
}

/// Filesystem kind of an administrative marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdminEntryKind {
    Missing,
    File,
    Directory,
    Symlink,
    Other,
}

/// An explicitly checked administrative path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathObservation {
    pub path: PathBuf,
    pub kind: AdminEntryKind,
}

impl PathObservation {
    pub fn is_present(&self) -> bool {
        self.kind != AdminEntryKind::Missing
    }
}

/// Index and ref lock evidence. Only present ref locks are retained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockObservation {
    pub index_lock: PathObservation,
    pub ref_locks: Vec<PathBuf>,
}

/// Git operation marker with its resolved per-worktree path.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OperationMarkerKind {
    Sequencer,
    MergeHead,
    RebaseHead,
    RebaseMerge,
    RebaseApply,
    CherryPickHead,
    RevertHead,
    AutoMerge,
    BisectStart,
    BisectLog,
    BisectNames,
    BisectExpectedRev,
}

impl OperationMarkerKind {
    /// Whether a present marker is authoritative evidence of an active Git
    /// operation. Mirrors the repository observer: `AUTO_MERGE` is a derived
    /// tree ref written by merge-ort for any worktree-updating merge and is
    /// removed on completion, so a standalone AUTO_MERGE is advisory only; a
    /// stopped merge also carries `MERGE_HEAD`.
    pub fn is_active_operation_evidence(self) -> bool {
        !matches!(self, Self::AutoMerge)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sequencer => "sequencer",
            Self::MergeHead => "merge-head",
            Self::RebaseHead => "rebase-head",
            Self::RebaseMerge => "rebase-merge",
            Self::RebaseApply => "rebase-apply",
            Self::CherryPickHead => "cherry-pick-head",
            Self::RevertHead => "revert-head",
            Self::AutoMerge => "auto-merge",
            Self::BisectStart => "bisect-start",
            Self::BisectLog => "bisect-log",
            Self::BisectNames => "bisect-names",
            Self::BisectExpectedRev => "bisect-expected-rev",
        }
    }

    fn relative_path(self) -> &'static str {
        match self {
            Self::Sequencer => "sequencer",
            Self::MergeHead => "MERGE_HEAD",
            Self::RebaseHead => "REBASE_HEAD",
            Self::RebaseMerge => "rebase-merge",
            Self::RebaseApply => "rebase-apply",
            Self::CherryPickHead => "CHERRY_PICK_HEAD",
            Self::RevertHead => "REVERT_HEAD",
            Self::AutoMerge => "AUTO_MERGE",
            Self::BisectStart => "BISECT_START",
            Self::BisectLog => "BISECT_LOG",
            Self::BisectNames => "BISECT_NAMES",
            Self::BisectExpectedRev => "BISECT_EXPECTED_REV",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationMarkerObservation {
    pub marker: OperationMarkerKind,
    pub path: PathBuf,
    pub kind: AdminEntryKind,
}

impl OperationMarkerObservation {
    pub fn is_present(&self) -> bool {
        self.kind != AdminEntryKind::Missing
    }
}

/// Repository state plus every operation marker checked by the observer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationObservation {
    pub repository_state: String,
    pub markers: Vec<OperationMarkerObservation>,
}

impl OperationObservation {
    /// Whether Git owns an in-progress operation. True when libgit2 reports a
    /// non-`Clean` state or an authoritative marker is present; a standalone
    /// `AUTO_MERGE` is advisory and does not, by itself, count.
    pub fn is_in_progress(&self) -> bool {
        self.repository_state != "Clean"
            || self.markers.iter().any(|marker| {
                marker.is_present() && marker.marker.is_active_operation_evidence()
            })
    }

    /// Every present marker, including advisory ones, for reporting.
    pub fn present_markers(&self) -> Vec<OperationMarkerKind> {
        self.markers
            .iter()
            .filter(|marker| marker.is_present())
            .map(|marker| marker.marker)
            .collect()
    }

    /// Present markers that are authoritative evidence of an active operation.
    pub fn active_markers(&self) -> Vec<OperationMarkerKind> {
        self.markers
            .iter()
            .filter(|marker| marker.is_present() && marker.marker.is_active_operation_evidence())
            .map(|marker| marker.marker)
            .collect()
    }
}

/// A relevant Git ref and its current direct or symbolic target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RefObservation {
    pub name: Vec<u8>,
    pub target: RefTargetObservation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RefTargetObservation {
    Direct(Oid),
    Symbolic(Vec<u8>),
    Unresolved,
}

/// Minimal read-only Atomic state used for anchor comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AtomicAnchorObservation {
    pub view: String,
    pub state: String,
}

/// Existing bridge checkpoint fields that CB-0A can compare without mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeCheckpointObservation {
    pub view: String,
    pub atomic_state: String,
    pub git_head: String,
    pub git_tree: String,
}

/// Independent evidence about Atomic/Git manifest equivalence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestEquivalence {
    /// A caller has verified complete Atomic/Git repository manifests.
    Verified,
    /// Complete manifests were compared and differ.
    Mismatch { detail: String },
    /// CB-0A's forensic path intentionally did not perform a mass scan.
    NotComputed,
}

/// A checkpoint that is safe to propose but has not been persisted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvisionalCheckpoint {
    pub source: ProvisionalCheckpointSource,
    pub atomic_view: String,
    pub atomic_state: String,
    pub head_symref: String,
    pub head_oid: Oid,
    pub head_tree_oid: Oid,
    pub index_tree_oid: Oid,
    pub index_digest: CanonicalIndexDigest,
    pub refs_digest: CanonicalRefsDigest,
    pub paths: GitAdminPaths,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionalCheckpointSource {
    ExistingVerifiedCheckpoint,
    VerifiedManifestBootstrap,
}

/// Pure checkpoint eligibility result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProvisionalCheckpointEligibility {
    Eligible(Box<ProvisionalCheckpoint>),
    Unanchored(Unanchored),
}

/// Typed reasons that observed state cannot be treated as an anchor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Unanchored {
    NoGit,
    BareRepository,
    DetachedHead {
        oid: Oid,
    },
    UnbornHead {
        symref: String,
    },
    MissingHeadTarget {
        symref: String,
    },
    UnsupportedHeadSymref {
        symref: String,
    },
    IndexLocked {
        path: PathBuf,
    },
    RefLocked {
        paths: Vec<PathBuf>,
    },
    OperationInProgress {
        repository_state: String,
        markers: Vec<OperationMarkerKind>,
    },
    NonZeroIndexStages {
        entries: Vec<(Vec<u8>, u8)>,
    },
    IndexTreeUnavailable {
        reason: IndexTreeUnavailable,
    },
    IndexDiffersFromHead {
        missing_from_index: Vec<Vec<u8>>,
        added_to_index: Vec<Vec<u8>>,
        changed: Vec<Vec<u8>>,
    },
    AtomicViewMismatch {
        atomic_view: String,
        git_branch: String,
    },
    CheckpointDrift {
        fields: Vec<String>,
    },
    ManifestMismatch {
        detail: String,
    },
    ManifestEquivalenceNotComputed,
}

impl fmt::Display for Unanchored {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoGit => write!(formatter, "no Git repository is present"),
            Self::BareRepository => write!(formatter, "Git repository has no working tree"),
            Self::DetachedHead { oid } => write!(formatter, "Git HEAD is detached at {oid}"),
            Self::UnbornHead { symref } => write!(formatter, "Git HEAD is unborn at {symref}"),
            Self::MissingHeadTarget { symref } => {
                write!(formatter, "Git HEAD target {symref} is missing")
            }
            Self::UnsupportedHeadSymref { symref } => {
                write!(formatter, "Git HEAD symref {symref} is not a local branch")
            }
            Self::IndexLocked { path } => {
                write!(formatter, "Git index lock is present at {}", path.display())
            }
            Self::RefLocked { paths } => {
                write!(
                    formatter,
                    "Git ref locks are present at {} path(s)",
                    paths.len()
                )
            }
            Self::OperationInProgress {
                repository_state,
                markers,
            } => write!(
                formatter,
                "Git operation is in progress (state {repository_state}, {} marker(s))",
                markers.len()
            ),
            Self::NonZeroIndexStages { entries } => write!(
                formatter,
                "Git index contains {} stage 1-3 entr{}",
                entries.len(),
                if entries.len() == 1 { "y" } else { "ies" }
            ),
            Self::IndexTreeUnavailable { reason } => {
                write!(formatter, "Git index tree is unavailable: {reason:?}")
            }
            Self::IndexDiffersFromHead { .. } => {
                write!(formatter, "Git index stage 0 differs from the HEAD tree")
            }
            Self::AtomicViewMismatch {
                atomic_view,
                git_branch,
            } => write!(
                formatter,
                "Atomic view '{atomic_view}' does not match Git branch '{git_branch}'"
            ),
            Self::CheckpointDrift { fields } => {
                write!(formatter, "bridge checkpoint drift: {}", fields.join(", "))
            }
            Self::ManifestMismatch { detail } => {
                write!(formatter, "Atomic/Git manifests differ: {detail}")
            }
            Self::ManifestEquivalenceNotComputed => write!(
                formatter,
                "Atomic/Git manifest equivalence was not computed in forensic mode"
            ),
        }
    }
}

/// Errors mean required Git metadata was malformed or unreadable.
#[derive(Debug)]
pub struct ObservationError {
    message: String,
}

impl ObservationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ObservationError {}

/// Observe Git at `root` without modifying Git, Atomic, or the working tree.
pub fn observe_git(root: &Path) -> Result<GitObservation, ObservationError> {
    let resolved_root = resolve_path(root, root)?;
    let git_marker_exists = fs::symlink_metadata(root.join(".git")).is_ok();
    let repository = match GitRepository::open(root) {
        Ok(repository) => repository,
        Err(error) if error.code() == ErrorCode::NotFound && !git_marker_exists => {
            return Ok(GitObservation::NoGit {
                root: resolved_root,
            });
        }
        Err(error) => {
            return Err(ObservationError::new(format!(
                "cannot read Git repository at '{}': {error}",
                root.display()
            )));
        }
    };

    observe_open_repository(root, &repository)
        .map(Box::new)
        .map(GitObservation::Repository)
}

/// Observe an already-open repository. This helper is also used by the bridge
/// to share HEAD interpretation without changing its mutating reconcile path.
pub fn observe_open_repository(
    root: &Path,
    repository: &GitRepository,
) -> Result<GitRepositoryObservation, ObservationError> {
    let worktree_git_dir = resolve_path(repository.path(), root)?;
    let common_dir = resolve_common_dir(&worktree_git_dir)?;
    let worktree_root = repository
        .workdir()
        .map(|path| resolve_path(path, root))
        .transpose()?;

    let index = repository
        .index()
        .map_err(|error| ObservationError::new(format!("cannot read Git index: {error}")))?;
    let raw_index_path = index
        .path()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| worktree_git_dir.join("index"));
    let index_path = resolve_path(&raw_index_path, &worktree_git_dir)?;
    let paths = GitAdminPaths {
        worktree_root,
        worktree_git_dir,
        common_dir,
        index_path,
    };

    let head = observe_head(repository)?;
    let (head_tree_oid, head_tree_entries) = observe_head_tree(repository, &head)?;
    let index = observe_index(
        repository,
        &index,
        &paths.index_path,
        head_tree_oid,
        &head_tree_entries,
    )?;
    let locks = observe_locks(&paths)?;
    let operation = observe_operation(repository, &paths.worktree_git_dir)?;
    let refs = observe_refs(repository)?;
    let refs_digest = digest_refs(&refs);

    Ok(GitRepositoryObservation {
        paths,
        head,
        head_tree_oid,
        index,
        locks,
        operation,
        refs,
        refs_digest,
    })
}

/// Interpret raw HEAD without requiring it to be attached or born.
pub fn observe_head(repository: &GitRepository) -> Result<HeadObservation, ObservationError> {
    let head = repository
        .find_reference("HEAD")
        .map_err(|error| ObservationError::new(format!("cannot read Git HEAD: {error}")))?;

    if let Some(symref) = head.symbolic_target() {
        let symref = symref.to_string();
        match head.resolve() {
            Ok(resolved) => {
                let oid = resolved.target().ok_or_else(|| {
                    ObservationError::new(format!(
                        "Git HEAD target '{symref}' does not resolve directly to an object"
                    ))
                })?;
                Ok(HeadObservation::Attached { symref, oid })
            }
            Err(error) if error.code() == ErrorCode::NotFound => {
                let references = repository.references().map_err(|state_error| {
                    ObservationError::new(format!(
                        "cannot distinguish unborn Git HEAD from a missing branch: {state_error}"
                    ))
                })?;
                let mut has_resolved_ref = false;
                for reference in references {
                    let reference = reference.map_err(|state_error| {
                        ObservationError::new(format!(
                            "cannot inspect Git refs while classifying HEAD: {state_error}"
                        ))
                    })?;
                    has_resolved_ref |= reference.target().is_some();
                }
                if has_resolved_ref {
                    Ok(HeadObservation::MissingTarget { symref })
                } else {
                    Ok(HeadObservation::Unborn { symref })
                }
            }
            Err(error) if error.code() == ErrorCode::UnbornBranch => {
                Ok(HeadObservation::Unborn { symref })
            }
            Err(error) => Err(ObservationError::new(format!(
                "cannot resolve Git HEAD target '{symref}': {error}"
            ))),
        }
    } else if let Some(oid) = head.target() {
        Ok(HeadObservation::Detached { oid })
    } else {
        Err(ObservationError::new(
            "Git HEAD is neither a symbolic reference nor a direct object ID",
        ))
    }
}

/// Classify checkpoint eligibility using only supplied observations/evidence.
/// This function performs no I/O and cannot persist the provisional result.
pub fn classify_provisional_checkpoint(
    git: &GitObservation,
    atomic: &AtomicAnchorObservation,
    checkpoint: Option<&BridgeCheckpointObservation>,
    manifest_equivalence: &ManifestEquivalence,
) -> ProvisionalCheckpointEligibility {
    let repository = match git {
        GitObservation::NoGit { .. } => {
            return ProvisionalCheckpointEligibility::Unanchored(Unanchored::NoGit);
        }
        GitObservation::Repository(repository) => repository,
    };

    if repository.paths.worktree_root.is_none() {
        return ProvisionalCheckpointEligibility::Unanchored(Unanchored::BareRepository);
    }

    let (symref, head_oid) = match &repository.head {
        HeadObservation::Attached { symref, oid } => (symref, *oid),
        HeadObservation::Detached { oid } => {
            return ProvisionalCheckpointEligibility::Unanchored(Unanchored::DetachedHead {
                oid: *oid,
            });
        }
        HeadObservation::Unborn { symref } => {
            return ProvisionalCheckpointEligibility::Unanchored(Unanchored::UnbornHead {
                symref: symref.clone(),
            });
        }
        HeadObservation::MissingTarget { symref } => {
            return ProvisionalCheckpointEligibility::Unanchored(Unanchored::MissingHeadTarget {
                symref: symref.clone(),
            });
        }
    };

    let Some(git_branch) = symref.strip_prefix("refs/heads/") else {
        return ProvisionalCheckpointEligibility::Unanchored(Unanchored::UnsupportedHeadSymref {
            symref: symref.clone(),
        });
    };

    if repository.locks.index_lock.is_present() {
        return ProvisionalCheckpointEligibility::Unanchored(Unanchored::IndexLocked {
            path: repository.locks.index_lock.path.clone(),
        });
    }
    if !repository.locks.ref_locks.is_empty() {
        return ProvisionalCheckpointEligibility::Unanchored(Unanchored::RefLocked {
            paths: repository.locks.ref_locks.clone(),
        });
    }
    if repository.operation.is_in_progress() {
        return ProvisionalCheckpointEligibility::Unanchored(Unanchored::OperationInProgress {
            repository_state: repository.operation.repository_state.clone(),
            markers: repository.operation.present_markers(),
        });
    }

    let nonzero_entries: Vec<_> = repository
        .index
        .entries
        .iter()
        .filter(|entry| entry.stage != 0)
        .map(|entry| (entry.path.clone(), entry.stage))
        .collect();
    if !nonzero_entries.is_empty() {
        return ProvisionalCheckpointEligibility::Unanchored(Unanchored::NonZeroIndexStages {
            entries: nonzero_entries,
        });
    }

    let index_tree_oid = match (
        repository.index.tree_oid,
        &repository.index.tree_availability,
    ) {
        (Some(oid), IndexTreeAvailability::ComputedReadOnly) => oid,
        (_, IndexTreeAvailability::Unavailable(reason)) => {
            return ProvisionalCheckpointEligibility::Unanchored(
                Unanchored::IndexTreeUnavailable {
                    reason: reason.clone(),
                },
            );
        }
        (None, IndexTreeAvailability::ComputedReadOnly) => {
            return ProvisionalCheckpointEligibility::Unanchored(
                Unanchored::ManifestEquivalenceNotComputed,
            );
        }
    };

    let head_tree_oid = match &repository.index.head_equivalence {
        IndexHeadEquivalence::Equal { head_tree } => *head_tree,
        IndexHeadEquivalence::Different {
            missing_from_index,
            added_to_index,
            changed,
            ..
        } => {
            return ProvisionalCheckpointEligibility::Unanchored(
                Unanchored::IndexDiffersFromHead {
                    missing_from_index: missing_from_index.clone(),
                    added_to_index: added_to_index.clone(),
                    changed: changed.clone(),
                },
            );
        }
        IndexHeadEquivalence::NotApplicable => {
            return ProvisionalCheckpointEligibility::Unanchored(
                Unanchored::ManifestEquivalenceNotComputed,
            );
        }
    };

    if git_branch != atomic.view {
        return ProvisionalCheckpointEligibility::Unanchored(Unanchored::AtomicViewMismatch {
            atomic_view: atomic.view.clone(),
            git_branch: git_branch.to_string(),
        });
    }

    let source = if let Some(checkpoint) = checkpoint {
        let mut drift = Vec::new();
        if checkpoint.view != atomic.view {
            drift.push("view".to_string());
        }
        if checkpoint.atomic_state != atomic.state {
            drift.push("atomic-state".to_string());
        }
        if checkpoint.git_head != head_oid.to_string() {
            drift.push("git-head".to_string());
        }
        if checkpoint.git_tree != head_tree_oid.to_string() {
            drift.push("git-tree".to_string());
        }
        if !drift.is_empty() {
            return ProvisionalCheckpointEligibility::Unanchored(Unanchored::CheckpointDrift {
                fields: drift,
            });
        }
        ProvisionalCheckpointSource::ExistingVerifiedCheckpoint
    } else {
        match manifest_equivalence {
            ManifestEquivalence::Verified => ProvisionalCheckpointSource::VerifiedManifestBootstrap,
            ManifestEquivalence::Mismatch { detail } => {
                return ProvisionalCheckpointEligibility::Unanchored(
                    Unanchored::ManifestMismatch {
                        detail: detail.clone(),
                    },
                );
            }
            ManifestEquivalence::NotComputed => {
                return ProvisionalCheckpointEligibility::Unanchored(
                    Unanchored::ManifestEquivalenceNotComputed,
                );
            }
        }
    };

    ProvisionalCheckpointEligibility::Eligible(Box::new(ProvisionalCheckpoint {
        source,
        atomic_view: atomic.view.clone(),
        atomic_state: atomic.state.clone(),
        head_symref: symref.clone(),
        head_oid,
        head_tree_oid,
        index_tree_oid,
        index_digest: repository.index.canonical_digest.clone(),
        refs_digest: repository.refs_digest.clone(),
        paths: repository.paths.clone(),
    }))
}

fn observe_head_tree(
    repository: &GitRepository,
    head: &HeadObservation,
) -> Result<(Option<Oid>, Vec<TreeEntryObservation>), ObservationError> {
    let Some(oid) = head.oid() else {
        return Ok((None, Vec::new()));
    };
    let commit = repository.find_commit(oid).map_err(|error| {
        ObservationError::new(format!("cannot read Git HEAD commit {oid}: {error}"))
    })?;
    let tree = commit.tree().map_err(|error| {
        ObservationError::new(format!("cannot read Git HEAD tree for {oid}: {error}"))
    })?;
    let tree_oid = tree.id();
    let mut entries = Vec::new();
    collect_tree_entries(repository, &tree, &[], &mut entries)?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((Some(tree_oid), entries))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeEntryObservation {
    path: Vec<u8>,
    oid: Oid,
    mode: u32,
}

fn collect_tree_entries(
    repository: &GitRepository,
    tree: &git2::Tree<'_>,
    prefix: &[u8],
    entries: &mut Vec<TreeEntryObservation>,
) -> Result<(), ObservationError> {
    for entry in tree.iter() {
        let mut path = prefix.to_vec();
        if !path.is_empty() {
            path.push(b'/');
        }
        path.extend_from_slice(entry.name_bytes());

        if entry.kind() == Some(ObjectType::Tree) {
            let child = repository.find_tree(entry.id()).map_err(|error| {
                ObservationError::new(format!(
                    "cannot read Git tree {} while observing '{}': {error}",
                    entry.id(),
                    display_git_bytes(&path)
                ))
            })?;
            collect_tree_entries(repository, &child, &path, entries)?;
        } else {
            entries.push(TreeEntryObservation {
                path,
                oid: entry.id(),
                mode: entry.filemode() as u32,
            });
        }
    }
    Ok(())
}

fn observe_index(
    repository: &GitRepository,
    index: &git2::Index,
    index_path: &Path,
    head_tree_oid: Option<Oid>,
    head_tree_entries: &[TreeEntryObservation],
) -> Result<IndexObservation, ObservationError> {
    let mut entries: Vec<_> = index
        .iter()
        .map(|entry| IndexEntryObservation {
            stage: ((entry.flags >> 12) & 0x3) as u8,
            path: entry.path,
            oid: entry.id,
            mode: entry.mode,
            flags: entry.flags,
            flags_extended: entry.flags_extended,
        })
        .collect();
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.stage.cmp(&right.stage))
            .then_with(|| left.oid.as_bytes().cmp(right.oid.as_bytes()))
    });

    let canonical_digest = digest_index(&entries);
    let (tree_oid, tree_availability) = compute_index_tree_oid(repository, &entries)?;
    let head_equivalence =
        compare_index_to_head(&entries, tree_oid, head_tree_oid, head_tree_entries);
    let exists = match fs::symlink_metadata(index_path) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(ObservationError::new(format!(
                "cannot inspect Git index path '{}': {error}",
                index_path.display()
            )));
        }
    };

    Ok(IndexObservation {
        path: index_path.to_path_buf(),
        exists,
        version: index.version(),
        entries,
        canonical_digest,
        tree_oid,
        tree_availability,
        head_equivalence,
    })
}

#[derive(Default)]
struct InMemoryIndexTree {
    entries: BTreeMap<Vec<u8>, InMemoryIndexTreeEntry>,
}

enum InMemoryIndexTreeEntry {
    Directory(InMemoryIndexTree),
    Object { oid: Oid, mode: CanonicalTreeMode },
}

#[derive(Clone, Copy)]
enum CanonicalTreeMode {
    Tree,
    Regular,
    Executable,
    Symlink,
    Gitlink,
}

impl CanonicalTreeMode {
    fn from_index_mode(mode: u32) -> Option<Self> {
        match mode & 0o170000 {
            0o040000 => Some(Self::Tree),
            0o100000 if mode & 0o111 == 0 => Some(Self::Regular),
            0o100000 => Some(Self::Executable),
            0o120000 => Some(Self::Symlink),
            0o160000 => Some(Self::Gitlink),
            _ => None,
        }
    }

    fn encoded(self) -> &'static [u8] {
        match self {
            Self::Tree => b"40000",
            Self::Regular => b"100644",
            Self::Executable => b"100755",
            Self::Symlink => b"120000",
            Self::Gitlink => b"160000",
        }
    }

    fn is_tree(self) -> bool {
        matches!(self, Self::Tree)
    }
}

struct EncodedTreeEntry {
    name: Vec<u8>,
    oid: Oid,
    mode: CanonicalTreeMode,
}

fn compute_index_tree_oid(
    repository: &GitRepository,
    entries: &[IndexEntryObservation],
) -> Result<(Option<Oid>, IndexTreeAvailability), ObservationError> {
    let stages = entries
        .iter()
        .filter(|entry| entry.stage != 0)
        .map(|entry| entry.stage)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if !stages.is_empty() {
        return Ok((
            None,
            IndexTreeAvailability::Unavailable(IndexTreeUnavailable::NonZeroStages { stages }),
        ));
    }

    let config = repository.config().map_err(|error| {
        ObservationError::new(format!(
            "cannot read Git object format configuration: {error}"
        ))
    })?;
    match config.get_string("extensions.objectFormat") {
        Ok(object_format) if !object_format.eq_ignore_ascii_case("sha1") => {
            return Ok((
                None,
                IndexTreeAvailability::Unavailable(IndexTreeUnavailable::UnsupportedObjectFormat {
                    object_format,
                }),
            ));
        }
        Ok(_) => {}
        Err(error) if error.code() == ErrorCode::NotFound => {}
        Err(error) => {
            return Err(ObservationError::new(format!(
                "cannot read Git object format configuration: {error}"
            )));
        }
    }

    let mut root = InMemoryIndexTree::default();
    for entry in entries {
        let Some(mode) = CanonicalTreeMode::from_index_mode(entry.mode) else {
            return Ok((
                None,
                IndexTreeAvailability::Unavailable(IndexTreeUnavailable::UnsupportedEntry {
                    path: entry.path.clone(),
                    mode: entry.mode,
                }),
            ));
        };
        if entry.oid == Oid::zero() || insert_index_tree_entry(&mut root, entry, mode).is_err() {
            return Ok((
                None,
                IndexTreeAvailability::Unavailable(IndexTreeUnavailable::UnsupportedEntry {
                    path: entry.path.clone(),
                    mode: entry.mode,
                }),
            ));
        }
    }

    let oid = hash_index_tree(&root)?;
    Ok((Some(oid), IndexTreeAvailability::ComputedReadOnly))
}

fn insert_index_tree_entry(
    tree: &mut InMemoryIndexTree,
    entry: &IndexEntryObservation,
    mode: CanonicalTreeMode,
) -> Result<(), ()> {
    let path = if mode.is_tree() {
        entry.path.strip_suffix(b"/").unwrap_or(&entry.path)
    } else {
        &entry.path
    };
    if path.is_empty() || path.contains(&0) {
        return Err(());
    }
    let components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
    if components.iter().any(|component| component.is_empty()) {
        return Err(());
    }
    insert_index_tree_components(tree, &components, entry.oid, mode)
}

fn insert_index_tree_components(
    tree: &mut InMemoryIndexTree,
    components: &[&[u8]],
    oid: Oid,
    mode: CanonicalTreeMode,
) -> Result<(), ()> {
    let (name, remaining) = components.split_first().ok_or(())?;
    if remaining.is_empty() {
        if tree.entries.contains_key(*name) {
            return Err(());
        }
        tree.entries
            .insert(name.to_vec(), InMemoryIndexTreeEntry::Object { oid, mode });
        return Ok(());
    }

    let child = tree
        .entries
        .entry(name.to_vec())
        .or_insert_with(|| InMemoryIndexTreeEntry::Directory(InMemoryIndexTree::default()));
    match child {
        InMemoryIndexTreeEntry::Directory(child) => {
            insert_index_tree_components(child, remaining, oid, mode)
        }
        InMemoryIndexTreeEntry::Object { .. } => Err(()),
    }
}

fn hash_index_tree(tree: &InMemoryIndexTree) -> Result<Oid, ObservationError> {
    let mut entries = Vec::with_capacity(tree.entries.len());
    for (name, entry) in &tree.entries {
        let (oid, mode) = match entry {
            InMemoryIndexTreeEntry::Directory(child) => {
                (hash_index_tree(child)?, CanonicalTreeMode::Tree)
            }
            InMemoryIndexTreeEntry::Object { oid, mode } => (*oid, *mode),
        };
        entries.push(EncodedTreeEntry {
            name: name.clone(),
            oid,
            mode,
        });
    }
    entries.sort_by(git_tree_entry_order);

    let mut bytes = Vec::new();
    for entry in entries {
        bytes.extend_from_slice(entry.mode.encoded());
        bytes.push(b' ');
        bytes.extend_from_slice(&entry.name);
        bytes.push(0);
        bytes.extend_from_slice(entry.oid.as_bytes());
    }
    Oid::hash_object(ObjectType::Tree, &bytes).map_err(|error| {
        ObservationError::new(format!("cannot hash in-memory Git index tree: {error}"))
    })
}

fn git_tree_entry_order(left: &EncodedTreeEntry, right: &EncodedTreeEntry) -> std::cmp::Ordering {
    let common = left.name.len().min(right.name.len());
    match left.name[..common].cmp(&right.name[..common]) {
        std::cmp::Ordering::Equal => {
            let left_next = left.name.get(common).copied().unwrap_or_else(|| {
                if left.mode.is_tree() {
                    b'/'
                } else {
                    0
                }
            });
            let right_next = right.name.get(common).copied().unwrap_or_else(|| {
                if right.mode.is_tree() {
                    b'/'
                } else {
                    0
                }
            });
            left_next.cmp(&right_next)
        }
        ordering => ordering,
    }
}

fn compare_index_to_head(
    entries: &[IndexEntryObservation],
    index_tree_oid: Option<Oid>,
    head_tree_oid: Option<Oid>,
    head_entries: &[TreeEntryObservation],
) -> IndexHeadEquivalence {
    let (Some(index_tree), Some(head_tree)) = (index_tree_oid, head_tree_oid) else {
        return IndexHeadEquivalence::NotApplicable;
    };
    if index_tree == head_tree {
        return IndexHeadEquivalence::Equal { head_tree };
    }

    let index_entries: BTreeMap<_, _> = entries
        .iter()
        .filter(|entry| entry.stage == 0)
        .map(|entry| (entry.path.clone(), (entry.oid, entry.mode)))
        .collect();
    let tree_entries: BTreeMap<_, _> = head_entries
        .iter()
        .map(|entry| (entry.path.clone(), (entry.oid, entry.mode)))
        .collect();

    let missing_from_index = tree_entries
        .keys()
        .filter(|path| !index_entries.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>();
    let added_to_index = index_entries
        .keys()
        .filter(|path| !tree_entries.contains_key(*path))
        .cloned()
        .collect::<Vec<_>>();
    let changed = tree_entries
        .iter()
        .filter_map(|(path, tree_value)| {
            index_entries
                .get(path)
                .filter(|index_value| *index_value != tree_value)
                .map(|_| path.clone())
        })
        .collect::<Vec<_>>();

    IndexHeadEquivalence::Different {
        head_tree,
        missing_from_index,
        added_to_index,
        changed,
    }
}

fn digest_index(entries: &[IndexEntryObservation]) -> CanonicalIndexDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(INDEX_DIGEST_DOMAIN);
    hasher.update(&(entries.len() as u64).to_le_bytes());
    for entry in entries {
        hasher.update(&(entry.path.len() as u64).to_le_bytes());
        hasher.update(&entry.path);
        hasher.update(&[entry.stage]);
        hasher.update(entry.oid.as_bytes());
        hasher.update(&entry.mode.to_le_bytes());
        hasher.update(&entry.flags.to_le_bytes());
        hasher.update(&entry.flags_extended.to_le_bytes());
    }
    CanonicalIndexDigest(hasher.finalize().to_hex().to_string())
}

fn observe_locks(paths: &GitAdminPaths) -> Result<LockObservation, ObservationError> {
    let index_lock_path = append_suffix(&paths.index_path, ".lock");
    let index_lock = observe_path(index_lock_path)?;
    let mut ref_locks = BTreeSet::new();

    for admin_dir in [&paths.common_dir, &paths.worktree_git_dir] {
        collect_top_level_locks(admin_dir, &mut ref_locks)?;
        collect_locks_recursive(&admin_dir.join("refs"), &mut ref_locks)?;
    }
    ref_locks.remove(&index_lock.path);

    Ok(LockObservation {
        index_lock,
        ref_locks: ref_locks.into_iter().collect(),
    })
}

fn collect_top_level_locks(
    directory: &Path,
    locks: &mut BTreeSet<PathBuf>,
) -> Result<(), ObservationError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(ObservationError::new(format!(
                "cannot inspect Git administrative directory '{}': {error}",
                directory.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            ObservationError::new(format!(
                "cannot inspect entry under '{}': {error}",
                directory.display()
            ))
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("lock") {
            locks.insert(path);
        }
    }
    Ok(())
}

fn collect_locks_recursive(
    directory: &Path,
    locks: &mut BTreeSet<PathBuf>,
) -> Result<(), ObservationError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(ObservationError::new(format!(
                "cannot inspect Git refs directory '{}': {error}",
                directory.display()
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            ObservationError::new(format!(
                "cannot inspect entry under '{}': {error}",
                directory.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            ObservationError::new(format!(
                "cannot inspect Git administrative path '{}': {error}",
                entry.path().display()
            ))
        })?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_locks_recursive(&path, locks)?;
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("lock") {
            locks.insert(path);
        }
    }
    Ok(())
}

fn observe_operation(
    repository: &GitRepository,
    worktree_git_dir: &Path,
) -> Result<OperationObservation, ObservationError> {
    let marker_kinds = [
        OperationMarkerKind::Sequencer,
        OperationMarkerKind::MergeHead,
        OperationMarkerKind::RebaseHead,
        OperationMarkerKind::RebaseMerge,
        OperationMarkerKind::RebaseApply,
        OperationMarkerKind::CherryPickHead,
        OperationMarkerKind::RevertHead,
        OperationMarkerKind::AutoMerge,
        OperationMarkerKind::BisectStart,
        OperationMarkerKind::BisectLog,
        OperationMarkerKind::BisectNames,
        OperationMarkerKind::BisectExpectedRev,
    ];
    let mut markers = Vec::with_capacity(marker_kinds.len());
    for marker in marker_kinds {
        let observed = observe_path(worktree_git_dir.join(marker.relative_path()))?;
        markers.push(OperationMarkerObservation {
            marker,
            path: observed.path,
            kind: observed.kind,
        });
    }

    Ok(OperationObservation {
        repository_state: repository_state_name(repository.state()).to_string(),
        markers,
    })
}

fn repository_state_name(state: RepositoryState) -> &'static str {
    match state {
        RepositoryState::Clean => "Clean",
        RepositoryState::Merge => "Merge",
        RepositoryState::Revert => "Revert",
        RepositoryState::RevertSequence => "RevertSequence",
        RepositoryState::CherryPick => "CherryPick",
        RepositoryState::CherryPickSequence => "CherryPickSequence",
        RepositoryState::Bisect => "Bisect",
        RepositoryState::Rebase => "Rebase",
        RepositoryState::RebaseInteractive => "RebaseInteractive",
        RepositoryState::RebaseMerge => "RebaseMerge",
        RepositoryState::ApplyMailbox => "ApplyMailbox",
        RepositoryState::ApplyMailboxOrRebase => "ApplyMailboxOrRebase",
    }
}

fn observe_refs(repository: &GitRepository) -> Result<Vec<RefObservation>, ObservationError> {
    let references = repository
        .references()
        .map_err(|error| ObservationError::new(format!("cannot enumerate Git refs: {error}")))?;
    let mut refs = Vec::new();
    for reference in references {
        let reference = reference.map_err(|error| {
            ObservationError::new(format!("cannot read an enumerated Git ref: {error}"))
        })?;
        let target = if let Some(oid) = reference.target() {
            RefTargetObservation::Direct(oid)
        } else if let Some(symbolic) = reference.symbolic_target() {
            RefTargetObservation::Symbolic(symbolic.as_bytes().to_vec())
        } else {
            RefTargetObservation::Unresolved
        };
        refs.push(RefObservation {
            name: reference.name_bytes().to_vec(),
            target,
        });
    }
    refs.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(refs)
}

fn digest_refs(refs: &[RefObservation]) -> CanonicalRefsDigest {
    let mut hasher = blake3::Hasher::new();
    hasher.update(REFS_DIGEST_DOMAIN);
    let publishable_count = refs
        .iter()
        .filter(|reference| !is_wip_ref(&reference.name))
        .count();
    hasher.update(&(publishable_count as u64).to_le_bytes());
    for reference in refs.iter().filter(|reference| !is_wip_ref(&reference.name)) {
        hasher.update(&(reference.name.len() as u64).to_le_bytes());
        hasher.update(&reference.name);
        match &reference.target {
            RefTargetObservation::Direct(oid) => {
                hasher.update(&[0]);
                hasher.update(oid.as_bytes());
            }
            RefTargetObservation::Symbolic(target) => {
                hasher.update(&[1]);
                hasher.update(&(target.len() as u64).to_le_bytes());
                hasher.update(target);
            }
            RefTargetObservation::Unresolved => {
                hasher.update(&[2]);
            }
        }
    }
    CanonicalRefsDigest(hasher.finalize().to_hex().to_string())
}

fn observe_path(path: PathBuf) -> Result<PathObservation, ObservationError> {
    let kind = match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                AdminEntryKind::Symlink
            } else if file_type.is_file() {
                AdminEntryKind::File
            } else if file_type.is_dir() {
                AdminEntryKind::Directory
            } else {
                AdminEntryKind::Other
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => AdminEntryKind::Missing,
        Err(error) => {
            return Err(ObservationError::new(format!(
                "cannot inspect Git administrative path '{}': {error}",
                path.display()
            )));
        }
    };
    Ok(PathObservation { path, kind })
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn resolve_common_dir(worktree_git_dir: &Path) -> Result<PathBuf, ObservationError> {
    let path = worktree_git_dir.join("commondir");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(worktree_git_dir.to_path_buf());
        }
        Err(error) => {
            return Err(ObservationError::new(format!(
                "cannot read Git common-dir pointer '{}': {error}",
                path.display()
            )));
        }
    };
    let value = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let value = value.strip_suffix(b"\r").unwrap_or(value);
    if value.is_empty() || value.contains(&0) || value.contains(&b'\n') || value.contains(&b'\r') {
        return Err(ObservationError::new(format!(
            "Git common-dir pointer '{}' is malformed",
            path.display()
        )));
    }
    let value = std::str::from_utf8(value).map_err(|_| {
        ObservationError::new(format!(
            "Git common-dir pointer '{}' is not valid UTF-8",
            path.display()
        ))
    })?;
    resolve_path(Path::new(value), worktree_git_dir)
}

fn resolve_path(path: &Path, base: &Path) -> Result<PathBuf, ObservationError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let base = if base.is_absolute() {
            base.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| {
                    ObservationError::new(format!("cannot resolve current directory: {error}"))
                })?
                .join(base)
        };
        base.join(path)
    };
    Ok(fs::canonicalize(&absolute).unwrap_or(absolute))
}

/// Reversible display for raw Git path/ref bytes.
pub fn display_git_bytes(bytes: &[u8]) -> String {
    let mut output = String::new();
    for byte in bytes {
        match *byte {
            b' '..=b'~' if *byte != b'\\' => output.push(*byte as char),
            b'\\' => output.push_str("\\\\"),
            _ => output.push_str(&format!("\\x{byte:02x}")),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    use git2::{ResetType, Signature};
    use tempfile::TempDir;

    fn git(root: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .args(args)
            .current_dir(root)
            .output()
            .expect("run git")
    }

    fn initialized_repository() -> (TempDir, GitRepository) {
        let directory = TempDir::new().expect("temp directory");
        let repository = GitRepository::init(directory.path()).expect("init repository");
        repository
            .set_head("refs/heads/main")
            .expect("set unborn main");
        (directory, repository)
    }

    fn commit_file(repository: &GitRepository, path: &str, bytes: &[u8], message: &str) -> Oid {
        let root = repository.workdir().expect("worktree");
        fs::write(root.join(path), bytes).expect("write file");
        let mut index = repository.index().expect("index");
        index.add_path(Path::new(path)).expect("add path");
        index.write().expect("write index");
        let tree_oid = index.write_tree().expect("write tree");
        let tree = repository.find_tree(tree_oid).expect("find tree");
        let signature = Signature::now("Atomic Test", "atomic@example.com").expect("signature");
        let parents = repository
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| repository.find_commit(oid).expect("parent"));
        match parents.as_ref() {
            Some(parent) => repository
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    message,
                    &tree,
                    &[parent],
                )
                .expect("commit"),
            None => repository
                .commit(Some("HEAD"), &signature, &signature, message, &tree, &[])
                .expect("initial commit"),
        }
    }

    fn repository_observation(root: &Path) -> Box<GitRepositoryObservation> {
        match observe_git(root).expect("observe") {
            GitObservation::Repository(observation) => observation,
            GitObservation::NoGit { .. } => panic!("expected Git repository"),
        }
    }

    fn checkpoint_for(
        observation: &GitRepositoryObservation,
        atomic: &AtomicAnchorObservation,
    ) -> BridgeCheckpointObservation {
        BridgeCheckpointObservation {
            view: atomic.view.clone(),
            atomic_state: atomic.state.clone(),
            git_head: observation.head.oid().expect("head oid").to_string(),
            git_tree: observation.head_tree_oid.expect("head tree").to_string(),
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    enum SnapshotValue {
        Directory,
        File(Vec<u8>),
        Symlink(PathBuf),
        Other,
    }

    fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, SnapshotValue> {
        fn collect(root: &Path, current: &Path, values: &mut BTreeMap<PathBuf, SnapshotValue>) {
            let mut entries = fs::read_dir(current)
                .expect("read snapshot directory")
                .collect::<Result<Vec<_>, _>>()
                .expect("read snapshot entries");
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let relative = path.strip_prefix(root).expect("relative").to_path_buf();
                let file_type = entry.file_type().expect("file type");
                if file_type.is_symlink() {
                    values.insert(
                        relative,
                        SnapshotValue::Symlink(fs::read_link(&path).expect("read link")),
                    );
                } else if file_type.is_dir() {
                    values.insert(relative, SnapshotValue::Directory);
                    collect(root, &path, values);
                } else if file_type.is_file() {
                    values.insert(
                        relative,
                        SnapshotValue::File(fs::read(&path).expect("read file")),
                    );
                } else {
                    values.insert(relative, SnapshotValue::Other);
                }
            }
        }

        let mut values = BTreeMap::new();
        collect(root, root, &mut values);
        values
    }

    #[test]
    fn observes_attached_head_index_refs_and_is_non_mutating() {
        let (directory, repository) = initialized_repository();
        let oid = commit_file(&repository, "file.txt", b"one\n", "initial");
        drop(repository);
        let before = snapshot_tree(directory.path());

        let observation = repository_observation(directory.path());

        assert_eq!(
            observation.head,
            HeadObservation::Attached {
                symref: "refs/heads/main".to_string(),
                oid,
            }
        );
        assert_eq!(observation.index.entries.len(), 1);
        assert_eq!(observation.index.entries[0].stage, 0);
        assert!(matches!(
            observation.index.head_equivalence,
            IndexHeadEquivalence::Equal { .. }
        ));
        assert!(observation
            .refs
            .iter()
            .any(|reference| reference.name == b"refs/heads/main"));
        assert_eq!(before, snapshot_tree(directory.path()));
    }

    #[test]
    fn read_only_index_tree_oid_matches_git_write_tree() {
        let (directory, repository) = initialized_repository();
        fs::create_dir_all(directory.path().join("foo")).expect("foo directory");
        fs::create_dir_all(directory.path().join("nested/deeper")).expect("nested directory");
        fs::write(directory.path().join("foo."), b"dot\n").expect("dot file");
        fs::write(directory.path().join("foo/bar"), b"bar\n").expect("bar file");
        fs::write(directory.path().join("foo0"), b"zero\n").expect("zero file");
        fs::write(directory.path().join("nested/deeper/value"), b"value\n").expect("nested file");
        drop(repository);
        assert!(git(directory.path(), &["add", "--all"]).status.success());
        let before = snapshot_tree(directory.path());

        let observation = repository_observation(directory.path());

        assert_eq!(
            observation.index.tree_availability,
            IndexTreeAvailability::ComputedReadOnly
        );
        let observed = observation.index.tree_oid.expect("computed tree OID");
        assert_eq!(before, snapshot_tree(directory.path()));

        let output = git(directory.path(), &["write-tree"]);
        assert!(
            output.status.success(),
            "git write-tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = String::from_utf8(output.stdout)
            .expect("UTF-8 tree OID")
            .trim()
            .parse::<Oid>()
            .expect("parse tree OID");
        assert_eq!(observed, expected);
    }

    #[test]
    fn classifies_verified_equivalent_state_as_provisionally_eligible() {
        let (directory, repository) = initialized_repository();
        commit_file(&repository, "file.txt", b"one\n", "initial");
        let observation = GitObservation::Repository(repository_observation(directory.path()));
        let atomic = AtomicAnchorObservation {
            view: "main".to_string(),
            state: "atomic-state".to_string(),
        };

        let eligibility = classify_provisional_checkpoint(
            &observation,
            &atomic,
            None,
            &ManifestEquivalence::Verified,
        );

        assert!(matches!(
            eligibility,
            ProvisionalCheckpointEligibility::Eligible(checkpoint)
                if checkpoint.source == ProvisionalCheckpointSource::VerifiedManifestBootstrap
        ));
    }

    #[test]
    fn index_or_manifest_non_equivalence_is_typed_unanchored() {
        let (directory, repository) = initialized_repository();
        commit_file(&repository, "file.txt", b"one\n", "initial");
        let atomic = AtomicAnchorObservation {
            view: "main".into(),
            state: "state".into(),
        };
        let clean = GitObservation::Repository(repository_observation(directory.path()));
        assert!(matches!(
            classify_provisional_checkpoint(
                &clean,
                &atomic,
                None,
                &ManifestEquivalence::Mismatch {
                    detail: "repository roots differ".into(),
                },
            ),
            ProvisionalCheckpointEligibility::Unanchored(Unanchored::ManifestMismatch { .. })
        ));

        fs::write(directory.path().join("file.txt"), b"staged\n").expect("staged file");
        let mut index = repository.index().expect("index");
        index
            .add_path(Path::new("file.txt"))
            .expect("stage changed file");
        index.write().expect("write changed index");
        drop(index);
        drop(repository);
        let staged = GitObservation::Repository(repository_observation(directory.path()));
        assert!(matches!(
            classify_provisional_checkpoint(&staged, &atomic, None, &ManifestEquivalence::Verified,),
            ProvisionalCheckpointEligibility::Unanchored(Unanchored::IndexDiffersFromHead { .. })
        ));
    }

    #[test]
    fn checkout_is_typed_unanchored_without_mutation() {
        let (directory, repository) = initialized_repository();
        let main = commit_file(&repository, "file.txt", b"main\n", "main");
        let main_observation = repository_observation(directory.path());
        let atomic = AtomicAnchorObservation {
            view: "main".to_string(),
            state: "state-main".to_string(),
        };
        let checkpoint = checkpoint_for(&main_observation, &atomic);

        repository
            .branch(
                "feature",
                &repository.find_commit(main).expect("main commit"),
                false,
            )
            .expect("feature branch");
        repository
            .set_head("refs/heads/feature")
            .expect("switch HEAD");
        repository
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .expect("checkout feature");
        drop(repository);
        let before = snapshot_tree(directory.path());
        let observed = GitObservation::Repository(repository_observation(directory.path()));

        let classification = classify_provisional_checkpoint(
            &observed,
            &atomic,
            Some(&checkpoint),
            &ManifestEquivalence::NotComputed,
        );
        assert!(matches!(
            classification,
            ProvisionalCheckpointEligibility::Unanchored(Unanchored::AtomicViewMismatch { .. })
        ));
        assert_eq!(before, snapshot_tree(directory.path()));
    }

    #[test]
    fn detached_head_is_typed_unanchored() {
        let (directory, repository) = initialized_repository();
        let oid = commit_file(&repository, "file.txt", b"one\n", "initial");
        repository.set_head_detached(oid).expect("detach HEAD");
        drop(repository);
        let observation = GitObservation::Repository(repository_observation(directory.path()));
        let classification = classify_provisional_checkpoint(
            &observation,
            &AtomicAnchorObservation {
                view: "main".into(),
                state: "state".into(),
            },
            None,
            &ManifestEquivalence::NotComputed,
        );
        assert_eq!(
            classification,
            ProvisionalCheckpointEligibility::Unanchored(Unanchored::DetachedHead { oid })
        );
    }

    #[test]
    fn reset_is_reported_as_checkpoint_drift() {
        let (directory, repository) = initialized_repository();
        let first = commit_file(&repository, "file.txt", b"one\n", "first");
        let atomic = AtomicAnchorObservation {
            view: "main".into(),
            state: "state".into(),
        };
        commit_file(&repository, "file.txt", b"two\n", "second");
        let checkpoint = checkpoint_for(&repository_observation(directory.path()), &atomic);
        let first_object = repository.find_object(first, None).expect("first object");
        repository
            .reset(&first_object, ResetType::Hard, None)
            .expect("hard reset");
        drop(first_object);
        drop(repository);

        let observed = GitObservation::Repository(repository_observation(directory.path()));
        let classification = classify_provisional_checkpoint(
            &observed,
            &atomic,
            Some(&checkpoint),
            &ManifestEquivalence::NotComputed,
        );
        assert!(matches!(
            classification,
            ProvisionalCheckpointEligibility::Unanchored(Unanchored::CheckpointDrift { .. })
        ));
    }

    #[test]
    fn unborn_head_is_observed_and_typed() {
        let (directory, repository) = initialized_repository();
        drop(repository);
        let observation = GitObservation::Repository(repository_observation(directory.path()));
        assert_eq!(
            match &observation {
                GitObservation::Repository(repository) => &repository.head,
                GitObservation::NoGit { .. } => panic!("expected repository"),
            },
            &HeadObservation::Unborn {
                symref: "refs/heads/main".into()
            }
        );
        assert!(matches!(
            classify_provisional_checkpoint(
                &observation,
                &AtomicAnchorObservation {
                    view: "main".into(),
                    state: "state".into(),
                },
                None,
                &ManifestEquivalence::NotComputed,
            ),
            ProvisionalCheckpointEligibility::Unanchored(Unanchored::UnbornHead { .. })
        ));
    }

    #[test]
    fn deleted_head_branch_is_distinct_from_unborn() {
        let (directory, repository) = initialized_repository();
        let oid = commit_file(&repository, "file.txt", b"one\n", "initial");
        let commit = repository.find_commit(oid).expect("commit");
        repository
            .branch("kept", &commit, false)
            .expect("kept branch");
        repository
            .branch("gone", &commit, false)
            .expect("gone branch");
        repository.set_head("refs/heads/gone").expect("attach gone");
        repository
            .find_reference("refs/heads/gone")
            .expect("gone ref")
            .delete()
            .expect("delete gone");
        drop(commit);
        drop(repository);

        let observation = GitObservation::Repository(repository_observation(directory.path()));
        assert!(matches!(
            match &observation {
                GitObservation::Repository(repository) => &repository.head,
                GitObservation::NoGit { .. } => panic!("expected repository"),
            },
            HeadObservation::MissingTarget { symref } if symref == "refs/heads/gone"
        ));
    }

    #[test]
    fn sequence_and_all_conflict_stages_are_observed() {
        let (directory, repository) = initialized_repository();
        commit_file(&repository, "file.txt", b"base\n", "base");
        drop(repository);

        assert!(
            git(directory.path(), &["config", "user.name", "Atomic Test"])
                .status
                .success()
        );
        assert!(git(
            directory.path(),
            &["config", "user.email", "atomic@example.com"]
        )
        .status
        .success());
        assert!(git(directory.path(), &["switch", "-c", "side"])
            .status
            .success());
        fs::write(directory.path().join("file.txt"), b"side\n").expect("side file");
        assert!(git(directory.path(), &["add", "file.txt"]).status.success());
        assert!(git(directory.path(), &["commit", "-m", "side"])
            .status
            .success());
        assert!(git(directory.path(), &["switch", "main"]).status.success());
        fs::write(directory.path().join("file.txt"), b"main\n").expect("main file");
        assert!(git(directory.path(), &["add", "file.txt"]).status.success());
        assert!(git(directory.path(), &["commit", "-m", "main"])
            .status
            .success());
        assert!(!git(directory.path(), &["merge", "side"]).status.success());

        let before = snapshot_tree(directory.path());
        let observation = repository_observation(directory.path());
        let stages = observation
            .index
            .entries
            .iter()
            .map(|entry| entry.stage)
            .collect::<BTreeSet<_>>();
        assert!(stages.is_superset(&BTreeSet::from([1, 2, 3])));
        assert_eq!(observation.index.tree_oid, None);
        assert!(matches!(
            &observation.index.tree_availability,
            IndexTreeAvailability::Unavailable(IndexTreeUnavailable::NonZeroStages { stages })
                if stages == &vec![1, 2, 3]
        ));
        assert!(observation.operation.is_in_progress());
        assert!(observation
            .operation
            .present_markers()
            .contains(&OperationMarkerKind::MergeHead));
        assert_eq!(before, snapshot_tree(directory.path()));
    }

    #[test]
    fn rebase_cherry_pick_revert_and_bisect_markers_are_reported() {
        let (directory, repository) = initialized_repository();
        let oid = commit_file(&repository, "file.txt", b"one\n", "initial");
        let git_dir = repository.path().to_path_buf();
        drop(repository);
        fs::create_dir(git_dir.join("rebase-merge")).expect("rebase marker");
        fs::write(git_dir.join("REBASE_HEAD"), format!("{oid}\n")).expect("rebase head");
        fs::write(git_dir.join("CHERRY_PICK_HEAD"), format!("{oid}\n")).expect("cherry-pick head");
        fs::write(git_dir.join("REVERT_HEAD"), format!("{oid}\n")).expect("revert head");
        fs::write(git_dir.join("BISECT_LOG"), b"git bisect start\n").expect("bisect log");
        let before = snapshot_tree(directory.path());

        let observation = repository_observation(directory.path());
        let present = observation.operation.present_markers();
        assert!(present.contains(&OperationMarkerKind::RebaseMerge));
        assert!(present.contains(&OperationMarkerKind::RebaseHead));
        assert!(present.contains(&OperationMarkerKind::CherryPickHead));
        assert!(present.contains(&OperationMarkerKind::RevertHead));
        assert!(present.contains(&OperationMarkerKind::BisectLog));
        assert_eq!(before, snapshot_tree(directory.path()));
    }

    /// A standalone `AUTO_MERGE` is advisory merge-ort output, not proof of an
    /// active operation; it is reported but never fences on its own. A
    /// `MERGE_HEAD` (with or without AUTO_MERGE) is authoritative.
    #[test]
    fn standalone_auto_merge_marker_is_advisory_not_in_progress() {
        let marker = |marker, kind| OperationMarkerObservation {
            marker,
            path: PathBuf::from(format!(".git/{}", marker.relative_path())),
            kind,
        };

        let advisory = OperationObservation {
            repository_state: "Clean".to_string(),
            markers: vec![marker(OperationMarkerKind::AutoMerge, AdminEntryKind::File)],
        };
        assert!(!advisory.is_in_progress());
        assert!(advisory.active_markers().is_empty());
        assert_eq!(
            advisory.present_markers(),
            vec![OperationMarkerKind::AutoMerge]
        );

        let active = OperationObservation {
            repository_state: "Clean".to_string(),
            markers: vec![
                marker(OperationMarkerKind::MergeHead, AdminEntryKind::File),
                marker(OperationMarkerKind::AutoMerge, AdminEntryKind::File),
            ],
        };
        assert!(active.is_in_progress());
        assert_eq!(
            active.active_markers(),
            vec![OperationMarkerKind::MergeHead]
        );
        assert_eq!(active.present_markers().len(), 2);

        let non_clean = OperationObservation {
            repository_state: "Merge".to_string(),
            markers: Vec::new(),
        };
        assert!(non_clean.is_in_progress());
    }

    /// A real repository with only a standalone `AUTO_MERGE` ref is observed as
    /// not-in-progress, still reports the marker, and is left unchanged.
    #[test]
    fn standalone_auto_merge_ref_in_real_repository_is_advisory() {
        let (directory, repository) = initialized_repository();
        let _oid = commit_file(&repository, "file.txt", b"one\n", "initial");
        let git_dir = repository.path().to_path_buf();
        drop(repository);
        fs::write(git_dir.join("AUTO_MERGE"), b"one-tree-oid\n").expect("auto merge ref");
        let before = snapshot_tree(directory.path());

        let observation = repository_observation(directory.path());
        assert!(
            !observation.operation.is_in_progress(),
            "a standalone AUTO_MERGE must not read as an active operation"
        );
        assert!(observation
            .operation
            .present_markers()
            .contains(&OperationMarkerKind::AutoMerge));
        assert!(observation.operation.active_markers().is_empty());
        assert_eq!(before, snapshot_tree(directory.path()), "observation is read-only");
    }

    #[test]
    fn index_and_ref_locks_are_reported() {
        let (directory, repository) = initialized_repository();
        commit_file(&repository, "file.txt", b"one\n", "initial");
        let git_dir = repository.path().to_path_buf();
        drop(repository);
        fs::write(git_dir.join("index.lock"), b"").expect("index lock");
        fs::write(git_dir.join("refs/heads/main.lock"), b"").expect("ref lock");

        let observation = repository_observation(directory.path());
        assert!(observation.locks.index_lock.is_present());
        assert!(observation
            .locks
            .ref_locks
            .iter()
            .any(|path| path.ends_with("refs/heads/main.lock")));
    }

    #[test]
    fn wip_refs_are_observed_but_excluded_from_canonical_digest() {
        let (directory, repository) = initialized_repository();
        let head = commit_file(&repository, "file.txt", b"one\n", "initial");
        let before = repository_observation(directory.path()).refs_digest;
        let ref_name = "refs/atomic/wip/workspace/operation";

        repository
            .reference(ref_name, head, false, "create recovery ref")
            .expect("create WIP ref");
        let with_wip = repository_observation(directory.path());
        assert_eq!(with_wip.refs_digest, before);
        assert!(with_wip
            .refs
            .iter()
            .any(|reference| reference.name == ref_name.as_bytes()));

        let replacement = repository.blob(b"replacement").expect("replacement object");
        repository
            .reference(ref_name, replacement, true, "move test recovery ref")
            .expect("move WIP ref for digest test");
        let moved_wip = repository_observation(directory.path());
        assert_eq!(moved_wip.refs_digest, before);
        assert!(moved_wip.refs.iter().any(|reference| {
            reference.name == ref_name.as_bytes()
                && reference.target == RefTargetObservation::Direct(replacement)
        }));
    }

    #[test]
    fn no_git_is_explicit_and_non_mutating() {
        let directory = TempDir::new().expect("temp directory");
        fs::write(directory.path().join("plain.txt"), b"plain\n").expect("plain file");
        let before = snapshot_tree(directory.path());
        let observation = observe_git(directory.path()).expect("observe no Git");
        assert!(matches!(observation, GitObservation::NoGit { .. }));
        assert_eq!(before, snapshot_tree(directory.path()));
    }

    #[test]
    fn linked_worktree_paths_resolve_git_file_common_dir_and_index() {
        let (directory, repository) = initialized_repository();
        commit_file(&repository, "file.txt", b"one\n", "initial");
        drop(repository);
        let linked_parent = TempDir::new().expect("linked parent");
        let linked = linked_parent.path().join("linked");
        let output = Command::new("git")
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg("linked")
            .arg(&linked)
            .current_dir(directory.path())
            .output()
            .expect("add linked worktree");
        assert!(
            output.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(linked.join(".git").is_file());

        let observation = repository_observation(&linked);
        assert_eq!(
            observation.paths.worktree_root,
            Some(fs::canonicalize(&linked).expect("canonical linked"))
        );
        assert_ne!(
            observation.paths.worktree_git_dir,
            observation.paths.common_dir
        );
        assert!(observation
            .paths
            .index_path
            .starts_with(&observation.paths.worktree_git_dir));
    }
}

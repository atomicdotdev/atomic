//! Shared, non-recursive stale-baseline guard for working-copy command boundaries.
//!
//! Command wiring is intentionally delivered by later CB-0C tasks.
#![allow(dead_code)]
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::checkpoint::{
    self, BridgeCheckpoint, CheckpointAtomicState, CheckpointError, GitAdminIdentity,
    ManifestRootEvidence,
};
use super::observation::{
    display_git_bytes, observe_git, GitObservation, HeadObservation, ObservationError,
    RefTargetObservation,
};
use super::wip::{capture_or_reuse_tracked_wip, WipCapture, WipCaptureError, WipCaptureRequest};

/// A command boundary whose relationship to the shared filesystem is explicit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardOperation {
    Status,
    Diff,
    Record,
    Add,
    Materialize,
    ViewSwitch,
    AgentTurnEnd,
    AgentSessionEnd,
    /// Read-only forensic status intentionally observes drift instead of being
    /// blocked by it.
    ForensicStatus,
    /// A historical diff does not interpret the working copy.
    HistoryOnlyDiff,
    /// Bridge bootstrap/reconciliation is the remediation path and must not
    /// recursively guard itself.
    BridgeBootstrap,
}

impl GuardOperation {
    pub(crate) fn is_bypass(self) -> bool {
        matches!(
            self,
            Self::ForensicStatus | Self::HistoryOnlyDiff | Self::BridgeBootstrap
        )
    }
}

impl std::fmt::Display for GuardOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Status => "status",
            Self::Diff => "diff",
            Self::Record => "record",
            Self::Add => "add",
            Self::Materialize => "materialize",
            Self::ViewSwitch => "view switch",
            Self::AgentTurnEnd => "agent turn-end",
            Self::AgentSessionEnd => "agent session-end",
            Self::ForensicStatus => "status --no-reconcile",
            Self::HistoryOnlyDiff => "history-only diff",
            Self::BridgeBootstrap => "bridge bootstrap/reconcile",
        })
    }
}

/// Optional protection to perform only after the guard has decided to refuse.
#[derive(Clone, Copy, Debug)]
pub(crate) enum WipPolicy<'a> {
    None,
    CaptureOrReuse {
        workspace: &'a str,
        operation_id: &'a str,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GuardRequest<'a> {
    pub root: &'a Path,
    pub operation: GuardOperation,
    pub wip: WipPolicy<'a>,
}

impl<'a> GuardRequest<'a> {
    pub(crate) fn new(root: &'a Path, operation: GuardOperation) -> Self {
        Self {
            root,
            operation,
            wip: WipPolicy::None,
        }
    }

    pub(crate) fn with_wip(mut self, workspace: &'a str, operation_id: &'a str) -> Self {
        self.wip = WipPolicy::CaptureOrReuse {
            workspace,
            operation_id,
        };
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AtomicGuardEvidence {
    pub view: String,
    pub state: String,
    pub manifest_root: Option<ManifestRootEvidence>,
}

impl From<CheckpointAtomicState> for AtomicGuardEvidence {
    fn from(value: CheckpointAtomicState) -> Self {
        Self {
            view: value.view,
            state: value.state,
            manifest_root: value.manifest_root,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GitHeadEvidence {
    Attached { symref: String, oid: String },
    Detached { oid: String },
    Unborn { symref: String },
    MissingTarget { symref: String },
    Unknown { oid: Option<String> },
}

impl GitHeadEvidence {
    fn symref(&self) -> Option<&str> {
        match self {
            Self::Attached { symref, .. }
            | Self::Unborn { symref }
            | Self::MissingTarget { symref } => Some(symref),
            Self::Detached { .. } | Self::Unknown { .. } => None,
        }
    }

    fn oid(&self) -> Option<&str> {
        match self {
            Self::Attached { oid, .. } | Self::Detached { oid } => Some(oid),
            Self::Unknown { oid } => oid.as_deref(),
            Self::Unborn { .. } | Self::MissingTarget { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitRefEvidence {
    pub name: Vec<u8>,
    pub target: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitIndexStageEvidence {
    pub path: Vec<u8>,
    pub stage: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitGuardEvidence {
    pub head: GitHeadEvidence,
    pub head_tree: Option<String>,
    pub index_tree: Option<String>,
    pub index_digest: Option<String>,
    pub refs_digest: Option<String>,
    pub refs: Vec<GitRefEvidence>,
    pub admin: Option<GitAdminIdentity>,
    pub index_lock: Option<PathBuf>,
    pub ref_locks: Vec<PathBuf>,
    pub operation_state: String,
    pub operation_markers: Vec<String>,
    pub conflict_stages: Vec<GitIndexStageEvidence>,
    pub manifest_root: Option<ManifestRootEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GuardGitEvidence {
    NoGit { root: PathBuf },
    Repository(Box<GitGuardEvidence>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GuardEvidence {
    pub atomic: AtomicGuardEvidence,
    pub git: GuardGitEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardPassReason {
    Bypass,
    NoGit,
    Unchanged,
    AtomicAdvanced,
    EquivalentView,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GuardPass {
    pub operation: GuardOperation,
    pub reason: GuardPassReason,
    pub old: Option<GuardEvidence>,
    pub current: Option<GuardEvidence>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardRefusalReason {
    CheckpointMissing,
    StaleGitBaseline,
    Diverged,
    DetachedHead,
    UnbornHead,
    MissingHeadTarget,
    IndexLocked,
    RefLocked,
    GitOperationInProgress,
    ConflictStages,
    IndexTreeUnavailable,
    AdminIdentityChanged,
    AtomicEvidenceChanged,
}

impl std::fmt::Display for GuardRefusalReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::CheckpointMissing => "no bridge checkpoint establishes the filesystem baseline",
            Self::StaleGitBaseline => "Git moved away from the bridge checkpoint",
            Self::Diverged => "both Git and Atomic moved away from the bridge checkpoint",
            Self::DetachedHead => "Git HEAD is detached",
            Self::UnbornHead => "Git HEAD is unborn",
            Self::MissingHeadTarget => "Git HEAD points to a missing branch target",
            Self::IndexLocked => "the Git index is locked",
            Self::RefLocked => "one or more Git refs are locked",
            Self::GitOperationInProgress => "a Git sequence operation is in progress",
            Self::ConflictStages => "the Git index contains conflict stages",
            Self::IndexTreeUnavailable => "the Git index has no supported read-only tree identity",
            Self::AdminIdentityChanged => "the Git worktree administrative identity changed",
            Self::AtomicEvidenceChanged => {
                "the same Atomic state produced different provisional graph evidence"
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum DriftField {
    Checkpoint,
    AtomicView,
    AtomicState,
    AtomicManifestRoot,
    GitHeadState,
    GitHeadSymref,
    GitHeadOid,
    GitHeadTree,
    GitIndexTree,
    GitIndexLock,
    GitRefLock,
    GitOperation,
    GitConflictStages,
    GitAdminIdentity,
}

impl std::fmt::Display for DriftField {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Checkpoint => "checkpoint",
            Self::AtomicView => "atomic-view",
            Self::AtomicState => "atomic-state",
            Self::AtomicManifestRoot => "atomic-manifest-root",
            Self::GitHeadState => "git-head-state",
            Self::GitHeadSymref => "git-head-symref",
            Self::GitHeadOid => "git-head-oid",
            Self::GitHeadTree => "git-head-tree",
            Self::GitIndexTree => "git-index-tree",
            Self::GitIndexLock => "git-index-lock",
            Self::GitRefLock => "git-ref-lock",
            Self::GitOperation => "git-operation",
            Self::GitConflictStages => "git-conflict-stages",
            Self::GitAdminIdentity => "git-admin-identity",
        })
    }
}

/// Concrete, state-specific recovery guidance. `commands` contains only
/// commands that are safe to name for the observed state; `summary` says when
/// user choice is still required.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GuardRemediation {
    pub summary: String,
    pub commands: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GuardRefusal {
    pub operation: GuardOperation,
    pub reason: GuardRefusalReason,
    pub drift_fields: Vec<DriftField>,
    pub old: Option<GuardEvidence>,
    pub current: GuardEvidence,
    pub remediation: GuardRemediation,
    pub recovery: Option<WipCapture>,
}

impl std::fmt::Display for GuardRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(formatter, "Unsafe operation: {}", self.operation)?;
        writeln!(formatter, "Refusal: {}", self.reason)?;
        write!(formatter, "Drift fields:")?;
        for field in &self.drift_fields {
            write!(formatter, " {field}")?;
        }
        writeln!(formatter)?;
        if let Some(old) = &self.old {
            write_evidence(formatter, "Old checkpoint", old)?;
        } else {
            writeln!(formatter, "Old checkpoint: absent")?;
        }
        write_evidence(formatter, "Current", &self.current)?;
        if let Some(recovery) = &self.recovery {
            writeln!(
                formatter,
                "Recovery ref: {} (tree {})",
                recovery.ref_name, recovery.tree_oid
            )?;
        }
        writeln!(formatter, "Remediation: {}", self.remediation.summary)?;
        for command in &self.remediation.commands {
            writeln!(formatter, "  {command}")?;
        }
        Ok(())
    }
}

fn write_evidence(
    formatter: &mut std::fmt::Formatter<'_>,
    label: &str,
    evidence: &GuardEvidence,
) -> std::fmt::Result {
    writeln!(formatter, "{label} Atomic view: {}", evidence.atomic.view)?;
    writeln!(formatter, "{label} Atomic state: {}", evidence.atomic.state)?;
    writeln!(
        formatter,
        "{label} Atomic manifest root: {}",
        display_root(evidence.atomic.manifest_root.as_ref())
    )?;
    match &evidence.git {
        GuardGitEvidence::NoGit { root } => {
            writeln!(formatter, "{label} Git: none ({})", root.display())?;
        }
        GuardGitEvidence::Repository(git) => {
            writeln!(formatter, "{label} Git HEAD: {}", display_head(&git.head))?;
            writeln!(
                formatter,
                "{label} Git HEAD tree: {}",
                git.head_tree.as_deref().unwrap_or("unavailable")
            )?;
            writeln!(
                formatter,
                "{label} Git index tree: {}",
                git.index_tree.as_deref().unwrap_or("unavailable")
            )?;
            writeln!(
                formatter,
                "{label} Git index digest: {}",
                git.index_digest.as_deref().unwrap_or("unavailable")
            )?;
            writeln!(
                formatter,
                "{label} Git refs digest: {}",
                git.refs_digest.as_deref().unwrap_or("unavailable")
            )?;
            if git.refs.is_empty() {
                writeln!(
                    formatter,
                    "{label} Git refs: digest-only checkpoint evidence"
                )?;
            } else {
                writeln!(formatter, "{label} Git refs: {}", git.refs.len())?;
                for reference in &git.refs {
                    writeln!(
                        formatter,
                        "  {} -> {}",
                        display_git_bytes(&reference.name),
                        reference.target
                    )?;
                }
            }
            if let Some(admin) = &git.admin {
                writeln!(
                    formatter,
                    "{label} Git admin: worktree={} git-dir={} common-dir={} index={}",
                    admin
                        .worktree_root
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "none".to_string()),
                    admin.worktree_git_dir.display(),
                    admin.common_dir.display(),
                    admin.index_path.display()
                )?;
            } else {
                writeln!(formatter, "{label} Git admin: unavailable")?;
            }
            writeln!(
                formatter,
                "{label} Git manifest root: {}",
                display_root(git.manifest_root.as_ref())
            )?;
        }
    }
    Ok(())
}

fn display_root(root: Option<&ManifestRootEvidence>) -> String {
    root.map(ToString::to_string)
        .unwrap_or_else(|| "unavailable".to_string())
}

fn display_head(head: &GitHeadEvidence) -> String {
    match head {
        GitHeadEvidence::Attached { symref, oid } => format!("attached {symref} at {oid}"),
        GitHeadEvidence::Detached { oid } => format!("detached at {oid}"),
        GitHeadEvidence::Unborn { symref } => format!("unborn {symref}"),
        GitHeadEvidence::MissingTarget { symref } => format!("missing target {symref}"),
        GitHeadEvidence::Unknown { oid } => format!(
            "unknown{}",
            oid.as_ref()
                .map(|oid| format!(" at {oid}"))
                .unwrap_or_default()
        ),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GuardOutcome {
    Pass(Box<GuardPass>),
    Refuse(Box<GuardRefusal>),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum GuardError {
    #[error(transparent)]
    Checkpoint(#[from] CheckpointError),
    #[error(transparent)]
    Observation(#[from] ObservationError),
    #[error(transparent)]
    Wip(#[from] WipCaptureError),
}

/// Run one command-boundary guard without invoking status, materialization, or
/// another guarded command. Atomic and Git handles are consumed while building
/// owned evidence and are dropped before this function returns.
pub(crate) fn guard_working_copy(request: GuardRequest<'_>) -> Result<GuardOutcome, GuardError> {
    if request.operation.is_bypass() {
        return Ok(GuardOutcome::Pass(Box::new(GuardPass {
            operation: request.operation,
            reason: GuardPassReason::Bypass,
            old: None,
            current: None,
        })));
    }

    // `observe_current_atomic` owns and drops its read-only Repository handle.
    let atomic: AtomicGuardEvidence = checkpoint::observe_current_atomic(request.root)?.into();
    // `observe_git` returns owned evidence; its libgit2 handle is local to the observer.
    let repository = match observe_git(request.root)? {
        GitObservation::NoGit { root } => {
            return Ok(GuardOutcome::Pass(Box::new(GuardPass {
                operation: request.operation,
                reason: GuardPassReason::NoGit,
                old: None,
                current: Some(GuardEvidence {
                    atomic,
                    git: GuardGitEvidence::NoGit { root },
                }),
            })));
        }
        GitObservation::Repository(repository) => repository,
    };
    let current = GuardEvidence {
        atomic,
        git: GuardGitEvidence::Repository(Box::new(git_evidence(&repository))),
    };
    let checkpoint = checkpoint::read_checkpoint(request.root)?;
    let mut outcome = classify_guard(request.operation, checkpoint.as_ref(), &current);

    if let (
        GuardOutcome::Refuse(ref mut refusal),
        WipPolicy::CaptureOrReuse {
            workspace,
            operation_id,
        },
    ) = (&mut outcome, request.wip)
    {
        let recovery = capture_or_reuse_tracked_wip(WipCaptureRequest::new(
            request.root,
            workspace,
            operation_id,
        ))?;
        refusal.recovery = Some(recovery);
    }

    Ok(outcome)
}

/// Pure causal-baseline classifier used by every future command boundary.
pub(crate) fn classify_guard(
    operation: GuardOperation,
    checkpoint: Option<&BridgeCheckpoint>,
    current: &GuardEvidence,
) -> GuardOutcome {
    if operation.is_bypass() {
        return GuardOutcome::Pass(Box::new(GuardPass {
            operation,
            reason: GuardPassReason::Bypass,
            old: None,
            current: None,
        }));
    }

    let GuardGitEvidence::Repository(current_git) = &current.git else {
        return GuardOutcome::Pass(Box::new(GuardPass {
            operation,
            reason: GuardPassReason::NoGit,
            old: None,
            current: Some(current.clone()),
        }));
    };
    let old = checkpoint.map(checkpoint_evidence);

    match &current_git.head {
        GitHeadEvidence::Detached { .. } => {
            return refuse(
                operation,
                GuardRefusalReason::DetachedHead,
                vec![DriftField::GitHeadState],
                old,
                current,
            );
        }
        GitHeadEvidence::Unborn { .. } => {
            return refuse(
                operation,
                GuardRefusalReason::UnbornHead,
                vec![DriftField::GitHeadState],
                old,
                current,
            );
        }
        GitHeadEvidence::MissingTarget { .. } => {
            return refuse(
                operation,
                GuardRefusalReason::MissingHeadTarget,
                vec![DriftField::GitHeadState],
                old,
                current,
            );
        }
        GitHeadEvidence::Unknown { .. } => {
            return refuse(
                operation,
                GuardRefusalReason::MissingHeadTarget,
                vec![DriftField::GitHeadState],
                old,
                current,
            );
        }
        GitHeadEvidence::Attached { .. } => {}
    }

    if current_git.index_lock.is_some() {
        return refuse(
            operation,
            GuardRefusalReason::IndexLocked,
            vec![DriftField::GitIndexLock],
            old,
            current,
        );
    }
    if !current_git.ref_locks.is_empty() {
        return refuse(
            operation,
            GuardRefusalReason::RefLocked,
            vec![DriftField::GitRefLock],
            old,
            current,
        );
    }
    if current_git.operation_state != "Clean" || !current_git.operation_markers.is_empty() {
        return refuse(
            operation,
            GuardRefusalReason::GitOperationInProgress,
            vec![DriftField::GitOperation],
            old,
            current,
        );
    }
    if !current_git.conflict_stages.is_empty() {
        return refuse(
            operation,
            GuardRefusalReason::ConflictStages,
            vec![DriftField::GitConflictStages],
            old,
            current,
        );
    }
    if current_git.index_tree.is_none() {
        return refuse(
            operation,
            GuardRefusalReason::IndexTreeUnavailable,
            vec![DriftField::GitIndexTree],
            old,
            current,
        );
    }

    let Some(checkpoint) = checkpoint else {
        return refuse(
            operation,
            GuardRefusalReason::CheckpointMissing,
            vec![DriftField::Checkpoint],
            None,
            current,
        );
    };
    let old_evidence = checkpoint_evidence(checkpoint);
    let GuardGitEvidence::Repository(old_git) = &old_evidence.git else {
        unreachable!("checkpoint evidence is always a Git repository")
    };

    let mut git_drift = Vec::new();
    if old_git.head.symref() != current_git.head.symref() {
        git_drift.push(DriftField::GitHeadSymref);
    }
    if old_git.head.oid() != current_git.head.oid() {
        git_drift.push(DriftField::GitHeadOid);
    }
    if old_git.head_tree != current_git.head_tree {
        git_drift.push(DriftField::GitHeadTree);
    }
    if old_git.admin.is_some() && old_git.admin != current_git.admin {
        git_drift.push(DriftField::GitAdminIdentity);
    }

    let atomic_state_changed = old_evidence.atomic.state != current.atomic.state;
    let atomic_view_changed = old_evidence.atomic.view != current.atomic.view;
    let atomic_root_changed_without_state = !atomic_state_changed
        && matches!(
            (
                old_evidence.atomic.manifest_root.as_ref(),
                current.atomic.manifest_root.as_ref()
            ),
            (Some(old), Some(current))
                if old.kind == current.kind
                    && old.provisional == current.provisional
                    && old.value != current.value
        );

    if atomic_root_changed_without_state {
        return refuse(
            operation,
            GuardRefusalReason::AtomicEvidenceChanged,
            vec![DriftField::AtomicManifestRoot],
            Some(old_evidence),
            current,
        );
    }

    if !git_drift.is_empty() {
        if atomic_view_changed {
            git_drift.push(DriftField::AtomicView);
        }
        if atomic_state_changed {
            git_drift.push(DriftField::AtomicState);
        }
        git_drift.sort();
        git_drift.dedup();
        return refuse(
            operation,
            if atomic_state_changed {
                GuardRefusalReason::Diverged
            } else {
                GuardRefusalReason::StaleGitBaseline
            },
            git_drift,
            Some(old_evidence),
            current,
        );
    }

    GuardOutcome::Pass(Box::new(GuardPass {
        operation,
        reason: if atomic_state_changed {
            GuardPassReason::AtomicAdvanced
        } else if atomic_view_changed {
            GuardPassReason::EquivalentView
        } else {
            GuardPassReason::Unchanged
        },
        old: Some(old_evidence),
        current: Some(current.clone()),
    }))
}

fn checkpoint_evidence(checkpoint: &BridgeCheckpoint) -> GuardEvidence {
    let head = match (&checkpoint.git_head_symref, checkpoint.git_head.is_empty()) {
        (Some(symref), false) => GitHeadEvidence::Attached {
            symref: symref.clone(),
            oid: checkpoint.git_head.clone(),
        },
        (_, false) => GitHeadEvidence::Unknown {
            oid: Some(checkpoint.git_head.clone()),
        },
        _ => GitHeadEvidence::Unknown { oid: None },
    };
    GuardEvidence {
        atomic: AtomicGuardEvidence {
            view: checkpoint.view.clone(),
            state: checkpoint.atomic_state.clone(),
            manifest_root: checkpoint.atomic_manifest_root.clone(),
        },
        git: GuardGitEvidence::Repository(Box::new(GitGuardEvidence {
            head,
            head_tree: Some(checkpoint.git_tree.clone()),
            index_tree: checkpoint.git_index_tree.clone(),
            index_digest: checkpoint.git_index_digest.clone(),
            refs_digest: checkpoint.git_refs_digest.clone(),
            refs: Vec::new(),
            admin: checkpoint.git_admin.clone(),
            index_lock: None,
            ref_locks: Vec::new(),
            operation_state: "Clean".to_string(),
            operation_markers: Vec::new(),
            conflict_stages: Vec::new(),
            manifest_root: checkpoint.git_manifest_root.clone(),
        })),
    }
}

fn git_evidence(repository: &super::observation::GitRepositoryObservation) -> GitGuardEvidence {
    let head = match &repository.head {
        HeadObservation::Attached { symref, oid } => GitHeadEvidence::Attached {
            symref: symref.clone(),
            oid: oid.to_string(),
        },
        HeadObservation::Detached { oid } => GitHeadEvidence::Detached {
            oid: oid.to_string(),
        },
        HeadObservation::Unborn { symref } => GitHeadEvidence::Unborn {
            symref: symref.clone(),
        },
        HeadObservation::MissingTarget { symref } => GitHeadEvidence::MissingTarget {
            symref: symref.clone(),
        },
    };
    let head_tree = repository.head_tree_oid.map(|oid| oid.to_string());
    GitGuardEvidence {
        head,
        head_tree: head_tree.clone(),
        index_tree: repository.index.tree_oid.map(|oid| oid.to_string()),
        index_digest: Some(repository.index.canonical_digest.0.clone()),
        refs_digest: Some(repository.refs_digest.0.clone()),
        refs: repository
            .refs
            .iter()
            .map(|reference| GitRefEvidence {
                name: reference.name.clone(),
                target: match &reference.target {
                    RefTargetObservation::Direct(oid) => oid.to_string(),
                    RefTargetObservation::Symbolic(target) => {
                        format!("symref {}", display_git_bytes(target))
                    }
                    RefTargetObservation::Unresolved => "unresolved".to_string(),
                },
            })
            .collect(),
        admin: Some(GitAdminIdentity {
            worktree_root: repository.paths.worktree_root.clone(),
            worktree_git_dir: repository.paths.worktree_git_dir.clone(),
            common_dir: repository.paths.common_dir.clone(),
            index_path: repository.paths.index_path.clone(),
        }),
        index_lock: repository
            .locks
            .index_lock
            .is_present()
            .then(|| repository.locks.index_lock.path.clone()),
        ref_locks: repository.locks.ref_locks.clone(),
        operation_state: repository.operation.repository_state.clone(),
        operation_markers: repository
            .operation
            .markers
            .iter()
            .filter(|marker| marker.is_present())
            .map(|marker| marker.marker.as_str().to_string())
            .collect(),
        conflict_stages: repository
            .index
            .entries
            .iter()
            .filter(|entry| entry.stage != 0)
            .map(|entry| GitIndexStageEvidence {
                path: entry.path.clone(),
                stage: entry.stage,
            })
            .collect(),
        manifest_root: head_tree.map(ManifestRootEvidence::git_tree),
    }
}

fn refuse(
    operation: GuardOperation,
    reason: GuardRefusalReason,
    drift_fields: Vec<DriftField>,
    old: Option<GuardEvidence>,
    current: &GuardEvidence,
) -> GuardOutcome {
    let remediation = remediation(reason, old.as_ref(), current);
    GuardOutcome::Refuse(Box::new(GuardRefusal {
        operation,
        reason,
        drift_fields,
        old,
        current: current.clone(),
        remediation,
        recovery: None,
    }))
}

fn remediation(
    reason: GuardRefusalReason,
    old: Option<&GuardEvidence>,
    current: &GuardEvidence,
) -> GuardRemediation {
    let forensic = "atomic status --no-reconcile".to_string();
    let reconcile = "atomic git bridge reconcile".to_string();
    match reason {
        GuardRefusalReason::CheckpointMissing | GuardRefusalReason::StaleGitBaseline => {
            GuardRemediation {
                summary: "Reconcile the observed Git baseline before retrying the refused operation."
                    .to_string(),
                commands: vec![forensic, reconcile],
            }
        }
        GuardRefusalReason::Diverged => GuardRemediation {
            summary: "No automatic authority choice is safe. Inspect both preserved histories, choose the intended side explicitly, and only then reconcile."
                .to_string(),
            commands: vec![forensic],
        },
        GuardRefusalReason::DetachedHead => {
            let mut commands = vec![forensic];
            if let Some(branch) = checkpoint_branch(old) {
                commands.push(format!("git switch {branch}"));
                commands.push(reconcile);
            }
            GuardRemediation {
                summary: "Return Git to the checkpoint's attached branch, then reconcile."
                    .to_string(),
                commands,
            }
        }
        GuardRefusalReason::UnbornHead => GuardRemediation {
            summary: "Create the repository's initial Git commit, then establish a bridge checkpoint."
                .to_string(),
            commands: vec![forensic, "git status".to_string(), reconcile],
        },
        GuardRefusalReason::MissingHeadTarget => {
            let mut commands = vec![forensic];
            if let (Some(branch), Some(oid)) = (checkpoint_branch(old), checkpoint_oid(old)) {
                commands.push(format!("git branch --force {branch} {oid}"));
                commands.push(format!("git switch {branch}"));
                commands.push(reconcile);
            }
            GuardRemediation {
                summary: "Restore the missing branch target from the recorded checkpoint, then reconcile."
                    .to_string(),
                commands,
            }
        }
        GuardRefusalReason::IndexLocked | GuardRefusalReason::RefLocked => GuardRemediation {
            summary: "Wait for the active Git process to finish; do not remove a live lock. Re-observe before retrying."
                .to_string(),
            commands: vec!["git status".to_string(), forensic],
        },
        GuardRefusalReason::GitOperationInProgress => GuardRemediation {
            summary: format!(
                "Complete or abort the in-progress Git operation ({}) before reconciliation.",
                current_operation(current)
            ),
            commands: vec!["git status".to_string(), forensic, reconcile],
        },
        GuardRefusalReason::ConflictStages => GuardRemediation {
            summary: "Resolve every Git index conflict stage and finish or abort the Git operation before reconciliation."
                .to_string(),
            commands: vec!["git status".to_string(), "git mergetool".to_string(), forensic, reconcile],
        },
        GuardRefusalReason::IndexTreeUnavailable => GuardRemediation {
            summary: "Inspect the unsupported index evidence; do not reinterpret the working tree until the index denotes one stage-0 tree."
                .to_string(),
            commands: vec!["git status".to_string(), forensic],
        },
        GuardRefusalReason::AdminIdentityChanged => GuardRemediation {
            summary: "Establish a checkpoint for this resolved Git worktree identity before retrying."
                .to_string(),
            commands: vec![forensic, reconcile],
        },
        GuardRefusalReason::AtomicEvidenceChanged => GuardRemediation {
            summary: "Verify graph projection integrity; the same Atomic state must not produce a different provisional root."
                .to_string(),
            commands: vec![forensic, "atomic git bridge verify".to_string()],
        },
    }
}

fn checkpoint_branch(old: Option<&GuardEvidence>) -> Option<String> {
    let GuardGitEvidence::Repository(git) = &old?.git else {
        return None;
    };
    git.head
        .symref()?
        .strip_prefix("refs/heads/")
        .map(str::to_string)
}

fn checkpoint_oid(old: Option<&GuardEvidence>) -> Option<String> {
    let GuardGitEvidence::Repository(git) = &old?.git else {
        return None;
    };
    git.head.oid().map(str::to_string)
}

fn current_operation(current: &GuardEvidence) -> String {
    let GuardGitEvidence::Repository(git) = &current.git else {
        return "unknown".to_string();
    };
    if git.operation_markers.is_empty() {
        git.operation_state.clone()
    } else {
        let mut value = String::new();
        let _ = write!(value, "{}: ", git.operation_state);
        value.push_str(&git.operation_markers.join(", "));
        value
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::super::checkpoint::{BridgeCheckpoint, GitAdminIdentity, CHECKPOINT_VERSION};
    use super::*;

    fn root(value: &str) -> ManifestRootEvidence {
        ManifestRootEvidence {
            kind: super::super::checkpoint::ManifestRootKind::AtomicVisibleRegularFilesBlake3V1,
            value: value.to_string(),
            provisional: true,
        }
    }

    fn checkpoint() -> BridgeCheckpoint {
        BridgeCheckpoint {
            version: CHECKPOINT_VERSION,
            view: "main".to_string(),
            atomic_state: "atomic-1".to_string(),
            atomic_manifest_root: Some(root("root-1")),
            git_head_symref: Some("refs/heads/main".to_string()),
            git_head: "git-1".to_string(),
            git_tree: "tree-1".to_string(),
            git_manifest_root: Some(ManifestRootEvidence::git_tree("tree-1")),
            git_index_tree: Some("tree-1".to_string()),
            git_index_digest: Some("index-1".to_string()),
            git_refs_digest: Some("refs-1".to_string()),
            git_admin: Some(admin()),
        }
    }

    fn admin() -> GitAdminIdentity {
        GitAdminIdentity {
            worktree_root: Some(PathBuf::from("/repo")),
            worktree_git_dir: PathBuf::from("/repo/.git"),
            common_dir: PathBuf::from("/repo/.git"),
            index_path: PathBuf::from("/repo/.git/index"),
        }
    }

    fn current() -> GuardEvidence {
        GuardEvidence {
            atomic: AtomicGuardEvidence {
                view: "main".to_string(),
                state: "atomic-1".to_string(),
                manifest_root: Some(root("root-1")),
            },
            git: GuardGitEvidence::Repository(Box::new(GitGuardEvidence {
                head: GitHeadEvidence::Attached {
                    symref: "refs/heads/main".to_string(),
                    oid: "git-1".to_string(),
                },
                head_tree: Some("tree-1".to_string()),
                index_tree: Some("tree-1".to_string()),
                index_digest: Some("index-1".to_string()),
                refs_digest: Some("refs-1".to_string()),
                refs: Vec::new(),
                admin: Some(admin()),
                index_lock: None,
                ref_locks: Vec::new(),
                operation_state: "Clean".to_string(),
                operation_markers: Vec::new(),
                conflict_stages: Vec::new(),
                manifest_root: Some(ManifestRootEvidence::git_tree("tree-1")),
            })),
        }
    }

    fn refusal(outcome: GuardOutcome) -> Box<GuardRefusal> {
        match outcome {
            GuardOutcome::Refuse(refusal) => refusal,
            GuardOutcome::Pass(pass) => panic!("expected refusal, got {pass:?}"),
        }
    }

    fn pass_reason(outcome: GuardOutcome) -> GuardPassReason {
        match outcome {
            GuardOutcome::Pass(pass) => pass.reason,
            GuardOutcome::Refuse(refusal) => panic!("expected pass, got {refusal:?}"),
        }
    }

    #[test]
    fn pure_matrix_allows_unchanged_atomic_advance_and_equivalent_agent_view() {
        let checkpoint = checkpoint();
        assert_eq!(
            pass_reason(classify_guard(
                GuardOperation::Status,
                Some(&checkpoint),
                &current()
            )),
            GuardPassReason::Unchanged
        );

        let mut atomic_advanced = current();
        atomic_advanced.atomic.state = "atomic-2".to_string();
        atomic_advanced.atomic.manifest_root = Some(root("root-2"));
        assert_eq!(
            pass_reason(classify_guard(
                GuardOperation::Record,
                Some(&checkpoint),
                &atomic_advanced
            )),
            GuardPassReason::AtomicAdvanced
        );

        let mut agent_view = current();
        agent_view.atomic.view = "agent/session-1".to_string();
        assert_eq!(
            pass_reason(classify_guard(
                GuardOperation::AgentTurnEnd,
                Some(&checkpoint),
                &agent_view
            )),
            GuardPassReason::EquivalentView
        );
    }

    #[test]
    fn pure_matrix_refuses_git_symref_oid_tree_and_true_divergence() {
        let checkpoint = checkpoint();
        for (field, mutate) in [
            (DriftField::GitHeadSymref, 0_u8),
            (DriftField::GitHeadOid, 1_u8),
            (DriftField::GitHeadTree, 2_u8),
        ] {
            let mut evidence = current();
            let GuardGitEvidence::Repository(git) = &mut evidence.git else {
                unreachable!()
            };
            match mutate {
                0 => {
                    git.head = GitHeadEvidence::Attached {
                        symref: "refs/heads/topic".to_string(),
                        oid: "git-1".to_string(),
                    }
                }
                1 => {
                    git.head = GitHeadEvidence::Attached {
                        symref: "refs/heads/main".to_string(),
                        oid: "git-2".to_string(),
                    }
                }
                2 => {
                    git.head_tree = Some("tree-2".to_string());
                    git.manifest_root = Some(ManifestRootEvidence::git_tree("tree-2"));
                }
                _ => unreachable!(),
            }
            let refusal = refusal(classify_guard(
                GuardOperation::Status,
                Some(&checkpoint),
                &evidence,
            ));
            assert_eq!(refusal.reason, GuardRefusalReason::StaleGitBaseline);
            assert!(refusal.drift_fields.contains(&field));
            assert!(refusal
                .remediation
                .commands
                .iter()
                .any(|command| command == "atomic git bridge reconcile"));
        }

        let mut diverged = current();
        diverged.atomic.state = "atomic-2".to_string();
        let GuardGitEvidence::Repository(git) = &mut diverged.git else {
            unreachable!()
        };
        git.head = GitHeadEvidence::Attached {
            symref: "refs/heads/main".to_string(),
            oid: "git-2".to_string(),
        };
        let refusal = refusal(classify_guard(
            GuardOperation::Record,
            Some(&checkpoint),
            &diverged,
        ));
        assert_eq!(refusal.reason, GuardRefusalReason::Diverged);
        assert_eq!(
            refusal.remediation.commands,
            vec!["atomic status --no-reconcile"]
        );
    }

    #[test]
    fn unsupported_git_states_refuse_before_baseline_comparison() {
        let checkpoint = checkpoint();
        let cases = [
            (
                GuardRefusalReason::DetachedHead,
                GitHeadEvidence::Detached {
                    oid: "git-1".to_string(),
                },
            ),
            (
                GuardRefusalReason::UnbornHead,
                GitHeadEvidence::Unborn {
                    symref: "refs/heads/main".to_string(),
                },
            ),
            (
                GuardRefusalReason::MissingHeadTarget,
                GitHeadEvidence::MissingTarget {
                    symref: "refs/heads/main".to_string(),
                },
            ),
        ];
        for (expected, head) in cases {
            let mut evidence = current();
            let GuardGitEvidence::Repository(git) = &mut evidence.git else {
                unreachable!()
            };
            git.head = head;
            assert_eq!(
                refusal(classify_guard(
                    GuardOperation::Materialize,
                    Some(&checkpoint),
                    &evidence
                ))
                .reason,
                expected
            );
        }

        let mut locked = current();
        let GuardGitEvidence::Repository(git) = &mut locked.git else {
            unreachable!()
        };
        git.index_lock = Some(PathBuf::from("/repo/.git/index.lock"));
        assert_eq!(
            refusal(classify_guard(
                GuardOperation::Add,
                Some(&checkpoint),
                &locked
            ))
            .reason,
            GuardRefusalReason::IndexLocked
        );

        let mut operation = current();
        let GuardGitEvidence::Repository(git) = &mut operation.git else {
            unreachable!()
        };
        git.operation_state = "Merge".to_string();
        git.operation_markers.push("merge-head".to_string());
        assert_eq!(
            refusal(classify_guard(
                GuardOperation::ViewSwitch,
                Some(&checkpoint),
                &operation
            ))
            .reason,
            GuardRefusalReason::GitOperationInProgress
        );

        let mut conflicted = current();
        let GuardGitEvidence::Repository(git) = &mut conflicted.git else {
            unreachable!()
        };
        git.conflict_stages.push(GitIndexStageEvidence {
            path: b"src/lib.rs".to_vec(),
            stage: 2,
        });
        assert_eq!(
            refusal(classify_guard(
                GuardOperation::AgentSessionEnd,
                Some(&checkpoint),
                &conflicted
            ))
            .reason,
            GuardRefusalReason::ConflictStages
        );
    }

    #[test]
    fn no_git_and_explicit_bypasses_pass() {
        let mut no_git = current();
        no_git.git = GuardGitEvidence::NoGit {
            root: PathBuf::from("/repo"),
        };
        assert_eq!(
            pass_reason(classify_guard(GuardOperation::Status, None, &no_git)),
            GuardPassReason::NoGit
        );
        for operation in [
            GuardOperation::ForensicStatus,
            GuardOperation::HistoryOnlyDiff,
            GuardOperation::BridgeBootstrap,
        ] {
            assert_eq!(
                pass_reason(classify_guard(operation, None, &current())),
                GuardPassReason::Bypass
            );
        }
    }

    #[test]
    fn refusal_report_contains_states_refs_roots_operation_and_remediation() {
        let checkpoint = checkpoint();
        let mut evidence = current();
        let GuardGitEvidence::Repository(git) = &mut evidence.git else {
            unreachable!()
        };
        git.head_tree = Some("tree-2".to_string());
        let report = refusal(classify_guard(
            GuardOperation::Record,
            Some(&checkpoint),
            &evidence,
        ))
        .to_string();
        for required in [
            "Unsafe operation: record",
            "Old checkpoint Atomic state: atomic-1",
            "Current Atomic state: atomic-1",
            "Git refs digest: refs-1",
            "Atomic manifest root:",
            "Git manifest root:",
            "atomic git bridge reconcile",
        ] {
            assert!(report.contains(required), "missing {required:?}:\n{report}");
        }
    }

    #[test]
    fn no_git_guard_drops_repository_handle_before_return() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = atomic_repository::Repository::init(root.path()).expect("init Atomic");
        drop(repo);

        let outcome = guard_working_copy(GuardRequest::new(root.path(), GuardOperation::Status))
            .expect("guard no-Git repository");
        assert_eq!(pass_reason(outcome), GuardPassReason::NoGit);

        let reopened = atomic_repository::Repository::open_existing(root.path())
            .expect("guard must release the repository handle");
        drop(reopened);
    }

    fn git_ok(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("run Git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn initialized_colocated_repository() -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = atomic_repository::Repository::init_with_view(root.path(), "main")
            .expect("init Atomic");
        drop(repo);
        git_ok(root.path(), &["init", "-q"]);
        git_ok(root.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git_ok(root.path(), &["config", "user.name", "Atomic Test"]);
        git_ok(root.path(), &["config", "user.email", "atomic@example.com"]);
        fs::write(root.path().join("tracked.txt"), b"one\n").expect("write tracked");
        git_ok(root.path(), &["add", "tracked.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "initial"]);

        let head = git_ok(root.path(), &["rev-parse", "HEAD"]);
        let tree = git_ok(root.path(), &["rev-parse", "HEAD^{tree}"]);
        let atomic_state = {
            let repo =
                atomic_repository::Repository::open_readonly(root.path()).expect("open Atomic");
            let state = repo
                .get_view_info("main")
                .expect("main view")
                .state
                .to_string();
            drop(repo);
            state
        };
        super::super::checkpoint::write_verified_checkpoint(
            root.path(),
            super::super::checkpoint::VerifiedCheckpointInput {
                view: "main",
                atomic_state: &atomic_state,
                git_head: &head,
                git_tree: &tree,
            },
        )
        .expect("write checkpoint");
        root
    }

    #[test]
    fn ordinary_edit_passes_and_drift_recovery_is_reused() {
        let root = initialized_colocated_repository();
        fs::write(root.path().join("tracked.txt"), b"ordinary edit\n").expect("edit tracked");
        assert_eq!(
            pass_reason(
                guard_working_copy(GuardRequest::new(root.path(), GuardOperation::Status))
                    .expect("ordinary edit guard")
            ),
            GuardPassReason::Unchanged
        );

        git_ok(root.path(), &["add", "tracked.txt"]);
        git_ok(root.path(), &["commit", "-q", "-m", "Git advance"]);
        fs::write(root.path().join("tracked.txt"), b"post-checkout work\n")
            .expect("write drifted work");
        let request = GuardRequest::new(root.path(), GuardOperation::AgentTurnEnd)
            .with_wip("run-1", "turn-1");
        let first = refusal(guard_working_copy(request).expect("first refusal"));
        let retry = refusal(guard_working_copy(request).expect("retry refusal"));
        let first_recovery = first.recovery.expect("first recovery");
        let retry_recovery = retry.recovery.expect("retry recovery");
        assert_eq!(retry_recovery, first_recovery);

        let reopened = atomic_repository::Repository::open_existing(root.path())
            .expect("guard and WIP capture must release repository handles");
        drop(reopened);
    }
}

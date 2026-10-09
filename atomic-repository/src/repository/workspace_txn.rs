//! Ordered, mode-aware workspace transaction entry protocol.

use std::fs;
use std::path::{Path, PathBuf};

use atomic_core::operation::OperationScope;
use atomic_core::pristine::{ViewState, ViewTxnT, WorkingCopyRecord, WorkingCopyTxnT};
use atomic_core::{OperationId, WorkingCopyId};
use serde::Deserialize;

use super::git_observation::{
    observe_git_metadata, GitHeadObservation, GitObservationToken, GitOperationMarker,
    WorkspaceGitObservation,
};
use super::locks::WorkingCopyOperationLockGuard;
use super::{OperationHeadState, Repository};
use crate::RepositoryError;

/// Maximum number of stable-observation attempts at one transaction boundary.
pub const MAX_WORKSPACE_TXN_ATTEMPTS: u8 = 3;
/// A workspace entry plan always contains exactly these three ordered phases.
pub const MAX_WORKSPACE_ENTRY_PLAN_ITEMS: usize = 3;
pub(crate) const CHECKPOINT_RELATIVE_PATH: &str = ".atomic/bridge/workspace.json";

/// Mutation policy for a workspace transaction boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceTxnMode {
    /// Reconcile safe drift and refuse unsafe states.
    Reconcile,
    /// Report state without domain mutation.
    Observe,
    /// Permit explicit repair, except while Git owns a sequence operation.
    Force,
}

/// The enforced RFC floor for the reactive-reconcile silence window
/// (RFC §11.2 rule 3: at least 250 ms). Single source of truth is the
/// bridge watch configuration; this alias keeps the transaction-side
/// precondition readable without a magic number.
pub const MIN_REACTIVE_QUIESCENCE_MS: u64 = atomic_config::BridgeWatchConfig::MIN_QUIET_MS;

/// Effect budget a workspace-transaction caller granted itself (CB-13D,
/// RFC §11.2 rule 4).
///
/// The optional bridge watch daemon reuses the shared workspace transaction
/// path but carries a metadata-only budget: it may create
/// `ImportGitHead`/`ImportGitRefs` operations and adopt bookkeeping, but it
/// must never materialize working-copy files and must never move Git refs.
/// A command boundary holds the full [`ReconcileEffectBudget::Command`]
/// budget and is unaffected.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReconcileEffectBudget {
    /// Full command budget: metadata, filesystem, and ref effects as the
    /// transaction mode and its leases allow.
    #[default]
    Command,
    /// Metadata-only budget (the bridge watch daemon). Export-classified
    /// reconciliation surfaces a notice/remediation instead of executing.
    MetadataOnly,
}

impl ReconcileEffectBudget {
    /// Whether an Atomic→Git projection (export) may execute under this
    /// budget. Only a command boundary may move a mapped ref under its
    /// expected-old lease; the watch daemon is refused.
    pub fn allows_projection(self) -> bool {
        matches!(self, Self::Command)
    }

    /// Whether a filesystem materialization may execute under this budget.
    /// Only a command boundary may write working-copy bytes; the watch
    /// daemon adopts bookkeeping only.
    pub fn allows_materialization(self) -> bool {
        matches!(self, Self::Command)
    }

    /// Whether this is the metadata-only reactive budget. A metadata-only
    /// boundary must refuse every effect-bearing step *before* it runs:
    /// writable-open recovery, bound HEAD adoption (shelf planner/executor,
    /// WIP capture, ref deletion), and import bootstrap/materialization
    /// are all outside its grant (CB-13D review R1).
    pub fn is_metadata_only(self) -> bool {
        matches!(self, Self::MetadataOnly)
    }
}

/// Whether the observed Git state is quiescent enough for a reactive
/// reconcile (RFC §11.2 rule 3): no Git index lock, no Git administrative
/// locks (refs, HEAD, packed-refs, reftable, top-level), and no Git-owned
/// sequence markers. The evaluation is a precondition, not a wait — the
/// caller supplies the silence window. Enumeration is fail-closed: an
/// unreadable Git administrative directory reports `false` (busy) rather
/// than silently passing.
pub fn git_state_quiescent(observation: &super::git_observation::WorkspaceGitObservation) -> bool {
    match observation {
        super::git_observation::WorkspaceGitObservation::NoGit { .. } => true,
        super::git_observation::WorkspaceGitObservation::Repository(repository) => {
            !repository.index_lock.is_present()
                && !repository.operation.is_in_progress()
                && match git_locks_present(&repository.common_dir, &repository.worktree_git_dir) {
                    // Fail closed: an uninspectable admin directory is busy.
                    Ok(locks) => locks.is_empty(),
                    Err(_) => false,
                }
        }
    }
}

/// Every Git administrative lock that fences a reactive reconcile (CB-13D
/// review R2): `refs/**` under both the worktree and common Git
/// directories, `packed-refs.lock`, `HEAD.lock`, reftable locks
/// (`reftable/**` and `reftable.lock`), and any top-level `*.lock` file in
/// either administrative directory (e.g. `config.lock`, `gc.log.lock`).
/// The enumeration covers both backends by construction (a files-backed
/// repository has no `reftable/` directory; a reftable-backed one has no
/// loose refs to lock) and is **fail-closed**: a directory that cannot be
/// inspected is an error, never a silent skip. Returned sorted for stable
/// reporting; empty means no Git-owned transaction is in flight.
/// CB-13D ::24 R2 namespace-completeness proof: WHICH namespaces the
/// quiet-lock observation inspected and what the symlink policy skipped.
/// An observation that cannot prove its namespace is not a lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LockScanProof {
    /// The namespaces walked, in stable order.
    pub inspected: Vec<&'static str>,
    /// Symlinked directories skipped (recorded policy: never recurse into
    /// a symlinked directory — a cyclic or external target must not narrow
    /// the inspected namespace silently).
    pub skipped_symlink_dirs: Vec<String>,
}

impl LockScanProof {
    /// The complete namespace list an observation must have covered.
    pub fn is_complete(&self) -> bool {
        let required = [
            "worktree-head-lock",
            "worktree-packed-refs-lock",
            "worktree-reftable",
            "worktree-refs",
            "worktree-top-level",
            "common-head-lock",
            "common-packed-refs-lock",
            "common-reftable",
            "common-refs",
            "common-top-level",
        ];
        required.iter().all(|ns| self.inspected.contains(ns))
    }
}

pub fn git_locks_present(
    common_dir: &Path,
    worktree_git_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let (locks, _proof) = git_locks_present_proved(common_dir, worktree_git_dir)?;
    Ok(locks)
}

/// The quiet-lock enumeration with its namespace proof (CB-13D ::24 R2):
/// every walked namespace and every symlinked directory the policy skipped
/// is recorded, so an observation can prove the namespace it inspected.
pub fn git_locks_present_proved(
    common_dir: &Path,
    worktree_git_dir: &Path,
) -> Result<(Vec<PathBuf>, LockScanProof), String> {
    let mut locks = Vec::new();
    let mut proof = LockScanProof {
        inspected: Vec::new(),
        skipped_symlink_dirs: Vec::new(),
    };
    for (label, admin_dir) in [("worktree", worktree_git_dir), ("common", common_dir)] {
        let head_ns = format!("{label}-head-lock");
        let head_ns: &'static str = Box::leak(head_ns.into_boxed_str());
        let packed_ns = format!("{label}-packed-refs-lock");
        let packed_ns: &'static str = Box::leak(packed_ns.into_boxed_str());
        let reftable_ns = format!("{label}-reftable");
        let reftable_ns: &'static str = Box::leak(reftable_ns.into_boxed_str());
        let refs_ns = format!("{label}-refs");
        let refs_ns: &'static str = Box::leak(refs_ns.into_boxed_str());
        let top_ns = format!("{label}-top-level");
        let top_ns: &'static str = Box::leak(top_ns.into_boxed_str());
        // HEAD.lock fences every ref update that moves HEAD (attach,
        // detach, reset, checkout) — the check-to-entry race fence.
        proof.inspected.push(head_ns);
        if admin_dir.join("HEAD.lock").is_file() {
            locks.push(admin_dir.join("HEAD.lock"));
        }
        proof.inspected.push(packed_ns);
        if admin_dir.join("packed-refs.lock").is_file() {
            locks.push(admin_dir.join("packed-refs.lock"));
        }
        // Reftable backend: lock files under reftable/ plus the reftable
        // root lock itself.
        proof.inspected.push(reftable_ns);
        collect_lock_files_proved(&admin_dir.join("reftable"), &mut locks, 0, &mut proof)?;
        if admin_dir.join("reftable.lock").is_file() {
            locks.push(admin_dir.join("reftable.lock"));
        }
        // Loose refs (files backend).
        proof.inspected.push(refs_ns);
        collect_lock_files_proved(&admin_dir.join("refs"), &mut locks, 0, &mut proof)?;
        // Top-level administrative locks (config.lock, gc.log.lock, …).
        proof.inspected.push(top_ns);
        collect_top_level_locks(admin_dir, &mut locks)?;
    }
    locks.sort();
    locks.dedup();
    Ok((locks, proof))
}

/// The recursive lock collector recording the namespaces it walked and the
/// symlinked directories the policy skipped (CB-13D ::24 R2).
fn collect_lock_files_proved(
    directory: &Path,
    locks: &mut Vec<PathBuf>,
    depth: u8,
    proof: &mut LockScanProof,
) -> Result<(), String> {
    if depth > 16 {
        return Err(format!(
            "cannot inspect Git directory '{}' beyond the depth bound",
            directory.display()
        ));
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot inspect Git administrative directory '{}': {error}",
                directory.display()
            ))
        }
    };
    // CB-13D ::24 R2: per-entry I/O failures propagate fail-closed
    // instead of being silently flattened away — an unreadable entry must
    // fail the observation, not narrow the inspected namespace silently.
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "cannot inspect Git administrative directory '{}': {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        // Do not follow symlinks: a symlinked directory could recurse
        // cyclically; a symlinked lock file itself is still reported via
        // the is_file check below on the direct entry. CB-13D ::24 R2:
        // the skip is RECORDED, not silent.
        if entry.path().is_symlink() && path.is_dir() {
            proof.skipped_symlink_dirs.push(path.display().to_string());
            continue;
        }
        if path.is_dir() {
            collect_lock_files_proved(&path, locks, depth + 1, proof)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "lock")
        {
            locks.push(path);
        }
    }
    Ok(())
}

/// Collect `*.lock` files directly inside one Git administrative directory
/// (non-recursive: subdirectories have their own scoped enumeration).
fn collect_top_level_locks(directory: &Path, locks: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot inspect Git administrative directory '{}': {error}",
                directory.display()
            ))
        }
    };
    // CB-13D ::24 R2: per-entry I/O failures propagate fail-closed.
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "cannot inspect Git administrative directory '{}': {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "lock")
        {
            locks.push(path);
        }
    }
    Ok(())
}

/// Quiescence verdict for a reactive reconcile attempt (CB-13D).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitQuiescence {
    /// No Git-owned transaction is in flight; after the configured silence
    /// window a reactive reconcile may enter the shared transaction path.
    /// The namespace proof records WHICH namespaces were inspected (an
    /// observation that cannot prove its namespace is not a lease,
    /// CB-13D ::24 R2).
    Quiescent {
        /// The namespace-completeness proof of this observation.
        proof: LockScanProof,
    },
    /// A Git-owned transaction (lock or sequence state) is in flight; the
    /// watcher must wait and surface, never repair.
    Busy {
        /// Stable machine-readable reason: `index_lock`, `ref_locks`, or
        /// `sequence_markers`.
        reason: &'static str,
        /// Human-readable detail for the notice surface.
        detail: String,
    },
}

impl GitQuiescence {
    /// Evaluate one Git metadata observation against the reactive
    /// preconditions (RFC §11.2 rule 3). Sequence markers of any kind are
    /// busy: Git owns that transaction and the bridge does not model
    /// half-applied Git state (RFC §7.4).
    pub fn evaluate(observation: &super::git_observation::WorkspaceGitObservation) -> Self {
        match observation {
            super::git_observation::WorkspaceGitObservation::NoGit { .. } => Self::Quiescent {
                proof: LockScanProof {
                    inspected: Vec::new(),
                    skipped_symlink_dirs: Vec::new(),
                },
            },
            super::git_observation::WorkspaceGitObservation::Repository(repository) => {
                if repository.index_lock.is_present() {
                    return Self::Busy {
                        reason: "index_lock",
                        detail: format!(
                            "Git index lock present at '{}'",
                            repository.index_path.display()
                        ),
                    };
                }
                // Fail closed: an administrative directory that cannot be
                // inspected is busy, never silently quiescent (review R2).
                let (locks, proof) = match git_locks_present_proved(
                    &repository.common_dir,
                    &repository.worktree_git_dir,
                ) {
                    Ok((locks, proof)) => (locks, proof),
                    Err(detail) => {
                        return Self::Busy {
                            reason: "lock_enumeration_failed",
                            detail,
                        }
                    }
                };
                if !locks.is_empty() {
                    return Self::Busy {
                        reason: "ref_locks",
                        detail: format!(
                            "Git ref transaction in flight ({} lock file(s)); inspected                              namespaces {:?}, skipped symlink dirs {:?}",
                            locks.len(),
                            proof.inspected,
                            proof.skipped_symlink_dirs
                        ),
                    };
                }
                if repository.operation.is_in_progress() {
                    let markers: Vec<String> = repository
                        .operation
                        .present_markers()
                        .into_iter()
                        .map(|marker| marker.to_string())
                        .collect();
                    return Self::Busy {
                        reason: "sequence_markers",
                        detail: format!("Git-owned sequence operation in progress ({markers:?})"),
                    };
                }
                Self::Quiescent { proof }
            }
        }
    }
}

/// Outcome of one CB-7A §7.3 bound HEAD adoption attempt at the entry
/// boundary.
enum HeadAdoptionAttempt {
    /// The workspace advanced to the adopted baseline; the caller re-loads
    /// authority and re-classifies.
    Adopted,
    /// Operation heads diverged; adoption never crossed them.
    Diverged { heads: Vec<OperationId> },
    /// Adoption refused for a typed reason (logged); the caller keeps the
    /// head remediation it already classified.
    Refused,
}

/// Result of entering a workspace transaction boundary.
#[allow(clippy::large_enum_variant)] // Ready carries the live workspace handle
pub enum WorkspaceTxnStart {
    /// The observed workspace is stable and ready for command-specific work.
    Ready(WorkspaceTxn),
    /// The workspace is validly observed but requires a typed corrective action.
    Remediation(WorkspaceRemediation),
}

impl std::fmt::Debug for WorkspaceTxnStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready(_) => f.write_str("WorkspaceTxnStart::Ready(..)"),
            Self::Remediation(remediation) => {
                write!(f, "WorkspaceTxnStart::Remediation({remediation:?})")
            }
        }
    }
}

/// A stable workspace boundary retaining the canonical operation locks.
pub struct WorkspaceTxn {
    _operation_lock: WorkingCopyOperationLockGuard,
    mode: WorkspaceTxnMode,
    attempts: u8,
    record: WorkingCopyRecord,
    view: ViewState,
    operation_heads: OperationHeadState,
    checkpoint: Option<WorkspaceCheckpoint>,
    git: WorkspaceGitObservation,
    plan: WorkspaceEntryPlan,
    budget: ReconcileEffectBudget,
}

impl WorkspaceTxn {
    pub fn mode(&self) -> WorkspaceTxnMode {
        self.mode
    }

    /// The effect budget this boundary was entered with (CB-13D).
    pub fn budget(&self) -> ReconcileEffectBudget {
        self.budget
    }

    pub fn attempts(&self) -> u8 {
        self.attempts
    }

    pub fn working_copy(&self) -> WorkingCopyId {
        self.record.id
    }

    pub fn working_copy_record(&self) -> &WorkingCopyRecord {
        &self.record
    }

    pub fn view(&self) -> &ViewState {
        &self.view
    }

    pub fn operation_heads(&self) -> &OperationHeadState {
        &self.operation_heads
    }

    pub fn checkpoint(&self) -> Option<&WorkspaceCheckpoint> {
        self.checkpoint.as_ref()
    }

    pub fn git(&self) -> &WorkspaceGitObservation {
        &self.git
    }

    pub fn plan(&self) -> &WorkspaceEntryPlan {
        &self.plan
    }

    /// The ordered operation lock this transaction holds.
    ///
    /// Journaled repository mutations executed inside the transaction reuse
    /// this guard instead of acquiring a second lock set, preserving the
    /// common → working-copy → pristine → final-resource order.
    pub(super) fn operation_lock(&self) -> &WorkingCopyOperationLockGuard {
        &self._operation_lock
    }

    /// A same-thread alias of the held common operation lock (CB-10A review
    /// R6): journaled repository bookkeeping inside this workspace
    /// transaction borrows the boundary's locks instead of contending a
    /// fresh acquisition. Dropping the alias only decrements the nesting
    /// count.
    pub fn common_lock_alias(&self) -> crate::repository::locks::RepositoryCommonLockGuard {
        self._operation_lock.common_alias()
    }
}

/// Fixed-size entry phases. Their field order is the execution order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceEntryPlan {
    head: WorkspaceHeadPlan,
    filesystem: WorkspaceFilesystemPlan,
    refs: WorkspaceRefPlan,
}

impl WorkspaceEntryPlan {
    fn aligned() -> Self {
        Self {
            head: WorkspaceHeadPlan::Aligned,
            filesystem: WorkspaceFilesystemPlan::ObserveBaseline,
            refs: WorkspaceRefPlan::ObserveMapped,
        }
    }

    fn blocked(checkpoint: Option<String>, observed: GitHeadObservation) -> Self {
        Self {
            head: WorkspaceHeadPlan::ReconcileBeforeFilesystem {
                checkpoint,
                observed,
            },
            filesystem: WorkspaceFilesystemPlan::BlockedUntilHeadAligned,
            refs: WorkspaceRefPlan::Deferred,
        }
    }

    pub fn head(&self) -> &WorkspaceHeadPlan {
        &self.head
    }

    pub fn filesystem(&self) -> WorkspaceFilesystemPlan {
        self.filesystem
    }

    pub fn refs(&self) -> WorkspaceRefPlan {
        self.refs
    }

    pub fn item_count(&self) -> usize {
        MAX_WORKSPACE_ENTRY_PLAN_ITEMS
    }

    pub fn is_ordered(&self) -> bool {
        matches!(
            (&self.head, &self.filesystem),
            (
                WorkspaceHeadPlan::Aligned,
                WorkspaceFilesystemPlan::ObserveBaseline
            ) | (
                WorkspaceHeadPlan::ReconcileBeforeFilesystem { .. },
                WorkspaceFilesystemPlan::BlockedUntilHeadAligned
            )
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceHeadPlan {
    Aligned,
    ReconcileBeforeFilesystem {
        checkpoint: Option<String>,
        observed: GitHeadObservation,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceFilesystemPlan {
    ObserveBaseline,
    BlockedUntilHeadAligned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceRefPlan {
    ObserveMapped,
    Deferred,
}

impl WorkspaceTxnMode {
    /// Stable observability code for this mode (RFC §13 Phase 13 task 5).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Observe => "observe",
            Self::Force => "force",
        }
    }
}

/// Mode-independent corrective information returned instead of unsafe mutation.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum WorkspaceRemediation {
    GitOperationInProgress {
        mode: WorkspaceTxnMode,
        repository_state: String,
        markers: Vec<GitOperationMarker>,
        conflict_stages: Vec<u8>,
        disposition: GitOperationDisposition,
    },
    Unanchored {
        mode: WorkspaceTxnMode,
        state: UnanchoredWorkspace,
        plan: WorkspaceEntryPlan,
    },
    OperationHeadsDiverged {
        heads: Vec<OperationId>,
    },
    ConcurrentGitMutation {
        attempts: u8,
        first: GitObservationToken,
        last: GitObservationToken,
    },
    /// A Git administrative lock (HEAD.lock, packed-refs.lock, refs/**,
    /// reftable) appeared under the retained entry lease between the
    /// quiescence observation and the effect boundary (review R2). The
    /// boundary refused instead of reconciling across the in-flight Git
    /// transaction.
    GitLocksBusy {
        reason: &'static str,
        detail: String,
    },
}

impl WorkspaceRemediation {
    /// Whether this remediation describes a Git HEAD movement the CB-7A §7.3
    /// bound adoption may reconcile before the filesystem phase. Unborn and
    /// missing HEAD targets never adopt; they keep their typed refusals.
    pub fn is_head_candidate(&self) -> bool {
        matches!(
            self,
            WorkspaceRemediation::Unanchored {
                state: UnanchoredWorkspace::HeadChanged { .. }
                    | UnanchoredWorkspace::HeadSymrefChanged { .. }
                    | UnanchoredWorkspace::DetachedHead { .. },
                ..
            }
        )
    }

    /// Stable observability class for this remediation (RFC §13 Phase 13
    /// task 5). Codes are fixed; they never carry repository paths or
    /// observation payloads.
    pub fn code(&self) -> &'static str {
        match self {
            Self::GitOperationInProgress { .. } => "git_operation_in_progress",
            Self::Unanchored { .. } => "unanchored",
            Self::OperationHeadsDiverged { .. } => "operation_heads_diverged",
            Self::ConcurrentGitMutation { .. } => "concurrent_git_mutation",
            Self::GitLocksBusy { .. } => "git_locks_busy",
        }
    }

    /// Stable observability code for the transaction mode this remediation
    /// was produced under, when the variant carries one.
    pub fn mode_code(&self) -> &'static str {
        match self {
            Self::GitOperationInProgress { mode, .. } | Self::Unanchored { mode, .. } => {
                mode.as_str()
            }
            Self::OperationHeadsDiverged { .. }
            | Self::ConcurrentGitMutation { .. }
            | Self::GitLocksBusy { .. } => "unknown",
        }
    }

    /// One-line informative description for Observe output and typed refusals.
    ///
    /// This is the safe-to-display summary; it never includes workspace
    /// contents, only classification evidence.
    pub fn describe(&self) -> String {
        match self {
            Self::GitOperationInProgress {
                repository_state,
                markers,
                conflict_stages,
                disposition,
                ..
            } => {
                let mut description = format!(
                    "Git operation in progress ({repository_state}): {}",
                    disposition.describe()
                );
                if !markers.is_empty() {
                    let names: Vec<String> =
                        markers.iter().map(|marker| marker.to_string()).collect();
                    description.push_str(&format!("; sequence markers: {}", names.join(", ")));
                }
                if !conflict_stages.is_empty() {
                    description.push_str(&format!(
                        "; index holds {} conflicted stage entries",
                        conflict_stages.len()
                    ));
                }
                description
            }
            Self::Unanchored { state, .. } => {
                let mut description =
                    format!("workspace is not anchored to a verified Git baseline: {state:?}");
                if let Some(remediation) = state.remediation() {
                    description.push_str("\n\n");
                    description.push_str(&remediation);
                }
                description
            }
            Self::OperationHeadsDiverged { heads } => format!(
                "operation heads diverged; resolve the competing leases instead of retrying a stale plan: {}",
                heads.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ),
            Self::ConcurrentGitMutation {
                attempts,
                first,
                last,
            } => format!(
                "Git HEAD/index changed between observations across {attempts} attempt(s); \
                 aborting on a possibly stale baseline (first token {first:?}, last token {last:?})"
            ),
            Self::GitLocksBusy { reason, detail } => {
                format!("Git-owned transaction in flight ({reason}): {detail}")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitOperationDisposition {
    ObserveOnly,
    FinishOrAbortInGit,
    ForceForbidden,
}

impl GitOperationDisposition {
    /// Human-readable remediation instruction for the disposition.
    pub fn describe(self) -> &'static str {
        match self {
            Self::ObserveOnly => {
                "read-only observation only: no command body may run while Git owns this operation"
            }
            Self::FinishOrAbortInGit => {
                "finish or abort the Git operation in Git before retrying this command"
            }
            Self::ForceForbidden => {
                "explicit repair is forbidden: no mode may bypass a Git-owned operation"
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnanchoredWorkspace {
    DetachedHead {
        oid: String,
    },
    UnbornHead {
        symref: String,
    },
    MissingHeadTarget {
        symref: String,
    },
    MissingCheckpoint,
    GitRepositoryMissing,
    AtomicCheckpointDrift {
        checkpoint_view: String,
        checkpoint_state: String,
        desired_view: String,
        desired_state: String,
    },
    HeadSymrefChanged {
        checkpoint: String,
        observed: String,
    },
    HeadTreeChanged {
        checkpoint: String,
        observed: String,
    },
    IndexTreeChanged {
        checkpoint: String,
        observed: Option<String>,
    },
    HeadChanged {
        checkpoint: String,
        observed: String,
    },
    IndexLocked {
        path: PathBuf,
    },
    /// CB-13D ::24 R2: a Git-owned transaction lock appeared between the
    /// caller's quiescence check and the workspace entry (the late-lock
    /// race); the reactive boundary defers instead of running beside it.
    GitBusy {
        reason: String,
        detail: String,
    },
}

impl UnanchoredWorkspace {
    /// Commands that resolve this state when a single safe path exists.
    ///
    /// Ordinary boundaries only observe Git for bridge workspaces
    /// ([`Repository::bridge_workspace_active`]), so these states are reached
    /// after an explicit opt-in or anchoring.
    pub fn remediation(&self) -> Option<String> {
        const DISABLE: &str = "To use Atomic without the Git bridge in a workspace that was never \
anchored:\n  atomic git bridge disable";
        match self {
            Self::MissingCheckpoint => Some(format!(
                "The Git bridge is enabled for this repository, but this workspace has no \
verified Git baseline yet. Anchor it to the current Git HEAD (imports the Git \
history into the current view):\n  atomic git bridge reconcile\n{DISABLE}"
            )),
            Self::UnbornHead { symref } => Some(format!(
                "{symref} has no commits yet, so there is no Git baseline to anchor to. \
Create the first Git commit (or switch to a branch that has commits), then \
anchor the workspace:\n  git add <paths> && git commit\n  atomic git bridge reconcile\n{DISABLE}"
            )),
            _ => None,
        }
    }
}

/// Checkpoint fields needed by the entry protocol. Reading never rewrites legacy data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceCheckpoint {
    pub version: u32,
    pub view: String,
    pub atomic_state: String,
    pub git_head_symref: Option<String>,
    pub git_head: String,
    pub git_tree: String,
    pub git_index_tree: Option<String>,
    pub git_index_digest: Option<String>,
}

#[derive(Deserialize, serde::Serialize)]
struct CheckpointWire {
    version: u32,
    view: String,
    atomic_state: String,
    #[serde(default)]
    git_head_symref: Option<String>,
    git_head: String,
    git_tree: String,
    #[serde(default)]
    git_index_tree: Option<String>,
    #[serde(default)]
    git_index_digest: Option<String>,
}

impl Repository {
    /// Enter the shared workspace boundary while retaining its ordered locks.
    pub fn begin_workspace_txn(
        &mut self,
        mode: WorkspaceTxnMode,
    ) -> Result<WorkspaceTxnStart, RepositoryError> {
        self.begin_workspace_txn_budgeted(mode, ReconcileEffectBudget::Command)
    }

    /// Enter the shared workspace boundary under an explicit effect budget
    /// (CB-13D). The metadata-only budget refuses every effect-bearing entry
    /// step — pending recovery work and bound HEAD adoption — before any of
    /// it executes, and revalidates Git quiescence under the retained lease
    /// right before the boundary reports ready (review R1/R2).
    pub fn begin_workspace_txn_budgeted(
        &mut self,
        mode: WorkspaceTxnMode,
        budget: ReconcileEffectBudget,
    ) -> Result<WorkspaceTxnStart, RepositoryError> {
        // RFC §2 scopes the bridge to explicitly configured colocated
        // workspaces: Git metadata of a working copy that never enrolled is
        // not interpreted, so ordinary commands keep native behavior instead
        // of refusing for a Git anchor the user never asked for.
        let observe: fn(&Path) -> Result<WorkspaceGitObservation, super::ObservationError> =
            if self.bridge_workspace_active()? {
                observe_git_metadata
            } else {
                observe_native_workspace
            };
        self.begin_workspace_txn_with_opt(mode, observe, false, budget)
    }

    /// Whether this working copy participates in the colocated Git bridge.
    ///
    /// The bridge is explicit per-repository opt-in (RFC §2; CB-13C
    /// `[git.bridge] enabled`, default `false`): a `.git` directory next to
    /// `.atomic` is not consent. A working copy participates once the user
    /// recorded the opt-in (`atomic git bridge enable`) or an explicit bridge
    /// command (`git bridge reconcile`, `git import`, anchoring, clone
    /// bootstrap / `--adopt-git`) wrote the verified checkpoint that the
    /// stale-baseline guard protects. The explicit repair boundary
    /// ([`Self::begin_remediation_txn`]) always observes Git.
    pub fn bridge_workspace_active(&self) -> Result<bool, RepositoryError> {
        if read_workspace_checkpoint(self.root())?.is_some() {
            return Ok(true);
        }
        let config = atomic_config::RepoConfig::load(&self.dot_dir().join("config.toml")).map_err(
            |error| RepositoryError::InvalidRepository {
                reason: format!("cannot load the repository configuration: {error}"),
            },
        )?;
        Ok(config.git.bridge.enabled)
    }

    /// Enter the explicit repair boundary used by the bridge remediation path.
    ///
    /// This is the only entry that may proceed while the workspace is
    /// unanchored (missing/stale checkpoint, moved HEAD/index) — repairing that
    /// state is its purpose. It still refuses, in every case:
    /// - a Git-owned sequence operation or conflicted index (no mode bypasses
    ///   Git-owned state),
    /// - a held Git index lock,
    /// - diverged operation heads.
    ///
    /// All other workspace boundaries must use [`Self::begin_workspace_txn`].
    pub fn begin_remediation_txn(&mut self) -> Result<WorkspaceTxnStart, RepositoryError> {
        self.begin_workspace_txn_with_opt(
            WorkspaceTxnMode::Force,
            observe_git_metadata,
            true,
            ReconcileEffectBudget::Command,
        )
    }

    /// The repair boundary under an explicit effect budget (CB-13D).
    pub fn begin_remediation_txn_budgeted(
        &mut self,
        budget: ReconcileEffectBudget,
    ) -> Result<WorkspaceTxnStart, RepositoryError> {
        self.begin_workspace_txn_with_opt(
            WorkspaceTxnMode::Force,
            observe_git_metadata,
            true,
            budget,
        )
    }

    /// Test hook mirroring [`Self::begin_remediation_txn`] with observation
    /// injection.
    #[cfg(test)]
    fn begin_remediation_txn_with<F>(
        &mut self,
        observe: F,
    ) -> Result<WorkspaceTxnStart, RepositoryError>
    where
        F: FnMut(&Path) -> Result<WorkspaceGitObservation, super::ObservationError>,
    {
        self.begin_workspace_txn_with_opt(
            WorkspaceTxnMode::Force,
            observe,
            true,
            ReconcileEffectBudget::Command,
        )
    }

    /// Observation-injectable entry. Private in normal builds; test fixtures
    /// use the `pub(crate)` twin below.
    #[cfg(test)]
    pub(crate) fn begin_workspace_txn_with<F>(
        &mut self,
        mode: WorkspaceTxnMode,
        observe: F,
    ) -> Result<WorkspaceTxnStart, RepositoryError>
    where
        F: FnMut(&Path) -> Result<WorkspaceGitObservation, super::ObservationError>,
    {
        self.begin_workspace_txn_with_opt(mode, observe, false, ReconcileEffectBudget::Command)
    }

    /// Test hook with observation injection under an explicit budget.
    #[cfg(test)]
    #[allow(dead_code)] // seam kept for the next budget consumer
    pub(crate) fn begin_workspace_txn_budgeted_with<F>(
        &mut self,
        mode: WorkspaceTxnMode,
        observe: F,
        budget: ReconcileEffectBudget,
    ) -> Result<WorkspaceTxnStart, RepositoryError>
    where
        F: FnMut(&Path) -> Result<WorkspaceGitObservation, super::ObservationError>,
    {
        self.begin_workspace_txn_with_opt(mode, observe, false, budget)
    }

    /// Test hook mirroring the budgeted repair boundary with observation
    /// injection.
    #[cfg(test)]
    pub(crate) fn begin_remediation_txn_budgeted_with<F>(
        &mut self,
        observe: F,
        budget: ReconcileEffectBudget,
    ) -> Result<WorkspaceTxnStart, RepositoryError>
    where
        F: FnMut(&Path) -> Result<WorkspaceGitObservation, super::ObservationError>,
    {
        self.begin_workspace_txn_with_opt(WorkspaceTxnMode::Force, observe, true, budget)
    }

    fn begin_workspace_txn_with_opt<F>(
        &mut self,
        mode: WorkspaceTxnMode,
        observe: F,
        tolerate_unanchored: bool,
        budget: ReconcileEffectBudget,
    ) -> Result<WorkspaceTxnStart, RepositoryError>
    where
        F: FnMut(&Path) -> Result<WorkspaceGitObservation, super::ObservationError>,
    {
        // RFC Phase 0 (CB-0C): a guarded entry refuses before mutating
        // anything, including telemetry — refused entries are observable
        // through their typed diagnostic only. Structured events are
        // recorded by mutating boundaries (e.g. the remediation reconcile
        // path) instead.
        self.begin_workspace_txn_with_opt_inner(mode, observe, tolerate_unanchored, budget)
    }

    fn begin_workspace_txn_with_opt_inner<F>(
        &mut self,
        mode: WorkspaceTxnMode,
        mut observe: F,
        tolerate_unanchored: bool,
        budget: ReconcileEffectBudget,
    ) -> Result<WorkspaceTxnStart, RepositoryError>
    where
        F: FnMut(&Path) -> Result<WorkspaceGitObservation, super::ObservationError>,
    {
        let working_copy = self.require_working_copy_id()?;
        let operation_lock = self.try_lock_workspace_operation(working_copy)?;
        // CB-13D ::24 R2 late-lock race fence: a Git lock created between
        // the caller's quiescence check and this entry is caught HERE —
        // under the metadata-only budget the quiet-observation lease is
        // re-proved inside the entry (the namespace proof re-scanned), so
        // a late ref/index lock fails busy instead of letting the pass run
        // beside a Git-owned transaction.
        if budget.is_metadata_only() {
            let initial_observation = observe(self.root()).map_err(observation_error)?;
            if let GitQuiescence::Busy { reason, detail } =
                GitQuiescence::evaluate(&initial_observation)
            {
                return Ok(WorkspaceTxnStart::Remediation(
                    WorkspaceRemediation::Unanchored {
                        mode,
                        state: UnanchoredWorkspace::GitBusy {
                            reason: reason.to_string(),
                            detail,
                        },
                        plan: WorkspaceEntryPlan::blocked(
                            None,
                            super::git_observation::GitHeadObservation::Unborn {
                                symref: "refs/heads/unknown".to_string(),
                            },
                        ),
                    },
                ));
            }
        }
        let (mut record, mut view) = self.load_workspace_authority(working_copy)?;
        let observed_operation_heads = self
            .operation_log(OperationScope::WorkingCopy(working_copy), Some(0), false)?
            .head_state;
        if mode == WorkspaceTxnMode::Observe {
            if let OperationHeadState::Diverged(heads) = &observed_operation_heads {
                return Ok(WorkspaceTxnStart::Remediation(
                    WorkspaceRemediation::OperationHeadsDiverged {
                        heads: heads.clone(),
                    },
                ));
            }
        }

        let mut checkpoint = read_workspace_checkpoint(self.root())?;
        let mut first_changed = None;
        let mut last_changed = None;

        for attempt in 1..=MAX_WORKSPACE_TXN_ATTEMPTS {
            let initial = observe(self.root()).map_err(observation_error)?;
            if let Some(remediation) =
                classify_git_state(mode, &record, &view, checkpoint.as_ref(), &initial)
            {
                // CB-7A §7.3: a changed or detached HEAD is reconciled before
                // the filesystem phase (§12.2), but only in mutating modes and
                // never while the repair boundary has already accepted the
                // unanchored baseline. Adoption failure keeps today's typed
                // remediation; adoption success continues the entry loop
                // against the rewritten checkpoint.
                if mode != WorkspaceTxnMode::Observe && remediation.is_head_candidate() {
                    match self.attempt_head_adoption(
                        &operation_lock,
                        working_copy,
                        checkpoint.as_ref(),
                        &initial,
                        budget,
                    )? {
                        HeadAdoptionAttempt::Adopted => {
                            checkpoint = read_workspace_checkpoint(self.root())?;
                            (record, view) = self.load_workspace_authority(working_copy)?;
                            continue;
                        }
                        HeadAdoptionAttempt::Diverged { heads } => {
                            return Ok(WorkspaceTxnStart::Remediation(
                                WorkspaceRemediation::OperationHeadsDiverged { heads },
                            ));
                        }
                        HeadAdoptionAttempt::Refused => {}
                    }
                }
                if !tolerable_remediation_for_repair(&remediation, tolerate_unanchored) {
                    return Ok(WorkspaceTxnStart::Remediation(remediation));
                }
                // Repair-tolerated drift falls through to the aligned plan so
                // the remediation command body can resolve it.
            }

            let plan = WorkspaceEntryPlan::aligned();
            debug_assert!(plan.is_ordered());
            debug_assert!(plan.item_count() <= MAX_WORKSPACE_ENTRY_PLAN_ITEMS);

            let final_observation = observe(self.root()).map_err(observation_error)?;
            let initial_token = initial.token();
            let final_token = final_observation.token();
            if initial_token != final_token {
                if first_changed.is_none() {
                    first_changed = Some(initial_token);
                }
                last_changed = Some(final_token);
                continue;
            }
            if let Some(remediation) = classify_git_state(
                mode,
                &record,
                &view,
                checkpoint.as_ref(),
                &final_observation,
            ) {
                // CB-7A §7.3: a HEAD change appearing between the two entry
                // observations is a race; reconcile it exactly like the
                // initial-site candidate above.
                if mode != WorkspaceTxnMode::Observe && remediation.is_head_candidate() {
                    match self.attempt_head_adoption(
                        &operation_lock,
                        working_copy,
                        checkpoint.as_ref(),
                        &final_observation,
                        budget,
                    )? {
                        HeadAdoptionAttempt::Adopted => {
                            checkpoint = read_workspace_checkpoint(self.root())?;
                            (record, view) = self.load_workspace_authority(working_copy)?;
                            continue;
                        }
                        HeadAdoptionAttempt::Diverged { heads } => {
                            return Ok(WorkspaceTxnStart::Remediation(
                                WorkspaceRemediation::OperationHeadsDiverged { heads },
                            ));
                        }
                        HeadAdoptionAttempt::Refused => {}
                    }
                }
                if !tolerable_remediation_for_repair(&remediation, tolerate_unanchored) {
                    return Ok(WorkspaceTxnStart::Remediation(remediation));
                }
            }

            let (operation_heads, final_observation) = match mode {
                WorkspaceTxnMode::Observe => (observed_operation_heads.clone(), final_observation),
                WorkspaceTxnMode::Reconcile | WorkspaceTxnMode::Force => {
                    // CB-13D review R1: a metadata-only boundary must not
                    // execute recovery. Everything below can replay
                    // filesystem/ref plans; refuse *before* any of it runs
                    // when pending work exists, deferring to the explicit
                    // command boundary instead.
                    if budget.is_metadata_only() {
                        if let Some(detail) = self.pending_unsafe_recovery_work(working_copy)? {
                            return Err(RepositoryError::ReactiveDeferred { detail });
                        }
                    }
                    self.recover_pending_deferred_tree_alignment_locked(&operation_lock)?;
                    self.ensure_repository_operation_safe_for(&operation_lock)?;
                    if let OperationHeadState::Diverged(heads) =
                        self.consolidate_operation_heads_locked(&operation_lock)?
                    {
                        return Ok(WorkspaceTxnStart::Remediation(
                            WorkspaceRemediation::OperationHeadsDiverged { heads },
                        ));
                    }
                    self.recover_incomplete_operation(&operation_lock)?;
                    (record, view) = self.load_workspace_authority(working_copy)?;

                    let recovered_observation = observe(self.root()).map_err(observation_error)?;
                    if recovered_observation.token() != final_token {
                        return Ok(WorkspaceTxnStart::Remediation(
                            WorkspaceRemediation::ConcurrentGitMutation {
                                attempts: attempt,
                                first: final_token,
                                last: recovered_observation.token(),
                            },
                        ));
                    }
                    // CB-13D review R2: revalidate Git quiescence under the
                    // retained entry lease, immediately before the boundary
                    // reports ready. A lock that appeared after the caller's
                    // pre-entry wait is fenced here (the check-to-entry
                    // race), because every effect executed with this
                    // transaction happens after this point.
                    if budget.is_metadata_only() {
                        if let GitQuiescence::Busy { reason, detail } =
                            GitQuiescence::evaluate(&recovered_observation)
                        {
                            return Ok(WorkspaceTxnStart::Remediation(
                                WorkspaceRemediation::GitLocksBusy { reason, detail },
                            ));
                        }
                    }
                    if let Some(remediation) = classify_git_state(
                        mode,
                        &record,
                        &view,
                        checkpoint.as_ref(),
                        &recovered_observation,
                    )
                    .filter(|remediation| {
                        !tolerable_remediation_for_repair(remediation, tolerate_unanchored)
                    }) {
                        return Ok(WorkspaceTxnStart::Remediation(remediation));
                    }
                    (
                        self.consolidate_operation_heads_locked(&operation_lock)?,
                        recovered_observation,
                    )
                }
            };
            if let OperationHeadState::Diverged(heads) = &operation_heads {
                return Ok(WorkspaceTxnStart::Remediation(
                    WorkspaceRemediation::OperationHeadsDiverged {
                        heads: heads.clone(),
                    },
                ));
            }

            // RFC §7.1 step 6: a verified bridge workspace with pending edits
            // captures a baseline-relative snapshot plus pre-transition
            // evidence at the boundary, so any later bound HEAD adoption can
            // prove the carried edit (CB-7B). Capture failure refuses the
            // boundary (fail closed); non-bridge workspaces are unaffected.
            if mode != WorkspaceTxnMode::Observe {
                self.capture_pending_workspace_evidence(working_copy)?;
            }

            return Ok(WorkspaceTxnStart::Ready(WorkspaceTxn {
                _operation_lock: operation_lock,
                mode,
                attempts: attempt,
                record,
                view,
                operation_heads,
                checkpoint,
                git: final_observation,
                plan,
                budget,
            }));
        }

        // The retry budget is exhausted. A token-changing Git mutation reports the
        // concurrent-mutation remediation; a head candidate that kept adopting
        // without ever converging (never token-changing) must surface as its
        // typed remediation instead of panicking on the missing token.
        if let (None, Some(_)) = (&first_changed, &last_changed) {
            let last = observe(self.root()).map_err(observation_error)?;
            if let Some(remediation) =
                classify_git_state(mode, &record, &view, checkpoint.as_ref(), &last)
            {
                return Ok(WorkspaceTxnStart::Remediation(remediation));
            }
        }
        // A retry budget exhausted without ANY token change (all attempts
        // re-observed the same stable state) is not a concurrent Git
        // mutation — it is the entry's terminal classification loop failing
        // to converge; surface the typed remediation for the final state
        // instead of panicking on the missing token (CB-13D ::24 R5, the
        // linked-worktree entry path hit this).
        match (first_changed, last_changed) {
            (Some(first), Some(last)) => Ok(WorkspaceTxnStart::Remediation(
                WorkspaceRemediation::ConcurrentGitMutation {
                    attempts: MAX_WORKSPACE_TXN_ATTEMPTS,
                    first,
                    last,
                },
            )),
            (None, None) => {
                let last = observe(self.root()).map_err(observation_error)?;
                match classify_git_state(mode, &record, &view, checkpoint.as_ref(), &last) {
                    Some(remediation) => Ok(WorkspaceTxnStart::Remediation(remediation)),
                    None => Ok(WorkspaceTxnStart::Remediation(
                        WorkspaceRemediation::ConcurrentGitMutation {
                            attempts: MAX_WORKSPACE_TXN_ATTEMPTS,
                            first: last.token(),
                            last: last.token(),
                        },
                    )),
                }
            }
            (Some(first), last) | (last @ None, Some(first)) => {
                let last = last.clone().unwrap_or_else(|| first.clone());
                Ok(WorkspaceTxnStart::Remediation(
                    WorkspaceRemediation::ConcurrentGitMutation {
                        attempts: MAX_WORKSPACE_TXN_ATTEMPTS,
                        first,
                        last,
                    },
                ))
            }
        }
    }

    fn load_workspace_authority(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<(WorkingCopyRecord, ViewState), RepositoryError> {
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let record = txn
            .get_working_copy(working_copy)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?;
        let view = txn
            .get_view_by_id(record.desired_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: format!("id {}", record.desired_view),
            })?;
        if view.state != record.desired_state {
            return Err(RepositoryError::InvalidRepository {
                reason: format!(
                    "working-copy {} expects view '{}' at {}, but the view is at {}",
                    working_copy, view.name, record.desired_state, view.state
                ),
            });
        }
        Ok((record, view))
    }

    /// CB-7A §7.3: reconcile a candidate HEAD remediation before the
    /// filesystem phase (§12.2). Order mirrors the entry protocol: the
    /// shared recovery gates run first (§7.1 steps 2–4 already classified),
    /// then the bound adoption; the adoption itself re-observes HEAD/index
    /// before any checkpoint is published (§7.1 step 8).
    ///
    /// CB-13D review R1: under the metadata-only budget the adoption is
    /// refused *before* any recovery gate or shelf/WIP/ref effect runs — the
    /// §7.3 effect set (shelf planner/executor, WIP capture and WIP-ref
    /// deletion) is a command-boundary effect, not a metadata one. The
    /// caller keeps the typed remediation and surfaces it instead of
    /// silently invoking the full command effect set.
    fn attempt_head_adoption(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
        working_copy: WorkingCopyId,
        checkpoint: Option<&WorkspaceCheckpoint>,
        observation: &WorkspaceGitObservation,
        budget: ReconcileEffectBudget,
    ) -> Result<HeadAdoptionAttempt, RepositoryError> {
        if budget.is_metadata_only() {
            log::info!(
                "bridge watch: metadata-only budget refuses bound HEAD adoption; \
                 the workspace change needs an explicit 'atomic git bridge reconcile'"
            );
            return Ok(HeadAdoptionAttempt::Refused);
        }
        // Read-only pre-check first: a refusal (unbound commit, unborn HEAD,
        // unusable observation) must stay non-mutating, so the shared
        // recovery gates below only run when a verified binding actually
        // covers the observed HEAD (RFC §12.11, CB-0C guard contract).
        if super::anchor::head_binding_for_observation(self, observation).is_none() {
            return Ok(HeadAdoptionAttempt::Refused);
        }
        self.recover_pending_deferred_tree_alignment_locked(operation_lock)?;
        self.ensure_repository_operation_safe_for(operation_lock)?;
        if let OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(operation_lock)?
        {
            return Ok(HeadAdoptionAttempt::Diverged { heads });
        }
        self.recover_incomplete_operation(operation_lock)?;
        match self.adopt_bound_git_head_locked(
            operation_lock,
            working_copy,
            checkpoint,
            observation,
        ) {
            Ok(_) => Ok(HeadAdoptionAttempt::Adopted),
            Err(error) => {
                // The typed refusal is surfaced through the stale-baseline
                // report so users see exactly why the workspace stays
                // unanchored (RFC §7.3, §12.7).
                log::warn!("bound Git HEAD adoption refused: {error}");
                Ok(HeadAdoptionAttempt::Refused)
            }
        }
    }

    /// Whether the repository holds pending work whose recovery would
    /// execute effect-bearing plans (deferred tree alignment, incomplete
    /// working-copy or repository operation heads). Used by the
    /// metadata-only boundary to refuse *before* any recovery runs
    /// (CB-13D review R1); the query is read-only.
    pub(super) fn pending_unsafe_recovery_work(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<Option<String>, RepositoryError> {
        if self.has_pending_deferred_tree_alignment() {
            return Ok(Some(
                "a deferred tree alignment is pending and its recovery writes \
                 working-copy metadata through the command effect set"
                    .to_string(),
            ));
        }
        if self.working_copy_operation_requires_recovery(working_copy)? {
            return Ok(Some(
                "an incomplete working-copy operation head requires recovery; its \
                 filesystem/ref plan may only replay at a command boundary"
                    .to_string(),
            ));
        }
        if self.repository_operation_requires_recovery()? {
            return Ok(Some(
                "an incomplete repository operation head requires recovery; its \
                 effects may only replay at a command boundary"
                    .to_string(),
            ));
        }
        Ok(None)
    }
}

fn classify_git_state(
    mode: WorkspaceTxnMode,
    record: &WorkingCopyRecord,
    view: &ViewState,
    checkpoint: Option<&WorkspaceCheckpoint>,
    observation: &WorkspaceGitObservation,
) -> Option<WorkspaceRemediation> {
    let WorkspaceGitObservation::Repository(git) = observation else {
        return checkpoint.map(|checkpoint| {
            unanchored(
                mode,
                UnanchoredWorkspace::GitRepositoryMissing,
                Some(checkpoint),
                GitHeadObservation::Unborn {
                    symref: "no-git".to_string(),
                },
            )
        });
    };

    let conflict_stages = git.conflict_stages();
    if git.operation.is_in_progress() || !conflict_stages.is_empty() {
        let disposition = match mode {
            WorkspaceTxnMode::Observe => GitOperationDisposition::ObserveOnly,
            WorkspaceTxnMode::Reconcile => GitOperationDisposition::FinishOrAbortInGit,
            WorkspaceTxnMode::Force => GitOperationDisposition::ForceForbidden,
        };
        return Some(WorkspaceRemediation::GitOperationInProgress {
            mode,
            repository_state: git.operation.repository_state.clone(),
            markers: git.operation.present_markers(),
            conflict_stages,
            disposition,
        });
    }

    if git.index_lock.is_present() {
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::IndexLocked {
                path: git.index_lock.path.clone(),
            },
            checkpoint,
            git.head.clone(),
        ));
    }

    let observed_oid = match &git.head {
        GitHeadObservation::Attached { oid, .. } => oid,
        GitHeadObservation::Detached { oid } => {
            // CB-7A (RFC §7.5): a detached HEAD is aligned when the checkpoint
            // already matches it exactly — recording and observing on an
            // ephemeral `git/<oid>` view stay valid. Otherwise it is
            // unanchored and the §7.3 adoption may resolve it.
            let aligned_detached = match checkpoint {
                Some(checkpoint) => {
                    checkpoint.git_head_symref.is_none()
                        && checkpoint.git_head == *oid
                        && git.head_tree.as_deref() == Some(checkpoint.git_tree.as_str())
                }
                None => false,
            };
            if !aligned_detached {
                return Some(unanchored(
                    mode,
                    UnanchoredWorkspace::DetachedHead { oid: oid.clone() },
                    checkpoint,
                    git.head.clone(),
                ));
            }
            oid
        }
        GitHeadObservation::Unborn { symref } => {
            return Some(unanchored(
                mode,
                UnanchoredWorkspace::UnbornHead {
                    symref: symref.clone(),
                },
                checkpoint,
                git.head.clone(),
            ))
        }
        GitHeadObservation::MissingTarget { symref } => {
            return Some(unanchored(
                mode,
                UnanchoredWorkspace::MissingHeadTarget {
                    symref: symref.clone(),
                },
                checkpoint,
                git.head.clone(),
            ))
        }
    };

    let Some(checkpoint) = checkpoint else {
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::MissingCheckpoint,
            None,
            git.head.clone(),
        ));
    };
    let atomic_checkpoint_drift = checkpoint.view != view.name;
    if atomic_checkpoint_drift && mode != WorkspaceTxnMode::Force {
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::AtomicCheckpointDrift {
                checkpoint_view: checkpoint.view.clone(),
                checkpoint_state: checkpoint.atomic_state.clone(),
                desired_view: view.name.clone(),
                desired_state: record.desired_state.to_string(),
            },
            Some(checkpoint),
            git.head.clone(),
        ));
    }
    if let (Some(checkpoint_symref), Some(observed_symref)) =
        (checkpoint.git_head_symref.as_ref(), head_symref(&git.head))
    {
        if checkpoint_symref != observed_symref {
            return Some(unanchored(
                mode,
                UnanchoredWorkspace::HeadSymrefChanged {
                    checkpoint: checkpoint_symref.clone(),
                    observed: observed_symref.to_string(),
                },
                Some(checkpoint),
                git.head.clone(),
            ));
        }
    }
    // CB-7A §7.5: a detach→attach transition on the same commit changes only
    // the symbol. It matters exactly when the workspace sits on an ephemeral
    // `git/<oid>` view: `git switch -c` then renames that view onto the new
    // branch, and an ordinary `git switch <branch>` re-maps the workspace.
    // Elsewhere the symbol is advisory and the baseline stays aligned.
    if checkpoint.git_head == *observed_oid
        && git.head_tree.as_deref() == Some(checkpoint.git_tree.as_str())
        && checkpoint.git_head_symref.is_none()
        && head_symref(&git.head).is_some()
        && checkpoint.view.starts_with("git/")
    {
        let observed_display = head_symref(&git.head)
            .map(str::to_string)
            .unwrap_or_else(|| format!("<detached at {observed_oid}>"));
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::HeadSymrefChanged {
                checkpoint: format!("<detached at {}>", checkpoint.git_head),
                observed: observed_display,
            },
            Some(checkpoint),
            git.head.clone(),
        ));
    }
    if checkpoint.git_head != *observed_oid {
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::HeadChanged {
                checkpoint: checkpoint.git_head.clone(),
                observed: observed_oid.clone(),
            },
            Some(checkpoint),
            git.head.clone(),
        ));
    }
    if git.head_tree.as_deref() != Some(checkpoint.git_tree.as_str()) {
        return Some(unanchored(
            mode,
            UnanchoredWorkspace::HeadTreeChanged {
                checkpoint: checkpoint.git_tree.clone(),
                observed: git.head_tree.clone().unwrap_or_default(),
            },
            Some(checkpoint),
            git.head.clone(),
        ));
    }
    // CB-11A (RFC §7.6, §9.3): index movement (`git add`, `git reset`,
    // `git rm --cached`, alternate `GIT_INDEX_FILE`) is legitimate user work
    // between boundaries. It is mirrored into StagingState by status and
    // staging observations and must NOT be classified as a stale baseline:
    // the durable graph this workspace interprets is unchanged (HEAD, HEAD
    // tree, view, and state all matched above). The recorded index digest
    // stays advisory evidence for the forensic report and
    // `atomic bridge verify`.
    //
    // `UnanchoredWorkspace::IndexTreeChanged` remains a public variant and
    // stays tolerated by the repair boundary, but it is no longer produced
    // for plain index movement.
    None
}

/// Whether a classified remediation may be ignored by the explicit repair
/// boundary. Only unanchored checkpoint/HEAD/index drift is repairable; Git
/// locks, unborn HEAD, missing Git, and Git-owned operations are refused in
/// every mode.
///
/// A detached HEAD is tolerated here because the two remediation commands
/// that enter this boundary (`git bridge reconcile`, `git import`) are exactly
/// the §7.3/§7.5 paths that may resolve a detached commit: a verified binding
/// adopts through the CB-7A entry adoption; an unbound commit routes through
/// the §7.5 ephemeral mapping and the CB-9A importer. Ordinary command
/// boundaries never tolerate it and keep the typed refusal.
fn tolerable_remediation_for_repair(
    remediation: &WorkspaceRemediation,
    tolerate_unanchored: bool,
) -> bool {
    if !tolerate_unanchored {
        return false;
    }
    matches!(
        remediation,
        WorkspaceRemediation::Unanchored {
            state: UnanchoredWorkspace::MissingCheckpoint
                | UnanchoredWorkspace::AtomicCheckpointDrift { .. }
                | UnanchoredWorkspace::HeadSymrefChanged { .. }
                | UnanchoredWorkspace::HeadTreeChanged { .. }
                | UnanchoredWorkspace::IndexTreeChanged { .. }
                | UnanchoredWorkspace::HeadChanged { .. }
                | UnanchoredWorkspace::DetachedHead { .. },
            ..
        }
    )
}

fn head_symref(head: &GitHeadObservation) -> Option<&str> {
    match head {
        GitHeadObservation::Attached { symref, .. }
        | GitHeadObservation::Unborn { symref }
        | GitHeadObservation::MissingTarget { symref } => Some(symref),
        GitHeadObservation::Detached { .. } => None,
    }
}

/// Observation for a working copy outside the bridge: its Git metadata
/// belongs to the user and ordinary Atomic boundaries do not interpret it.
fn observe_native_workspace(
    root: &Path,
) -> Result<WorkspaceGitObservation, super::ObservationError> {
    Ok(WorkspaceGitObservation::NoGit {
        root: root.to_path_buf(),
    })
}

fn unanchored(
    mode: WorkspaceTxnMode,
    state: UnanchoredWorkspace,
    checkpoint: Option<&WorkspaceCheckpoint>,
    observed: GitHeadObservation,
) -> WorkspaceRemediation {
    WorkspaceRemediation::Unanchored {
        mode,
        state,
        plan: WorkspaceEntryPlan::blocked(
            checkpoint.map(|checkpoint| checkpoint.git_head.clone()),
            observed,
        ),
    }
}

pub(crate) fn read_workspace_checkpoint(
    root: &Path,
) -> Result<Option<WorkspaceCheckpoint>, RepositoryError> {
    let path = root.join(CHECKPOINT_RELATIVE_PATH);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RepositoryError::InvalidRepository {
                reason: format!(
                    "cannot read bridge checkpoint '{}': {error}",
                    path.display()
                ),
            })
        }
    };
    let wire: CheckpointWire =
        serde_json::from_slice(&bytes).map_err(|error| RepositoryError::InvalidRepository {
            reason: format!(
                "bridge checkpoint '{}' is malformed: {error}",
                path.display()
            ),
        })?;
    if wire.version != 1 && wire.version != 2 {
        return Err(RepositoryError::InvalidRepository {
            reason: format!("unsupported bridge checkpoint version {}", wire.version),
        });
    }
    let git_head_symref = wire
        .git_head_symref
        .or_else(|| (wire.version == 1).then(|| format!("refs/heads/{}", wire.view)));
    Ok(Some(WorkspaceCheckpoint {
        version: wire.version,
        view: wire.view,
        atomic_state: wire.atomic_state,
        git_head_symref,
        git_head: wire.git_head,
        git_tree: wire.git_tree.clone(),
        git_index_tree: wire
            .git_index_tree
            .or_else(|| (wire.version == 1).then_some(wire.git_tree)),
        git_index_digest: wire.git_index_digest,
    }))
}

/// Parse a checkpoint from its canonical wire bytes (CB-8B ac-3: the
/// retained checkpoint payload of a journaled publication is verified against
/// its FACTS digest before it may be published).
pub(crate) fn workspace_checkpoint_from_bytes(
    bytes: &[u8],
) -> Result<WorkspaceCheckpoint, RepositoryError> {
    let wire: CheckpointWire =
        serde_json::from_slice(bytes).map_err(|error| RepositoryError::InvalidRepository {
            reason: format!("bridge checkpoint bytes are malformed: {error}"),
        })?;
    if wire.version != 1 && wire.version != 2 {
        return Err(RepositoryError::InvalidRepository {
            reason: format!("unsupported bridge checkpoint version {}", wire.version),
        });
    }
    let git_head_symref = wire
        .git_head_symref
        .or_else(|| (wire.version == 1).then(|| format!("refs/heads/{}", wire.view)));
    Ok(WorkspaceCheckpoint {
        version: wire.version,
        view: wire.view,
        atomic_state: wire.atomic_state,
        git_head_symref,
        git_head: wire.git_head,
        git_tree: wire.git_tree.clone(),
        git_index_tree: wire
            .git_index_tree
            .or_else(|| (wire.version == 1).then_some(wire.git_tree)),
        git_index_digest: wire.git_index_digest,
    })
}

/// Persist the version-2 workspace checkpoint atomically (RFC §7.2 step 6).
///
/// The checkpoint is derived evidence outside the operation hash: it is
/// written only after the state it describes has been verified and
/// re-observed. The wire format matches [`read_workspace_checkpoint`] and the
/// CLI's richer v2 evidence fields are additive and optional.
pub(crate) fn write_workspace_checkpoint(
    root: &Path,
    checkpoint: &WorkspaceCheckpoint,
) -> Result<(), RepositoryError> {
    let path = root.join(CHECKPOINT_RELATIVE_PATH);
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidRepository {
            reason: "bridge checkpoint path has no parent directory".to_string(),
        })?;
    fs::create_dir_all(parent).map_err(|error| RepositoryError::InvalidRepository {
        reason: format!(
            "cannot create bridge checkpoint directory '{}': {error}",
            parent.display()
        ),
    })?;
    let bytes = workspace_checkpoint_bytes(checkpoint)?;
    let temporary = parent.join(format!(".workspace.json.{}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| RepositoryError::InvalidRepository {
        reason: format!(
            "cannot write bridge checkpoint '{}': {error}",
            path.display()
        ),
    })
}

/// The exact on-disk bytes of a version-2 checkpoint (pretty JSON + newline).
///
/// Shared by the write path and the R3 Checkpoint digest leases so a lease
/// digest always matches the file bytes byte-for-byte.
pub(crate) fn workspace_checkpoint_bytes(
    checkpoint: &WorkspaceCheckpoint,
) -> Result<Vec<u8>, RepositoryError> {
    let wire = CheckpointWire {
        version: 2,
        view: checkpoint.view.clone(),
        atomic_state: checkpoint.atomic_state.clone(),
        git_head_symref: checkpoint.git_head_symref.clone(),
        git_head: checkpoint.git_head.clone(),
        git_tree: checkpoint.git_tree.clone(),
        git_index_tree: checkpoint.git_index_tree.clone(),
        git_index_digest: checkpoint.git_index_digest.clone(),
    };
    let mut bytes = serde_json::to_vec_pretty(&wire)
        .map_err(|error| RepositoryError::Serialization(error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn observation_error(error: super::ObservationError) -> RepositoryError {
    RepositoryError::InvalidRepository {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_core::Hash;

    fn head(oid: &str) -> GitHeadObservation {
        GitHeadObservation::Attached {
            symref: "refs/heads/main".to_string(),
            oid: oid.to_string(),
        }
    }

    #[test]
    fn bounded_plan_structurally_blocks_filesystem_before_head_alignment() {
        let plan = WorkspaceEntryPlan::blocked(Some("old".to_string()), head("new"));
        assert!(plan.is_ordered());
        assert_eq!(plan.item_count(), 3);
        assert!(matches!(
            plan.filesystem,
            WorkspaceFilesystemPlan::BlockedUntilHeadAligned
        ));
    }

    #[test]
    fn observation_token_compares_head_and_index() {
        let first = token("one", b"index-one");
        let second = token("one", b"index-two");
        assert_ne!(first, second);
    }

    #[test]
    fn unstable_observation_retries_three_times_then_returns_remediation() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");
        let mut observations = vec![
            fake_git("stable", b"one"),
            fake_git("changed", b"two"),
            fake_git("stable", b"one"),
            fake_git("changed", b"three"),
            fake_git("stable", b"one"),
            fake_git("changed", b"four"),
        ]
        .into_iter();

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| {
                Ok(observations.next().unwrap())
            })
            .unwrap();
        let WorkspaceTxnStart::Remediation(WorkspaceRemediation::ConcurrentGitMutation {
            attempts,
            ..
        }) = start
        else {
            panic!("unstable observations must return retry remediation");
        };
        assert_eq!(attempts, MAX_WORKSPACE_TXN_ATTEMPTS);
    }

    #[test]
    fn changed_first_attempt_is_discarded_and_second_stable_attempt_succeeds() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");
        let mut observations = vec![
            fake_git("stable", b"one"),
            fake_git("changed", b"two"),
            fake_git("stable", b"three"),
            fake_git("stable", b"three"),
        ]
        .into_iter();

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| {
                Ok(observations.next().unwrap())
            })
            .unwrap();
        let WorkspaceTxnStart::Ready(txn) = start else {
            panic!("the second stable attempt should succeed");
        };
        assert_eq!(txn.attempts(), 2);
    }

    #[test]
    fn unsafe_state_appearing_on_reobservation_never_returns_ready() {
        use super::super::git_observation::{GitAdminEntryKind, GitOperationMarkerObservation};

        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");
        let clean = fake_git("stable", b"one");
        let mut sequence = clean.clone();
        let WorkspaceGitObservation::Repository(git) = &mut sequence else {
            unreachable!();
        };
        git.operation.markers.push(GitOperationMarkerObservation {
            marker: GitOperationMarker::RebaseApply,
            path: PathBuf::from(".git/rebase-apply"),
            kind: GitAdminEntryKind::Directory,
        });
        let mut observations = vec![clean, sequence.clone(), sequence].into_iter();

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| {
                Ok(observations.next().unwrap())
            })
            .unwrap();
        assert!(matches!(
            start,
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress { .. })
        ));
    }

    /// CB-13C observability + RFC Phase 0 (CB-0C): a guarded command's
    /// refused entry writes nothing — the refusal is observable through its
    /// typed diagnostic only, never through telemetry. A ready entry
    /// records nothing either; telemetry is emitted by mutating
    /// remediation boundaries.
    #[test]
    fn guarded_refusals_write_no_telemetry() {
        use super::super::git_observation::{GitAdminEntryKind, GitOperationMarkerObservation};

        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");
        let journal = directory
            .path()
            .join(super::super::DOT_DIR)
            .join("bridge/events.jsonl");

        // A ready entry records no event.
        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| {
                Ok(fake_git("stable", b"one"))
            })
            .unwrap();
        assert!(matches!(start, WorkspaceTxnStart::Ready(_)));
        assert!(!journal.exists(), "ready entries are not telemetry");

        // A Git-owned in-progress state refuses — and writes nothing.
        let mut sequence = fake_git("stable", b"one");
        let WorkspaceGitObservation::Repository(git) = &mut sequence else {
            unreachable!();
        };
        git.operation.markers.push(GitOperationMarkerObservation {
            marker: GitOperationMarker::RebaseApply,
            path: PathBuf::from(".git/rebase-apply"),
            kind: GitAdminEntryKind::Directory,
        });
        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| Ok(sequence.clone()))
            .unwrap();
        assert!(matches!(
            start,
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress { .. })
        ));
        assert!(
            !journal.exists(),
            "a refused guarded entry must not mutate .atomic (RFC Phase 0 no-mutation contract)"
        );
    }

    #[test]
    fn conflicted_index_stages_are_git_owned_and_cannot_be_forced() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");
        let mut conflict = fake_git("stable", b"conflict");
        let WorkspaceGitObservation::Repository(git) = &mut conflict else {
            unreachable!();
        };
        git.index_stages = vec![1, 2, 3];

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Force, |_| Ok(conflict.clone()))
            .unwrap();
        let WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress {
            conflict_stages,
            disposition,
            ..
        }) = start
        else {
            panic!("conflicted index must return Git-owned remediation");
        };
        assert_eq!(conflict_stages, vec![1, 2, 3]);
        assert_eq!(disposition, GitOperationDisposition::ForceForbidden);
    }

    fn fake_git(oid: &str, index: &[u8]) -> WorkspaceGitObservation {
        use super::super::git_observation::{
            GitAdminEntryKind, GitAdminPathObservation, GitOperationObservation,
            WorkspaceGitRepositoryObservation,
        };

        WorkspaceGitObservation::Repository(Box::new(WorkspaceGitRepositoryObservation {
            worktree_git_dir: PathBuf::from(".git"),
            common_dir: PathBuf::from(".git"),
            index_path: PathBuf::from(".git/index"),
            head: head(oid),
            head_tree: Some("tree".to_string()),
            index_digest: Hash::of(index),
            index_tree: Some("tree".to_string()),
            index_stages: vec![0],
            index_lock: GitAdminPathObservation {
                path: PathBuf::from(".git/index.lock"),
                kind: GitAdminEntryKind::Missing,
            },
            operation: GitOperationObservation {
                repository_state: "Clean".to_string(),
                markers: Vec::new(),
            },
        }))
    }

    fn token(oid: &str, index: &[u8]) -> GitObservationToken {
        GitObservationToken {
            head: head(oid),
            head_tree: Some("tree".to_string()),
            index_digest: Hash::of(index),
            index_tree: Some("tree".to_string()),
            index_stages: vec![0],
            index_locked: false,
            repository_state: "Clean".to_string(),
            markers: Vec::new(),
        }
    }

    fn write_test_checkpoint(root: &Path, repo: &Repository, oid: &str) {
        let path = root.join(CHECKPOINT_RELATIVE_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let state = repo
            .working_copy_record(repo.require_working_copy_id().unwrap())
            .unwrap()
            .desired_state;
        fs::write(
            path,
            format!(
                "{{\"version\":2,\"view\":\"dev\",\"atomic_state\":\"{state}\",\"git_head\":\"{oid}\",\"git_tree\":\"tree\"}}"
            ),
        )
        .unwrap();
    }

    fn with_marker(
        base: &WorkspaceGitObservation,
        marker: GitOperationMarker,
    ) -> WorkspaceGitObservation {
        use super::super::git_observation::{GitAdminEntryKind, GitOperationMarkerObservation};
        let mut cloned = base.clone();
        let WorkspaceGitObservation::Repository(git) = &mut cloned else {
            unreachable!()
        };
        git.operation.markers.push(GitOperationMarkerObservation {
            marker,
            path: PathBuf::from(format!(".git/{}", marker.relative_path())),
            kind: GitAdminEntryKind::Directory,
        });
        git.operation.repository_state = "Rebase".to_string();
        cloned
    }

    #[test]
    fn remediation_describe_reports_dispositions_markers_and_stages() {
        use super::super::git_observation::GitOperationMarker;
        let remediation = WorkspaceRemediation::GitOperationInProgress {
            mode: WorkspaceTxnMode::Reconcile,
            repository_state: "Rebase".to_string(),
            markers: vec![
                GitOperationMarker::RebaseMerge,
                GitOperationMarker::MergeHead,
            ],
            conflict_stages: vec![1, 2, 3],
            disposition: GitOperationDisposition::FinishOrAbortInGit,
        };
        let description = remediation.describe();
        assert!(
            description.contains("Git operation in progress (Rebase)"),
            "{description}"
        );
        assert!(
            description.contains("finish or abort the Git operation in Git"),
            "{description}"
        );
        assert!(
            description.contains("rebase-merge, MERGE_HEAD"),
            "{description}"
        );
        assert!(
            description.contains("3 conflicted stage entries"),
            "{description}"
        );

        let diverged = WorkspaceRemediation::OperationHeadsDiverged {
            heads: vec![
                OperationId::from_bytes([7u8; 32]),
                OperationId::from_bytes([9u8; 32]),
            ],
        };
        let message = diverged.describe();
        assert!(message.contains("operation heads diverged"), "{message}");
        assert!(
            message.contains("resolve the competing leases instead of retrying a stale plan"),
            "{message}"
        );
    }

    #[test]
    fn remediation_entry_tolerates_missing_checkpoint_and_stale_head() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        // A Git repository without any checkpoint: the ordinary boundary must
        // refuse, the repair boundary must proceed.
        let refused = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Force, |_| Ok(fake_git("stable", b"one")))
            .unwrap();
        assert!(matches!(
            refused,
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::Unanchored {
                state: UnanchoredWorkspace::MissingCheckpoint,
                ..
            })
        ));
        let start = repo
            .begin_remediation_txn_with(|_| Ok(fake_git("stable", b"one")))
            .unwrap();
        let WorkspaceTxnStart::Ready(txn) = start else {
            panic!("repair boundary must tolerate a missing checkpoint");
        };
        assert_eq!(txn.mode(), WorkspaceTxnMode::Force);
        assert!(txn.checkpoint().is_none());
    }

    #[test]
    fn remediation_entry_still_refuses_git_owned_operations_and_index_locks() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        let base = fake_git("stable", b"one");

        let conflicted = {
            let mut observation = base.clone();
            let WorkspaceGitObservation::Repository(git) = &mut observation else {
                unreachable!()
            };
            git.index_stages = vec![1, 2, 3];
            observation
        };
        let start = repo
            .begin_remediation_txn_with(|_| Ok(conflicted.clone()))
            .unwrap();
        assert!(matches!(
            start,
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress {
                disposition: GitOperationDisposition::ForceForbidden,
                ..
            })
        ));

        let locked = {
            use super::super::git_observation::GitAdminEntryKind;
            let mut observation = base.clone();
            let WorkspaceGitObservation::Repository(git) = &mut observation else {
                unreachable!()
            };
            git.index_lock.kind = GitAdminEntryKind::File;
            observation
        };
        let start = repo
            .begin_remediation_txn_with(|_| Ok(locked.clone()))
            .unwrap();
        assert!(matches!(
            start,
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::Unanchored {
                state: UnanchoredWorkspace::IndexLocked { .. },
                ..
            })
        ));

        let rebasing = with_marker(&base, GitOperationMarker::RebaseMerge);
        let start = repo
            .begin_remediation_txn_with(|_| Ok(rebasing.clone()))
            .unwrap();
        assert!(matches!(
            start,
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress { .. })
        ));
    }

    #[test]
    fn every_git_operation_marker_refuses_repair_entry() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        let base = fake_git("stable", b"one");
        for marker in GitOperationMarker::ALL {
            // AUTO_MERGE is advisory: standalone it does not refuse (covered by
            // `standalone_auto_merge_ref_is_advisory_and_permits_entry`).
            if !marker.is_active_operation_evidence() {
                continue;
            }
            let observation = with_marker(&base, marker);
            let start = repo
                .begin_remediation_txn_with(|_| Ok(observation.clone()))
                .unwrap();
            let WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress {
                markers,
                disposition,
                ..
            }) = start
            else {
                panic!("marker {marker:?} must refuse the repair boundary");
            };
            assert_eq!(disposition, GitOperationDisposition::ForceForbidden);
            assert!(markers.contains(&marker));
        }
    }

    /// CB-13D (RFC §11.2 rule 4): the metadata-only budget never grants
    /// projection or materialization, and the command budget grants both.
    #[test]
    fn metadata_only_budget_refuses_projection_and_materialization() {
        assert!(ReconcileEffectBudget::Command.allows_projection());
        assert!(ReconcileEffectBudget::Command.allows_materialization());
        assert!(!ReconcileEffectBudget::MetadataOnly.allows_projection());
        assert!(!ReconcileEffectBudget::MetadataOnly.allows_materialization());
    }

    /// CB-13D (RFC §11.2 rule 3): the reactive silence floor is the RFC
    /// 250 ms minimum, sourced from the bridge watch configuration.
    #[test]
    fn reactive_quiescence_floor_is_the_rfc_250ms_minimum() {
        assert_eq!(MIN_REACTIVE_QUIESCENCE_MS, 250);
        assert_eq!(
            MIN_REACTIVE_QUIESCENCE_MS,
            atomic_config::BridgeWatchConfig::MIN_QUIET_MS
        );
    }

    #[test]
    fn quiescence_evaluates_locks_and_sequence_markers() {
        use super::super::git_observation::GitAdminEntryKind;
        let base = fake_git("stable", b"one");

        // Clean observation: quiescent.
        assert!(matches!(
            GitQuiescence::evaluate(&base),
            GitQuiescence::Quiescent { .. }
        ));
        assert!(git_state_quiescent(&base));

        // Index lock: busy.
        let mut locked = base.clone();
        {
            let WorkspaceGitObservation::Repository(git) = &mut locked else {
                unreachable!()
            };
            git.index_lock.kind = GitAdminEntryKind::File;
        }
        match GitQuiescence::evaluate(&locked) {
            GitQuiescence::Busy {
                reason: "index_lock",
                ..
            } => {}
            other => panic!("index lock must be index_lock busy, got {other:?}"),
        }
        assert!(!git_state_quiescent(&locked));

        // Sequence markers: busy, every AUTHORITATIVE marker kind.
        for marker in GitOperationMarker::ALL {
            if !marker.is_active_operation_evidence() {
                continue;
            }
            let marked = with_marker(&base, marker);
            match GitQuiescence::evaluate(&marked) {
                GitQuiescence::Busy {
                    reason: "sequence_markers",
                    ..
                } => {}
                other => panic!("marker {marker:?} must be sequence busy, got {other:?}"),
            }
            assert!(!git_state_quiescent(&marked));
        }
    }

    /// RFC §11.2 rule 3: a standalone `AUTO_MERGE` root ref is merge-ort's
    /// derived tree reference, not proof of an active operation. With a
    /// `Clean` repository state and no other marker or unmerged stage, the
    /// workspace boundary must permit the operation while still reporting the
    /// advisory marker.
    #[test]
    fn standalone_auto_merge_ref_is_advisory_and_permits_entry() {
        use super::super::git_observation::{GitAdminEntryKind, GitOperationMarkerObservation};

        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");

        let mut standalone = fake_git("stable", b"one");
        {
            let WorkspaceGitObservation::Repository(git) = &mut standalone else {
                unreachable!()
            };
            assert_eq!(git.operation.repository_state, "Clean");
            git.operation.markers.push(GitOperationMarkerObservation {
                marker: GitOperationMarker::AutoMerge,
                path: PathBuf::from(".git/AUTO_MERGE"),
                kind: GitAdminEntryKind::File,
            });

            // Reported, but not active evidence.
            assert!(git
                .operation
                .present_markers()
                .contains(&GitOperationMarker::AutoMerge));
            assert!(git.operation.active_markers().is_empty());
            assert!(!git.operation.is_in_progress());
        }
        assert!(git_state_quiescent(&standalone));
        assert!(matches!(
            GitQuiescence::evaluate(&standalone),
            GitQuiescence::Quiescent { .. }
        ));

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| Ok(standalone.clone()))
            .unwrap();
        assert!(
            matches!(start, WorkspaceTxnStart::Ready(_)),
            "standalone AUTO_MERGE must not fence a Clean workspace"
        );
    }

    /// A genuine stopped merge carries `MERGE_HEAD` alongside `AUTO_MERGE` and
    /// must still refuse.
    #[test]
    fn active_merge_head_with_auto_merge_still_refuses() {
        use super::super::git_observation::{GitAdminEntryKind, GitOperationMarkerObservation};

        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");

        let mut merging = fake_git("stable", b"one");
        {
            let WorkspaceGitObservation::Repository(git) = &mut merging else {
                unreachable!()
            };
            for marker in [GitOperationMarker::MergeHead, GitOperationMarker::AutoMerge] {
                git.operation.markers.push(GitOperationMarkerObservation {
                    marker,
                    path: PathBuf::from(format!(".git/{}", marker.relative_path())),
                    kind: GitAdminEntryKind::File,
                });
            }
            assert!(git.operation.is_in_progress());
        }
        assert!(!git_state_quiescent(&merging));

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| Ok(merging.clone()))
            .unwrap();
        assert!(matches!(
            start,
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress { .. })
        ));
    }

    /// An unmerged index still refuses even when the only marker is the
    /// advisory `AUTO_MERGE`.
    #[test]
    fn unmerged_index_with_standalone_auto_merge_still_refuses() {
        use super::super::git_observation::{GitAdminEntryKind, GitOperationMarkerObservation};

        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        write_test_checkpoint(directory.path(), &repo, "stable");

        let mut conflict = fake_git("stable", b"conflict");
        let WorkspaceGitObservation::Repository(git) = &mut conflict else {
            unreachable!()
        };
        git.index_stages = vec![1, 2, 3];
        git.operation.markers.push(GitOperationMarkerObservation {
            marker: GitOperationMarker::AutoMerge,
            path: PathBuf::from(".git/AUTO_MERGE"),
            kind: GitAdminEntryKind::File,
        });
        assert_eq!(git.operation.repository_state, "Clean");
        assert!(!git.operation.is_in_progress());
        assert!(!git.conflict_stages().is_empty());

        let start = repo
            .begin_workspace_txn_with(WorkspaceTxnMode::Observe, |_| Ok(conflict.clone()))
            .unwrap();
        assert!(matches!(
            start,
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress { .. })
        ));
    }

    /// CB-13D (RFC §11.2 rule 3): ref locks found through the Git-resolved
    /// common/worktree dirs make the state busy, and the enumeration is
    /// real filesystem evidence, not a flag.
    #[test]
    fn ref_locks_on_disk_block_quiescence() {
        let directory = tempfile::TempDir::new().unwrap();
        let refs = directory.path().join("refs").join("heads");
        fs::create_dir_all(&refs).unwrap();
        let mut with_dirs = fake_git("stable", b"one");
        if let WorkspaceGitObservation::Repository(git) = &mut with_dirs {
            // Point the fixture's resolved dirs at the real temp layout.
            git.common_dir = directory.path().to_path_buf();
            git.worktree_git_dir = directory.path().to_path_buf();
        }
        assert!(git_state_quiescent(&with_dirs));
        fs::write(refs.join("main.lock"), b"").unwrap();
        assert!(!git_state_quiescent(&with_dirs));
        let locks = git_locks_present(directory.path(), directory.path()).unwrap();
        assert_eq!(locks, vec![refs.join("main.lock")]);
        match GitQuiescence::evaluate(&with_dirs) {
            GitQuiescence::Busy {
                reason: "ref_locks",
                ..
            } => {}
            other => panic!("ref lock must be ref_locks busy, got {other:?}"),
        }
    }

    /// CB-13D review R2: the lock enumeration covers HEAD.lock,
    /// packed-refs.lock, reftable locks and top-level administrative locks
    /// in both Git directories, and fails closed on an uninspectable path.
    #[test]
    fn git_locks_present_covers_head_admin_and_reftable_locks() {
        let directory = tempfile::TempDir::new().unwrap();
        let worktree_git = directory.path().join(".git");
        let common_git = directory.path().join("common.git");
        fs::create_dir_all(worktree_git.join("refs/heads")).unwrap();
        fs::create_dir_all(common_git.join("reftable")).unwrap();

        // Empty: no locks.
        assert!(git_locks_present(&common_git, &worktree_git)
            .unwrap()
            .is_empty());

        // HEAD.lock in the worktree git dir, packed-refs.lock and a
        // top-level admin lock in the common dir, plus a reftable lock.
        fs::write(worktree_git.join("HEAD.lock"), b"").unwrap();
        fs::write(common_git.join("packed-refs.lock"), b"").unwrap();
        fs::write(common_git.join("config.lock"), b"").unwrap();
        fs::write(common_git.join("reftable").join("000001.lock"), b"").unwrap();
        let locks = git_locks_present(&common_git, &worktree_git).unwrap();
        for expected in [
            worktree_git.join("HEAD.lock"),
            common_git.join("packed-refs.lock"),
            common_git.join("config.lock"),
            common_git.join("reftable").join("000001.lock"),
        ] {
            assert!(
                locks.contains(&expected),
                "HEAD/admin/reftable locks must be enumerated, missing {expected:?} in {locks:?}"
            );
        }
        // The enumeration is busy through the quiescence verdict too.
        let mut busy = fake_git("stable", b"one");
        if let WorkspaceGitObservation::Repository(git) = &mut busy {
            git.common_dir = common_git.clone();
            git.worktree_git_dir = worktree_git.clone();
        }
        assert!(!git_state_quiescent(&busy));
        match GitQuiescence::evaluate(&busy) {
            GitQuiescence::Busy {
                reason: "ref_locks",
                ..
            } => {}
            other => panic!("HEAD/admin locks must be ref_locks busy, got {other:?}"),
        }

        // Fail closed: where a directory is expected but a file blocks
        // inspection, the enumeration errors instead of silently passing.
        fs::remove_file(worktree_git.join("HEAD.lock")).unwrap();
        fs::remove_dir_all(worktree_git.join("refs")).unwrap();
        fs::write(worktree_git.join("refs"), b"not a directory").unwrap();
        assert!(git_locks_present(&common_git, &worktree_git).is_err());
    }

    /// CB-13D review R2: the check-to-entry race is fenced under the
    /// retained lease — a HEAD.lock that appears after the caller's
    /// pre-entry quiescence wait is caught by the entry's revalidation
    /// under the metadata-only budget, before any effect.
    /// CB-13D ::24 AC-2 namespace-completeness proof: the quiet-lock
    /// enumeration records WHICH namespaces it walked (all ten: head-lock,
    /// packed-refs-lock, reftable, refs, top-level × worktree/common) and
    /// records every symlinked directory the policy skipped — a symlinked
    /// subdirectory never silently narrows the inspected namespace.
    /// Failing before (the scan returned locks only, symlink skips were
    /// silent), passing after.
    #[test]
    fn lock_scan_proof_records_namespaces_and_symlink_skips() {
        let directory = tempfile::TempDir::new().unwrap();
        let git_dir = directory.path().join("git");
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        fs::create_dir_all(git_dir.join("reftable")).unwrap();
        let (locks, proof) = git_locks_present_proved(&git_dir, &git_dir).unwrap();
        assert!(locks.is_empty());
        assert!(
            proof.is_complete(),
            "the proof must name all ten namespaces: {:?}",
            proof.inspected
        );
        assert!(proof.skipped_symlink_dirs.is_empty());

        // A symlinked subdirectory is SKIPPED and RECORDED (never recursed
        // into, never silently dropped).
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(directory.path(), git_dir.join("refs/looped")).unwrap();
            let (_, proof) = git_locks_present_proved(&git_dir, &git_dir).unwrap();
            assert!(
                proof
                    .skipped_symlink_dirs
                    .iter()
                    .any(|skipped| skipped.contains("looped")),
                "the symlink skip must be recorded: {:?}",
                proof.skipped_symlink_dirs
            );
        }
    }

    /// CB-13D ::24 AC-2 check-to-entry race fixture: a Git-owned
    /// transaction lock that appears between the caller's quiescence check
    /// and the workspace entry is caught by the entry's own lease
    /// re-proof under the metadata-only budget (the caller-side fence and
    /// the entry-side fence are different observations).
    #[test]
    fn late_git_lock_refuses_the_metadata_only_entry() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        let git_dir = directory.path().join("race-git");
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        fs::write(git_dir.join("HEAD.lock"), b"race").unwrap();

        let mut base = fake_git("stable", b"one");
        if let WorkspaceGitObservation::Repository(git) = &mut base {
            git.common_dir = git_dir.clone();
            git.worktree_git_dir = git_dir.clone();
        }

        let start = repo
            .begin_remediation_txn_budgeted_with(
                |_| Ok(base.clone()),
                ReconcileEffectBudget::MetadataOnly,
            )
            .unwrap();
        match start {
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::Unanchored {
                state: UnanchoredWorkspace::GitBusy { reason, .. },
                ..
            }) => assert_eq!(reason, "ref_locks"),
            other => panic!("the late lock must refuse the entry: {other:?}"),
        }
    }

    #[test]
    fn metadata_only_entry_revalidates_git_locks_under_the_lease() {
        let directory = tempfile::TempDir::new().unwrap();
        let mut repo = Repository::init(directory.path()).unwrap();
        let race_git_dir = directory.path().join(".git");
        fs::create_dir_all(&race_git_dir).unwrap();
        let mut base = fake_git("stable", b"one");
        if let WorkspaceGitObservation::Repository(git) = &mut base {
            git.common_dir = race_git_dir.clone();
            git.worktree_git_dir = race_git_dir.clone();
        }
        let mut calls = 0u32;
        let injector_git_dir = race_git_dir.clone();
        let start = repo
            .begin_remediation_txn_budgeted_with(
                move |_| {
                    calls += 1;
                    if calls >= 3 {
                        // The race: HEAD.lock appears between the last
                        // pre-entry observation and the effect boundary.
                        let _ = fs::write(injector_git_dir.join("HEAD.lock"), b"");
                    }
                    Ok(base.clone())
                },
                ReconcileEffectBudget::MetadataOnly,
            )
            .unwrap();
        match start {
            WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitLocksBusy {
                reason, ..
            }) => assert_eq!(reason, "ref_locks"),
            other => panic!("the lease revalidation must fence the injected lock: {other:?}"),
        }
        // Cleanup for the Drop guard.
        let _ = fs::remove_file(race_git_dir.join("HEAD.lock"));

        // The command budget keeps its own semantics: the same observations
        // (minus the race injection) enter ready as before.
        let start = repo
            .begin_remediation_txn_with(|_| {
                let mut aligned = fake_git("stable", b"one");
                if let WorkspaceGitObservation::Repository(git) = &mut aligned {
                    git.common_dir = race_git_dir.clone();
                    git.worktree_git_dir = race_git_dir.clone();
                }
                Ok(aligned)
            })
            .unwrap();
        assert!(matches!(start, WorkspaceTxnStart::Ready(_)));
    }
}

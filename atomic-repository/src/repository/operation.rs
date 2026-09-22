//! Repository-level operation preparation and lease-safe recovery.
//!
//! This module deliberately owns orchestration rather than command routing. Callers
//! prepare and durably publish an immutable operation before performing any external
//! effect, record deterministic receipts after effects, and invoke recovery while the
//! same ordered per-working-copy operation lock is held.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use atomic_core::operation::{
    ActorRef, CheckpointKind, DigestKind, EffectPlan, EffectReceipt, EffectReceiptKind,
    EffectReceiptPayload, EffectTarget, EffectValue, FileKind, FileState, GitHashAlgorithm,
    GitHeadState, GitObjectId, GitRefTarget, MetadataTarget, MetadataTransition, MetadataValue,
    Operation, OperationKind, OperationPayload, OperationRelation, OperationScope, RepoStateDelta,
    RepoStateRef, WorkingCopyStateRef,
};
use atomic_core::pristine::{
    CapabilityMutTxnT, GraphTxnT, MutTxnT, OperationMutTxnT, OperationTxnT, RefMappingMutTxnT,
    RefMappingTxnT, TagMutTxnT, TagTxnT, ViewTxnT, WorkingCopyMutTxnT, WorkingCopyRecord,
    WorkingCopyTxnT, SUPPORTED_REPOSITORY_CAPABILITIES,
};
use atomic_core::types::Base32;
use atomic_core::{Hash, OperationId, WorkingCopyId};

use super::locks::WorkingCopyOperationLockGuard;
use super::workspace_txn::{
    read_workspace_checkpoint, write_workspace_checkpoint, WorkspaceCheckpoint,
};
use super::Repository;
use crate::RepositoryError;

const RECOVERY_DIR: &str = "operation-recovery";
const BACKUP_ENTRIES_DIR: &str = "entries";
const BACKUP_VALUE: &str = "value";
const BACKUP_ABSENT: &str = "absent";
const BACKUP_COMPLETE: &str = "complete";
/// CB-8B: marks a retained Git-index backup whose expected-old was an absent
/// index file (the rollback re-removes the index instead of restoring bytes).
const RECOVERY_ABSENT_MARKER: &str = "absent";
const BACKUP_VERSION: &[u8] = b"atomic-operation-recovery-v1\n";
const RECEIPT_ATTEMPT: u32 = 0;
const ANCHOR_ACTOR: &str = "cb-1b-anchor";
const RECOVERY_ACTOR: &str = "cb-1b-recovery";
const CONSOLIDATION_ACTOR: &str = "cb-1c-head-consolidation";
const MIN_OPERATION_PREFIX_LEN: usize = 4;

/// Completion state derived from immutable receipts for one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationVerificationState {
    /// The operation is durable but has no successful effect receipt yet.
    Prepared,
    /// At least one effect landed, but operation-level verification is absent.
    InProgress,
    /// An effect lease was rejected because the observed value diverged.
    LeaseRejected,
    /// Every effect completed and the operation has a final verified receipt.
    Verified,
}

/// Current causal state of one operation scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationHeadState {
    /// No operation history exists for the scope.
    Empty,
    /// Exactly one causal head exists.
    Single(OperationId),
    /// Multiple heads remain and no combined state is implied.
    Diverged(Vec<OperationId>),
}

/// One operation in deterministic causal log order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationLogEntry {
    pub operation: Operation,
    pub verification: OperationVerificationState,
    pub is_head: bool,
}

/// Reachable operation history for one mutable scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationLog {
    pub scope: OperationScope,
    pub head_state: OperationHeadState,
    pub entries: Vec<OperationLogEntry>,
}

/// Full immutable operation data plus its derived receipt/head state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationDetails {
    pub operation: Operation,
    pub receipts: Vec<EffectReceipt>,
    pub verification: OperationVerificationState,
    pub head_of: Vec<OperationScope>,
}

/// Prepared remote operation that retains the ordered locks through protocol I/O.
pub struct PreparedRemoteOperation {
    operation_id: OperationId,
    operation_lock: WorkingCopyOperationLockGuard,
}

impl PreparedRemoteOperation {
    /// Immutable operation identity published before the remote mutation.
    pub fn id(&self) -> OperationId {
        self.operation_id
    }
}

/// A journaled bridge-owned Git ref write awaiting its leased effect receipt.
///
/// The operation (and therefore its operation ID and intended effect) is
/// durable before the caller performs the Git mutation; retaining this value
/// keeps the ordered operation locks held until the receipt is recorded.
pub struct PreparedBridgeGitWrite {
    /// Immutable operation identity journaled before the Git write.
    pub operation_id: OperationId,
    /// Git ref the mutation targets (for example `refs/heads/main`).
    pub ref_name: String,
    /// Ref target observed before the write, when the ref existed.
    pub(super) observed_old: Option<GitRefTarget>,
    /// Intended post-write target of the ref.
    pub intended_new: GitRefTarget,
    /// The capture token minted for this write's capture context (review
    /// E2), or `None` for an ordinary ref movement that is not a rewrite
    /// execution (CB-9B F2). An executor driving Git hooks inside this
    /// write's capture context exports it as `ATOMIC_BRIDGE_CAPTURE_TOKEN`;
    /// only captured events carrying this exact token can later anchor to
    /// the operation.
    pub capture_token: Option<[u8; 32]>,
    pub(super) _operation_lock: WorkingCopyOperationLockGuard,
}

/// Pure classification of an observed value against one expected-old/new lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LeaseClassification {
    /// The old lease still holds and the effect may be applied.
    Apply,
    /// The new lease already holds; replay is an idempotent no-op.
    AlreadyApplied,
    /// Neither lease holds; touching the resource could overwrite newer work.
    Diverged,
}

/// Result of preparing a switch operation and its recovery substrate.
#[derive(Debug, Clone)]
pub(super) struct PreparedSwitchOperation {
    pub(super) operation: Operation,
    backup_root: PathBuf,
}

impl PreparedSwitchOperation {
    pub(super) fn operation(&self) -> &Operation {
        &self.operation
    }

    pub(super) fn backup_root(&self) -> &Path {
        &self.backup_root
    }
}

/// A landed filesystem effect awaiting its immutable receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingFilesystemEffect {
    ordinal: u32,
    observed_before: EffectValue,
    observed_after: EffectValue,
    mutated: bool,
}

impl PendingFilesystemEffect {
    pub(super) fn mutated(&self) -> bool {
        self.mutated
    }
}

/// Outcome of checking or recovering the current operation head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RecoveryOutcome {
    /// No operation has been prepared for this working copy yet.
    NoOperation,
    /// The current head already has an operation-level `Verified` receipt.
    AlreadyComplete { operation: OperationId },
    /// An inverse recovery operation was created or resumed and completed.
    Recovered {
        original: OperationId,
        recovery: OperationId,
        created: bool,
    },
}

#[derive(Debug)]
#[allow(dead_code)] // discriminants select remediation; payloads surface via Debug
enum ResolvedRecoveryTarget {
    Filesystem {
        path: PathBuf,
        recursive_directory: bool,
    },
    WorkingCopy(WorkingCopyId),
    /// A Git reference observed/rolled back through the lease-safe bridge
    /// reader (R3: the adoption completion's WIP-ref drop; CB-8B: projection
    /// refs and shared branches).
    GitRef(String),
    /// The working copy's Git HEAD, observed/rolled back through the
    /// lease-safe bridge reader (CB-8B projection publication).
    GitHead(WorkingCopyId),
    /// The derived bridge checkpoint file, addressed by content digest
    /// (R3: the adoption completion's verified checkpoint publish).
    Checkpoint {
        working_copy: WorkingCopyId,
    },
    /// One content-addressed Git object creation (CB-8B: journaled projection
    /// objects). Presence is idempotent; the object is never deleted.
    GitObject(atomic_core::operation::GitObjectId),
    /// The working copy's replaced Git index (CB-8B: journaled projection
    /// index replacement).
    GitIndex(WorkingCopyId),
}

/// Classify one observation without performing I/O or consulting receipts.
pub(super) fn filesystem_directory_value(mode: u32) -> EffectValue {
    EffectValue::File(FileState {
        kind: FileKind::Directory,
        mode,
        content: Hash::of(b"atomic:filesystem-directory-entry:v1\0"),
    })
}

pub(super) fn classify_effect_lease(
    observed: &EffectValue,
    expected_old: &EffectValue,
    expected_new: &EffectValue,
) -> LeaseClassification {
    if observed == expected_old {
        LeaseClassification::Apply
    } else if observed == expected_new {
        LeaseClassification::AlreadyApplied
    } else {
        LeaseClassification::Diverged
    }
}

/// Construct a stable content-addressed receipt for one operation.
///
/// Attempt and timestamp are derived from immutable operation data, so replaying the
/// same recovery decision produces the same receipt ID and append-only storage dedups it.
pub(super) fn deterministic_effect_receipt(
    operation: &Operation,
    effect_ordinal: Option<u32>,
    kind: EffectReceiptKind,
    observed_old: Option<EffectValue>,
    observed_new: Option<EffectValue>,
) -> Result<EffectReceipt, RepositoryError> {
    EffectReceipt::new(EffectReceiptPayload {
        operation: operation.id(),
        effect_ordinal,
        attempt: RECEIPT_ATTEMPT,
        kind,
        observed_old,
        observed_new,
        timestamp_ms: operation.payload().timestamp_ms,
    })
    .map_err(codec_error)
}

/// Whether a receipt set contains the immutable operation-level completion receipt.
pub(super) fn has_operation_verified_receipt(receipts: &[EffectReceipt]) -> bool {
    receipts.iter().any(|receipt| {
        receipt.payload().kind == EffectReceiptKind::Verified
            && receipt.payload().effect_ordinal.is_none()
    })
}

impl Repository {
    /// Resolve a full operation identity or unambiguous Base32 prefix.
    pub fn resolve_operation_id(&self, selector: &str) -> Result<OperationId, RepositoryError> {
        let selector = selector.trim();
        let normalized = selector.to_ascii_uppercase();
        if normalized.len() < MIN_OPERATION_PREFIX_LEN {
            return Err(RepositoryError::InvalidOperationSelector {
                selector: selector.to_string(),
                reason: format!(
                    "prefixes must contain at least {MIN_OPERATION_PREFIX_LEN} Base32 characters"
                ),
            });
        }
        if normalized.len() > 52
            || !normalized
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || (b'2'..=b'7').contains(&byte))
        {
            return Err(RepositoryError::InvalidOperationSelector {
                selector: selector.to_string(),
                reason: "expected a Base32 operation identity or prefix".to_string(),
            });
        }

        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let mut matches: Vec<OperationId> = txn
            .list_operations()
            .map_err(pristine_error)?
            .into_iter()
            .map(|operation| operation.id())
            .filter(|id| id.to_string().starts_with(&normalized))
            .collect();
        matches.sort_unstable();
        match matches.as_slice() {
            [id] => Ok(*id),
            [] => Err(RepositoryError::OperationNotFound {
                selector: selector.to_string(),
            }),
            _ => Err(RepositoryError::AmbiguousOperation {
                prefix: selector.to_string(),
                matches: matches.iter().map(ToString::to_string).collect(),
            }),
        }
    }

    /// Return full operation data, receipts, verification, and current head scopes.
    pub fn operation_details(
        &self,
        operation_id: OperationId,
    ) -> Result<OperationDetails, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let operation = txn
            .get_operation(operation_id)
            .map_err(pristine_error)?
            .ok_or_else(|| RepositoryError::OperationNotFound {
                selector: operation_id.to_string(),
            })?;
        let mut receipts = txn
            .get_effect_receipts(operation_id)
            .map_err(pristine_error)?;
        sort_receipts(&mut receipts);
        let verification = operation_verification_state(&receipts);
        let mut head_of: Vec<OperationScope> = txn
            .list_operation_heads()
            .map_err(pristine_error)?
            .into_iter()
            .filter_map(|(scope, heads)| heads.as_slice().contains(&operation_id).then_some(scope))
            .collect();
        head_of.sort_unstable();
        Ok(OperationDetails {
            operation,
            receipts,
            verification,
            head_of,
        })
    }

    /// Prepare a native remote command before its protocol mutation begins.
    pub fn prepare_remote_operation(
        &self,
        working_copy: WorkingCopyId,
        kind: OperationKind,
        remote: &str,
        evidence: Hash,
    ) -> Result<PreparedRemoteOperation, RepositoryError> {
        if !matches!(kind, OperationKind::Pull | OperationKind::Push) {
            return Err(RepositoryError::InvalidOperation {
                message: format!("{kind:?} is not a native remote operation kind"),
            });
        }
        let operation_lock = self.try_lock_operation(working_copy)?;
        if let OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: OperationScope::WorkingCopy(working_copy).to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let state = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            let record = txn
                .get_working_copy(working_copy)
                .map_err(pristine_error)?
                .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?;
            let view = ViewTxnT::get_view_by_id(&txn, record.desired_view)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::InvalidRepository {
                    reason: format!(
                        "working copy {working_copy} references missing view {}",
                        record.desired_view
                    ),
                })?;
            RepoStateRef {
                view: Some(atomic_core::operation::ViewStateRef {
                    name: view.name,
                    state: view.state,
                    set_id: None,
                }),
                working_copy: Some(working_copy_state_ref(record)),
                git: None,
            }
        };
        let operation = self.prepare_metadata_operation(
            &operation_lock,
            kind,
            None,
            state.clone(),
            state,
            Vec::new(),
            vec![
                evidence,
                Hash::of(format!("atomic:remote:v1\\0{remote}").as_bytes()),
            ],
            ActorRef::System {
                name: format!(
                    "repository-{}",
                    if kind == OperationKind::Pull {
                        "pull"
                    } else {
                        "push"
                    }
                ),
            },
            current_operation_timestamp_ms(),
        )?;
        Ok(PreparedRemoteOperation {
            operation_id: operation.id(),
            operation_lock,
        })
    }

    /// Mark a prepared remote command verified after protocol-level confirmation.
    pub fn finalize_remote_operation(
        &self,
        prepared: PreparedRemoteOperation,
    ) -> Result<(), RepositoryError> {
        let operation_id = prepared.operation_id;
        let working_copy = prepared.operation_lock.working_copy();
        let actual = self.sole_operation_head(OperationScope::WorkingCopy(working_copy))?;
        if actual != operation_id {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "remote operation head changed before verification: expected {operation_id}, found {actual}"
                ),
            });
        }
        let operation = self.load_operation(operation_id)?;
        if !matches!(
            operation.payload().kind,
            OperationKind::Pull | OperationKind::Push
        ) {
            return Err(RepositoryError::InvalidOperation {
                message: format!("operation {operation_id} is not a native remote operation"),
            });
        }
        self.finalize_operation_verified(&prepared.operation_lock, operation_id)?;
        Ok(())
    }

    /// Append a verified remote observation when no protocol mutation follows.
    pub fn append_verified_remote_operation(
        &self,
        working_copy: WorkingCopyId,
        kind: OperationKind,
        remote: &str,
        evidence: Hash,
    ) -> Result<OperationId, RepositoryError> {
        let prepared = self.prepare_remote_operation(working_copy, kind, remote, evidence)?;
        let operation_id = prepared.id();
        self.finalize_remote_operation(prepared)?;
        Ok(operation_id)
    }

    /// Journal one bridge-owned Git ref mutation before it becomes visible.
    ///
    /// The returned value carries the immutable operation ID and the exact
    /// intended effect. Journal failure returns an error and the caller must
    /// not perform the Git write. The lease observes `observed_old` as
    /// provided by the caller (the bridge owns Git observation through its
    /// own handle); a later receipt classifies the outcome against the lease.
    ///
    /// This is an ORDINARY ref movement, not a rewrite execution: it does NOT
    /// mint a capture token, so a post-rewrite hook event can never anchor to
    /// it (CB-9B F2). A caller that is actually executing a Git rewrite must
    /// use [`Self::prepare_bridge_git_rewrite`].
    pub fn prepare_bridge_git_ref_write(
        &self,
        working_copy: WorkingCopyId,
        ref_name: &str,
        observed_old: Option<GitRefTarget>,
        intended_new: GitRefTarget,
        evidence: Hash,
    ) -> Result<PreparedBridgeGitWrite, RepositoryError> {
        self.prepare_bridge_git_write_impl(
            false,
            working_copy,
            ref_name,
            observed_old,
            intended_new,
            evidence,
            Vec::new(),
        )
    }

    /// [`Self::prepare_bridge_git_ref_write`] with a companion durable
    /// metadata intent (CB-10A review R3): the SAME journaled operation
    /// carries the ref effect lease AND the metadata transitions, so a ref
    /// movement is never visible without its recoverable mapping intent —
    /// the mapping intent is durable before the ref write and is applied
    /// only after the ref effect's receipt (see
    /// [`Self::complete_bridge_git_write_with_metadata`]). The metadata
    /// leases are validated at prepare time: a mapping that moved between
    /// the caller's observation and this preparation refuses closed.
    pub fn prepare_bridge_git_ref_write_with_metadata(
        &self,
        working_copy: WorkingCopyId,
        ref_name: &str,
        observed_old: Option<GitRefTarget>,
        intended_new: GitRefTarget,
        evidence: Hash,
        metadata: Vec<atomic_core::operation::MetadataTransition>,
    ) -> Result<PreparedBridgeGitWrite, RepositoryError> {
        self.prepare_bridge_git_write_impl(
            false,
            working_copy,
            ref_name,
            observed_old,
            intended_new,
            evidence,
            metadata,
        )
    }

    /// [`Self::finalize_bridge_git_write`] for a write prepared with
    /// companion metadata (CB-10A review R3): the metadata transitions are
    /// applied under the held operation lock BEFORE the operation-level
    /// Verified receipt, so the mapping intent lands exactly once — after
    /// the ref effect it accompanies.
    pub fn complete_bridge_git_write_with_metadata(
        &self,
        prepared: PreparedBridgeGitWrite,
        observed_ref: Option<GitRefTarget>,
    ) -> Result<OperationId, RepositoryError> {
        self.apply_operation_metadata_locked(&prepared._operation_lock, prepared.operation_id)?;
        self.finalize_bridge_git_write(prepared, observed_ref)
    }

    /// Journal a bridge-owned Git REWRITE execution before it becomes
    /// visible, minting the operation's immutable capture context.
    ///
    /// Only a genuine rewrite executor (one that actually ran the Git rewrite
    /// and will move the ref to the rewritten target) calls this. The minted
    /// token is the unforgeable binding between a hook capture produced during
    /// this operation's execution and the operation itself (CB-9B F2): an
    /// ordinary ref movement prepared through
    /// [`Self::prepare_bridge_git_ref_write`] carries no capture context and
    /// can never be promoted into rewrite authority.
    pub fn prepare_bridge_git_rewrite(
        &self,
        working_copy: WorkingCopyId,
        ref_name: &str,
        observed_old: Option<GitRefTarget>,
        intended_new: GitRefTarget,
        evidence: Hash,
    ) -> Result<PreparedBridgeGitWrite, RepositoryError> {
        self.prepare_bridge_git_write_impl(
            true,
            working_copy,
            ref_name,
            observed_old,
            intended_new,
            evidence,
            Vec::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_bridge_git_write_impl(
        &self,
        mint_capture: bool,
        working_copy: WorkingCopyId,
        ref_name: &str,
        observed_old: Option<GitRefTarget>,
        intended_new: GitRefTarget,
        evidence: Hash,
        metadata: Vec<atomic_core::operation::MetadataTransition>,
    ) -> Result<PreparedBridgeGitWrite, RepositoryError> {
        let operation_lock = self.try_lock_operation(working_copy)?;
        if let OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: OperationScope::WorkingCopy(working_copy).to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        let state = self.current_working_copy_state(working_copy)?;
        let effects = vec![EffectPlan {
            ordinal: 0,
            target: EffectTarget::GitRef {
                name: ref_name.to_string(),
            },
            expected_old: observed_old
                .clone()
                .map(EffectValue::GitRef)
                .unwrap_or(EffectValue::Absent),
            expected_new: EffectValue::GitRef(intended_new.clone()),
        }];
        let operation = self.prepare_working_copy_transition_with_metadata(
            &operation_lock,
            OperationKind::ExportGitRefs,
            None,
            state.clone(),
            state,
            effects,
            metadata,
            vec![evidence],
            ActorRef::System {
                name: "repository-bridge-git".to_string(),
            },
            current_operation_timestamp_ms(),
        )?;
        // Review E2: mint this write's capture context NOW, at preparation
        // time, but ONLY for a rewrite execution (CB-9B F2). An ordinary ref
        // movement is not a rewrite and carries no capture context, so a
        // post-rewrite hook event can never anchor to it: the captured event
        // must carry this exact token, and a capture created before the
        // operation existed (the advisory hook path) can never be
        // retroactively promoted onto it. The token row is written while the
        // operation lock is still held; a crash before it lands leaves an
        // operation with no capture context, which fails closed (captures can
        // never anchor to it).
        let capture_token = if mint_capture {
            let mut token = [0u8; 32];
            token[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            token[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            {
                let mut txn = self.pristine.write_txn().map_err(pristine_error)?;
                use atomic_core::pristine::BridgeEventCaptureMutTxnT;
                txn.put_bridge_ref_capture_token(operation.operation.id().as_bytes(), &token)
                    .map_err(pristine_error)?;
                txn.commit()
                    .map_err(|e| RepositoryError::Database(e.to_string()))?;
            }
            Some(token)
        } else {
            None
        };
        Ok(PreparedBridgeGitWrite {
            operation_id: operation.operation.id(),
            _operation_lock: operation_lock,
            ref_name: ref_name.to_string(),
            observed_old,
            intended_new,
            capture_token,
        })
    }

    /// Append the leased effect receipt after a journaled bridge Git write.
    ///
    /// `observed_after` is what Git now shows for the ref. Values matching the
    /// intended target append an Applied (or idempotent Recovered) receipt;
    /// any third value appends a durable rejection receipt and fails closed.
    pub fn record_bridge_git_ref_receipt(
        &self,
        prepared: &PreparedBridgeGitWrite,
        observed_after: Option<GitRefTarget>,
    ) -> Result<EffectReceipt, RepositoryError> {
        let observed_after = observed_after
            .map(EffectValue::GitRef)
            .unwrap_or(EffectValue::Absent);
        let observed_before = prepared
            .observed_old
            .clone()
            .map(EffectValue::GitRef)
            .unwrap_or(EffectValue::Absent);
        self.record_effect_outcome(
            &prepared._operation_lock,
            prepared.operation_id,
            0,
            observed_before,
            observed_after,
        )
    }

    /// Append the operation-level Verified receipt for a completed bridge write.
    ///
    /// The GitRef effect lease is verified against `observed_ref`, which the
    /// bridge reads back from Git after the mutation (the lease-safe executor
    /// the recovery layer requires). Any third value fails closed without a
    /// Verified receipt.
    pub fn finalize_bridge_git_write(
        &self,
        prepared: PreparedBridgeGitWrite,
        observed_ref: Option<GitRefTarget>,
    ) -> Result<OperationId, RepositoryError> {
        let operation_id = prepared.operation_id;
        let operation = self.load_operation(operation_id)?;
        self.validate_operation_lock(&prepared._operation_lock, &operation)?;
        let receipts = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.get_effect_receipts(operation_id)
                .map_err(pristine_error)?
        };
        let completed: BTreeSet<u32> = receipts
            .iter()
            .filter_map(|receipt| match receipt.payload().kind {
                EffectReceiptKind::Applied
                | EffectReceiptKind::Recovered
                | EffectReceiptKind::RolledBack => receipt.payload().effect_ordinal,
                EffectReceiptKind::Verified | EffectReceiptKind::LeaseRejected => None,
            })
            .collect();
        for effect in &operation.payload().delta.effects {
            if !completed.contains(&effect.ordinal) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation {} cannot be verified before effect {} has a successful receipt",
                        operation.id(),
                        effect.ordinal
                    ),
                });
            }
        }
        let observed = observed_ref
            .map(EffectValue::GitRef)
            .unwrap_or(EffectValue::Absent);
        for effect in &operation.payload().delta.effects {
            if observed != effect.expected_new {
                return Err(lease_divergence_error(
                    effect,
                    &EffectValue::Absent,
                    Some(&observed),
                ));
            }
        }
        let receipt = deterministic_effect_receipt(
            &operation,
            None,
            EffectReceiptKind::Verified,
            None,
            None,
        )?;
        self.append_receipt_immediate(&prepared._operation_lock, &operation, &receipt)?;
        Ok(operation_id)
    }

    pub(super) fn current_working_copy_state(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<RepoStateRef, RepositoryError> {
        self.current_working_copy_state_for_projection(working_copy)
    }

    pub(super) fn current_working_copy_state_for_projection(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<RepoStateRef, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let record = txn
            .get_working_copy(working_copy)
            .map_err(pristine_error)?
            .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?;
        let view = ViewTxnT::get_view_by_id(&txn, record.desired_view)
            .map_err(pristine_error)?
            .ok_or_else(|| RepositoryError::InvalidRepository {
                reason: format!(
                    "working copy {working_copy} references missing view {}",
                    record.desired_view
                ),
            })?;
        Ok(RepoStateRef {
            view: Some(atomic_core::operation::ViewStateRef {
                name: view.name,
                state: view.state,
                set_id: None,
            }),
            working_copy: Some(working_copy_state_ref(record)),
            git: None,
        })
    }

    /// Append an inverse operation for the current verified working-copy head.
    ///
    /// The optional target must be the sole current head. This first implementation
    /// supports operations whose inverse is a view/materialization transition; other
    /// metadata kinds are refused until their routed deltas are available.
    pub fn undo_operation(
        &mut self,
        working_copy: WorkingCopyId,
        target: Option<OperationId>,
    ) -> Result<OperationId, RepositoryError> {
        let scope = OperationScope::WorkingCopy(working_copy);
        let current = self.sole_operation_head(scope)?;
        let target = target.unwrap_or(current);
        if target != current {
            return Err(RepositoryError::OperationNotReversible {
                operation: target.to_string(),
                kind: "historical".to_string(),
                reason: format!(
                    "undo requires the sole current head {}; use restore for an earlier state",
                    current
                ),
            });
        }
        let details = self.operation_details(target)?;
        if details.verification != OperationVerificationState::Verified {
            return Err(RepositoryError::OperationNotVerified {
                operation: target.to_string(),
            });
        }
        if !details.operation.payload().delta.metadata.is_empty() {
            return self.append_related_metadata_operation(
                working_copy,
                &details.operation,
                OperationKind::Undo,
                OperationRelation::Undo { target },
                true,
            );
        }
        if !matches!(
            details.operation.payload().kind,
            OperationKind::SwitchView | OperationKind::Undo | OperationKind::Restore
        ) {
            return Err(operation_not_reversible(
                &details.operation,
                "no typed inverse metadata transition is available",
            ));
        }
        let view = details
            .operation
            .payload()
            .before
            .view
            .as_ref()
            .map(|view| view.name.clone())
            .ok_or_else(|| {
                operation_not_reversible(
                    &details.operation,
                    "the operation does not carry a prior view state",
                )
            })?;
        self.switch_view_with_operation(
            working_copy,
            &view,
            OperationKind::Undo,
            Some(OperationRelation::Undo { target }),
            ActorRef::System {
                name: "operation-undo".to_string(),
            },
            Some(current),
        )?;
        self.sole_operation_head(scope)
    }

    /// Re-project the verified state selected by an earlier reachable operation.
    pub fn restore_operation(
        &mut self,
        working_copy: WorkingCopyId,
        target: OperationId,
    ) -> Result<OperationId, RepositoryError> {
        let scope = OperationScope::WorkingCopy(working_copy);
        let current_head = self.sole_operation_head(scope)?;
        let history = self.operation_log(scope, None, false)?;
        if !history
            .entries
            .iter()
            .any(|entry| entry.operation.id() == target)
        {
            return Err(RepositoryError::OperationNotReachable {
                operation: target.to_string(),
                scope: scope.to_string(),
            });
        }
        let details = self.operation_details(target)?;
        if details.verification != OperationVerificationState::Verified {
            return Err(RepositoryError::OperationNotVerified {
                operation: target.to_string(),
            });
        }
        if !details.operation.payload().delta.metadata.is_empty() {
            return self.append_related_metadata_operation(
                working_copy,
                &details.operation,
                OperationKind::Restore,
                OperationRelation::Restore { target },
                false,
            );
        }
        let view = details
            .operation
            .payload()
            .delta
            .after
            .view
            .as_ref()
            .map(|view| view.name.clone())
            .ok_or_else(|| {
                operation_not_reversible(
                    &details.operation,
                    "the operation does not carry a selected view state",
                )
            })?;
        let expected_state = details
            .operation
            .payload()
            .delta
            .after
            .view
            .as_ref()
            .map(|state| state.state)
            .ok_or_else(|| {
                operation_not_reversible(
                    &details.operation,
                    "the operation does not carry a selected view state",
                )
            })?;
        let observed_state = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.get_view(&view)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ViewNotFound { name: view.clone() })?
                .state
        };
        if observed_state != expected_state {
            return Err(operation_not_reversible(
                &details.operation,
                "the selected view has advanced beyond the historical state",
            ));
        }
        self.switch_view_with_operation(
            working_copy,
            &view,
            OperationKind::Restore,
            Some(OperationRelation::Restore { target }),
            ActorRef::System {
                name: "operation-restore".to_string(),
            },
            Some(current_head),
        )?;
        self.sole_operation_head(scope)
    }

    fn metadata_restore_transitions(
        &self,
        current: OperationId,
        target: OperationId,
    ) -> Result<Vec<MetadataTransition>, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let operations: BTreeMap<OperationId, Operation> = txn
            .list_operations()
            .map_err(pristine_error)?
            .into_iter()
            .map(|operation| (operation.id(), operation))
            .collect();
        drop(txn);
        let mut memo = BTreeMap::new();
        let mut visiting = BTreeSet::new();
        let current_snapshot =
            fold_operation_metadata_snapshot(current, &operations, &mut memo, &mut visiting)?;
        visiting.clear();
        let target_snapshot =
            fold_operation_metadata_snapshot(target, &operations, &mut memo, &mut visiting)?;
        let targets: BTreeSet<MetadataTarget> = current_snapshot
            .keys()
            .chain(target_snapshot.keys())
            .cloned()
            .collect();
        let mut transitions = Vec::new();
        for metadata_target in targets {
            let observed = self.observe_metadata_value(&metadata_target)?;
            let expected_new = target_snapshot
                .get(&metadata_target)
                .cloned()
                .unwrap_or(MetadataValue::Absent);
            if matches!(
                (&observed, &expected_new),
                (MetadataValue::Sequence(_), MetadataValue::Sequence(_))
            ) {
                continue;
            }
            if observed != expected_new {
                transitions.push(MetadataTransition {
                    target: metadata_target,
                    expected_old: observed,
                    expected_new,
                });
            }
        }
        Ok(transitions)
    }

    fn append_related_metadata_operation(
        &mut self,
        working_copy: WorkingCopyId,
        target: &Operation,
        kind: OperationKind,
        relation: OperationRelation,
        inverse: bool,
    ) -> Result<OperationId, RepositoryError> {
        let scope = OperationScope::WorkingCopy(working_copy);
        let current_id = self.sole_operation_head(scope)?;
        let current = self.operation_details(current_id)?.operation;
        let operation_lock = self.try_lock_operation(working_copy)?;
        if let OperationHeadState::Diverged(heads) =
            self.consolidate_operation_heads_locked(&operation_lock)?
        {
            return Err(RepositoryError::OperationHeadsDiverged {
                scope: scope.to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            });
        }
        if self.sole_operation_head(scope)? != current_id {
            return Err(RepositoryError::InvalidOperation {
                message: "operation head changed while preparing inverse metadata".to_string(),
            });
        }
        let metadata = if inverse {
            target
                .payload()
                .delta
                .metadata
                .iter()
                .map(|transition| MetadataTransition {
                    target: transition.target.clone(),
                    expected_old: transition.expected_new.clone(),
                    expected_new: transition.expected_old.clone(),
                })
                .collect()
        } else {
            self.metadata_restore_transitions(current_id, target.id())?
        };
        let after = if inverse {
            target.payload().before.clone()
        } else {
            target.payload().delta.after.clone()
        };
        // CB-13B R2: the undo of an effect-bearing operation carries the
        // swapped effect plans and replays them from the original's
        // retained backups, so a cutover's hook/config decommission
        // restores under its exact leases. Restore stays metadata-only:
        // replaying an arbitrary historical operation's effects forward
        // is not supported by this build.
        let inverse_effects = if inverse {
            if target.payload().delta.effects.is_empty() {
                Vec::new()
            } else {
                let selected = self.selected_inverse_ordinals(working_copy, target)?;
                if selected.is_empty() {
                    Vec::new()
                } else {
                    target
                        .payload()
                        .delta
                        .effects
                        .iter()
                        .rev()
                        .filter(|effect| selected.contains(&effect.ordinal))
                        .enumerate()
                        .map(|(ordinal, effect)| {
                            Ok(EffectPlan {
                                ordinal: u32::try_from(ordinal).map_err(|_| {
                                    RepositoryError::InvalidOperation {
                                        message: "too many effects to construct the undo operation"
                                            .to_string(),
                                    }
                                })?,
                                target: effect.target.clone(),
                                expected_old: effect.expected_new.clone(),
                                expected_new: effect.expected_old.clone(),
                            })
                        })
                        .collect::<Result<Vec<_>, RepositoryError>>()?
                }
            }
        } else {
            if !target.payload().delta.effects.is_empty() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation {} carries external effects; restore supports \
                         metadata-only operations",
                        target.id()
                    ),
                });
            }
            Vec::new()
        };
        if !inverse && metadata.is_empty() {
            if let Some(expected_view) = &after.view {
                let txn = self.pristine.read_txn().map_err(pristine_error)?;
                let observed = txn
                    .get_view(&expected_view.name)
                    .map_err(pristine_error)?
                    .ok_or_else(|| RepositoryError::ViewNotFound {
                        name: expected_view.name.clone(),
                    })?;
                if observed.state != expected_view.state {
                    return Err(operation_not_reversible(
                        target,
                        "historical ordering cannot be reconstructed without changing membership",
                    ));
                }
            }
        }
        let operation = if inverse_effects.is_empty() {
            self.prepare_metadata_operation(
                &operation_lock,
                kind,
                Some(relation),
                current.payload().delta.after.clone(),
                after,
                metadata,
                target.payload().evidence.clone(),
                ActorRef::System {
                    name: if inverse {
                        "operation-undo".to_string()
                    } else {
                        "operation-restore".to_string()
                    },
                },
                current_operation_timestamp_ms(),
            )?
        } else {
            self.prepare_working_copy_transition_with_metadata(
                &operation_lock,
                kind,
                Some(relation),
                current.payload().delta.after.clone(),
                after,
                inverse_effects.clone(),
                metadata,
                target.payload().evidence.clone(),
                ActorRef::System {
                    name: "operation-undo".to_string(),
                },
                current_operation_timestamp_ms(),
            )?
            .operation
        };
        self.apply_operation_metadata_locked(&operation_lock, operation.id())?;
        if !inverse_effects.is_empty() {
            // Replay the undo's effects from the original operation's
            // retained backups: every restoration is leased, and a
            // divergent value fails closed instead of overwriting newer
            // external work.
            self.replay_filesystem_recovery(&operation_lock, target, &operation)?;
        }
        self.finalize_operation_verified(&operation_lock, operation.id())?;
        let operation_id = operation.id();
        drop(operation_lock);
        if !inverse {
            self.materialize(working_copy)?;
        }
        Ok(operation_id)
    }

    pub(super) fn sole_operation_head(
        &self,
        scope: OperationScope,
    ) -> Result<OperationId, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let heads = txn.get_operation_heads(scope).map_err(pristine_error)?;
        match heads.as_slice() {
            [head] => Ok(*head),
            [] => Err(RepositoryError::OperationNotFound {
                selector: format!("current head of {scope}"),
            }),
            many => Err(RepositoryError::OperationHeadsDiverged {
                scope: scope.to_string(),
                heads: many.iter().map(ToString::to_string).collect(),
            }),
        }
    }

    /// Return operations reachable from a scope's heads in deterministic causal order.
    ///
    /// Children always precede parents unless `reverse` is true. Timestamps are used
    /// only to order causally independent ready nodes, with the operation ID as a
    /// deterministic tie-breaker.
    pub fn operation_log(
        &self,
        scope: OperationScope,
        limit: Option<usize>,
        reverse: bool,
    ) -> Result<OperationLog, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let heads = txn.get_operation_heads(scope).map_err(pristine_error)?;
        let head_state = match heads.as_slice() {
            [] => OperationHeadState::Empty,
            [head] => OperationHeadState::Single(*head),
            many => OperationHeadState::Diverged(many.to_vec()),
        };
        let head_ids: BTreeSet<OperationId> = heads.as_slice().iter().copied().collect();
        let operations: BTreeMap<OperationId, Operation> = txn
            .list_operations()
            .map_err(pristine_error)?
            .into_iter()
            .map(|operation| (operation.id(), operation))
            .collect();

        let mut reachable = BTreeSet::new();
        let mut pending = heads.as_slice().to_vec();
        while let Some(operation_id) = pending.pop() {
            if !reachable.insert(operation_id) {
                continue;
            }
            let operation = operations.get(&operation_id).ok_or_else(|| {
                RepositoryError::OperationNotFound {
                    selector: operation_id.to_string(),
                }
            })?;
            pending.extend(operation.payload().parents.iter().copied());
        }

        let mut child_counts: BTreeMap<OperationId, usize> = reachable
            .iter()
            .copied()
            .map(|operation_id| (operation_id, 0))
            .collect();
        for operation_id in &reachable {
            let operation =
                operations
                    .get(operation_id)
                    .ok_or_else(|| RepositoryError::OperationNotFound {
                        selector: operation_id.to_string(),
                    })?;
            for parent in &operation.payload().parents {
                if let Some(count) = child_counts.get_mut(parent) {
                    *count += 1;
                }
            }
        }

        let mut ready: BTreeSet<(i64, OperationId)> = child_counts
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(operation_id, _)| {
                let timestamp = operations
                    .get(operation_id)
                    .map(|operation| operation.payload().timestamp_ms)
                    .unwrap_or_default();
                (timestamp, *operation_id)
            })
            .collect();
        let mut ordered = Vec::with_capacity(reachable.len());
        while let Some(key) = ready.iter().next_back().copied() {
            ready.remove(&key);
            let operation_id = key.1;
            ordered.push(operation_id);
            let operation = operations.get(&operation_id).ok_or_else(|| {
                RepositoryError::OperationNotFound {
                    selector: operation_id.to_string(),
                }
            })?;
            for parent in &operation.payload().parents {
                let Some(count) = child_counts.get_mut(parent) else {
                    continue;
                };
                *count = count.saturating_sub(1);
                if *count == 0 {
                    let parent_operation = operations.get(parent).ok_or_else(|| {
                        RepositoryError::OperationNotFound {
                            selector: parent.to_string(),
                        }
                    })?;
                    ready.insert((parent_operation.payload().timestamp_ms, *parent));
                }
            }
        }
        if ordered.len() != reachable.len() {
            return Err(RepositoryError::InvalidOperation {
                message: format!("operation history for {scope} contains a causal cycle"),
            });
        }
        if reverse {
            ordered.reverse();
        }
        if let Some(limit) = limit {
            ordered.truncate(limit);
        }

        let mut entries = Vec::with_capacity(ordered.len());
        for operation_id in ordered {
            let operation = operations.get(&operation_id).cloned().ok_or_else(|| {
                RepositoryError::OperationNotFound {
                    selector: operation_id.to_string(),
                }
            })?;
            let receipts = txn
                .get_effect_receipts(operation_id)
                .map_err(pristine_error)?;
            entries.push(OperationLogEntry {
                operation,
                verification: operation_verification_state(&receipts),
                is_head: head_ids.contains(&operation_id),
            });
        }
        Ok(OperationLog {
            scope,
            head_state,
            entries,
        })
    }

    /// Consolidate verified commuting heads or return their explicit divergent state.
    ///
    /// Ancestor-dominated heads are removed first. Remaining direct siblings are
    /// consolidated only when their typed metadata/effect write sets and aggregate
    /// repository-state components do not overlap. Incompatible heads are preserved.
    pub fn consolidate_operation_heads(
        &self,
        scope: OperationScope,
    ) -> Result<OperationHeadState, RepositoryError> {
        match scope {
            OperationScope::Repository => {
                let operation_lock = self.try_lock_common_operation()?;
                let mut txn = operation_lock.begin_write_immediate()?;
                let state = self.consolidate_operation_heads_in_txn(&mut txn, scope)?;
                txn.commit()?;
                Ok(state)
            }
            OperationScope::WorkingCopy(working_copy) => {
                let operation_lock = self.try_lock_operation(working_copy)?;
                self.consolidate_operation_heads_locked(&operation_lock)
            }
        }
    }

    pub(super) fn consolidate_operation_heads_locked(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
    ) -> Result<OperationHeadState, RepositoryError> {
        let scope = OperationScope::WorkingCopy(operation_lock.working_copy());
        let mut txn = operation_lock.begin_write_immediate()?;
        let state = self.consolidate_operation_heads_in_txn(&mut txn, scope)?;
        txn.commit()?;
        Ok(state)
    }

    fn consolidate_operation_heads_in_txn(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        scope: OperationScope,
    ) -> Result<OperationHeadState, RepositoryError> {
        let original = txn.get_operation_heads(scope).map_err(pristine_error)?;
        let mut reduced = Vec::new();
        for candidate in original.as_slice() {
            let mut dominated = false;
            for other in original.as_slice() {
                if other != candidate && operation_reaches(txn, *other, *candidate)? {
                    dominated = true;
                    break;
                }
            }
            if !dominated {
                reduced.push(*candidate);
            }
        }
        reduced.sort_unstable();
        reduced.dedup();

        match reduced.as_slice() {
            [] => {
                if !original.is_empty() {
                    txn.compare_and_set_operation_heads(scope, original.as_slice(), &[])
                        .map_err(pristine_error)?;
                }
                return Ok(OperationHeadState::Empty);
            }
            [head] => {
                if original.as_slice() != reduced.as_slice() {
                    txn.compare_and_set_operation_heads(scope, original.as_slice(), &reduced)
                        .map_err(pristine_error)?;
                }
                return Ok(OperationHeadState::Single(*head));
            }
            _ => {}
        }

        let mut operations = Vec::with_capacity(reduced.len());
        for operation_id in &reduced {
            let operation = txn
                .get_operation(*operation_id)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::OperationNotFound {
                    selector: operation_id.to_string(),
                })?;
            let receipts = txn
                .get_effect_receipts(*operation_id)
                .map_err(pristine_error)?;
            if !has_operation_verified_receipt(&receipts) {
                return Ok(OperationHeadState::Diverged(reduced));
            }
            operations.push(operation);
        }

        let Some(merged_state) = merge_commuting_operation_states(&operations, scope) else {
            return Ok(OperationHeadState::Diverged(reduced));
        };
        if !operation_write_sets_are_disjoint(&operations) {
            return Ok(OperationHeadState::Diverged(reduced));
        }

        let timestamp_ms = operations
            .iter()
            .map(|operation| operation.payload().timestamp_ms)
            .max()
            .unwrap_or_default();
        let consolidation = Operation::new(OperationPayload {
            parents: reduced.clone(),
            kind: OperationKind::Consolidate,
            relation: None,
            working_copy: match scope {
                OperationScope::Repository => None,
                OperationScope::WorkingCopy(working_copy) => Some(working_copy),
            },
            before: merged_state.clone(),
            delta: RepoStateDelta {
                after: merged_state,
                metadata: Vec::new(),
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence: Vec::new(),
            actor: ActorRef::System {
                name: CONSOLIDATION_ACTOR.to_string(),
            },
            timestamp_ms,
            lossy: Vec::new(),
        })
        .map_err(codec_error)?;
        txn.put_operation(&consolidation).map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(scope, original.as_slice(), &[consolidation.id()])
            .map_err(pristine_error)?;
        let verified = deterministic_effect_receipt(
            &consolidation,
            None,
            EffectReceiptKind::Verified,
            None,
            None,
        )?;
        txn.append_effect_receipt(&verified)
            .map_err(pristine_error)?;
        Ok(OperationHeadState::Single(consolidation.id()))
    }

    /// Ensure one per-working-copy anchor exists and return the sole current head.
    pub(super) fn ensure_working_copy_anchor(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        state: RepoStateRef,
    ) -> Result<OperationId, RepositoryError> {
        let working_copy = operation_lock.working_copy();
        validate_state_working_copy(&state, working_copy)?;
        let scope = OperationScope::WorkingCopy(working_copy);
        let mut txn = operation_lock.begin_write_immediate()?;
        let heads = txn.get_operation_heads(scope).map_err(pristine_error)?;
        match heads.as_slice() {
            [head] => Ok(*head),
            [] => {
                let anchor = Operation::new(OperationPayload {
                    parents: Vec::new(),
                    kind: OperationKind::Anchor,
                    relation: None,
                    working_copy: Some(working_copy),
                    before: state.clone(),
                    delta: RepoStateDelta {
                        after: state,
                        metadata: Vec::new(),
                        effects: Vec::new(),
                    },
                    git_observed: Vec::new(),
                    evidence: Vec::new(),
                    actor: ActorRef::System {
                        name: ANCHOR_ACTOR.to_string(),
                    },
                    timestamp_ms: 0,
                    lossy: Vec::new(),
                })
                .map_err(codec_error)?;
                txn.put_operation(&anchor).map_err(pristine_error)?;
                txn.compare_and_set_operation_heads(scope, &[], &[anchor.id()])
                    .map_err(pristine_error)?;
                let verified = deterministic_effect_receipt(
                    &anchor,
                    None,
                    EffectReceiptKind::Verified,
                    None,
                    None,
                )?;
                txn.append_effect_receipt(&verified)
                    .map_err(pristine_error)?;
                txn.commit()?;
                Ok(anchor.id())
            }
            many => Err(multiple_heads_error(scope, many)),
        }
    }

    fn ensure_repository_anchor(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        state: &RepoStateRef,
    ) -> Result<OperationId, RepositoryError> {
        let _scope = OperationScope::Repository;
        let mut txn = operation_lock.begin_write_immediate()?;
        let head = self.ensure_repository_anchor_in_txn(&mut txn, state)?;
        txn.commit()?;
        Ok(head)
    }

    /// Resolve the sole current repository-scope head inside a caller-owned
    /// transaction, consolidating verified commuting heads and creating the
    /// verified anchor when the scope is empty.
    ///
    /// The caller owns the transaction lifecycle: nothing here commits, so a
    /// remediation journal can chain from the returned head in the same
    /// durable transaction as its own mutation.
    pub(super) fn ensure_repository_anchor_in_txn(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        state: &RepoStateRef,
    ) -> Result<OperationId, RepositoryError> {
        let scope = OperationScope::Repository;
        let repository_state = RepoStateRef {
            view: state.view.clone(),
            working_copy: None,
            git: state.git.clone(),
        };
        let head_state = self.consolidate_operation_heads_in_txn(txn, scope)?;
        match head_state {
            OperationHeadState::Single(head) => {
                let receipts = txn.get_effect_receipts(head).map_err(pristine_error)?;
                if !has_operation_verified_receipt(&receipts) {
                    return Err(RepositoryError::OperationNotVerified {
                        operation: head.to_string(),
                    });
                }
                Ok(head)
            }
            OperationHeadState::Empty => {
                let anchor = Operation::new(OperationPayload {
                    parents: Vec::new(),
                    kind: OperationKind::Anchor,
                    relation: None,
                    working_copy: None,
                    before: repository_state.clone(),
                    delta: RepoStateDelta {
                        after: repository_state,
                        metadata: Vec::new(),
                        effects: Vec::new(),
                    },
                    git_observed: Vec::new(),
                    evidence: Vec::new(),
                    actor: ActorRef::System {
                        name: ANCHOR_ACTOR.to_string(),
                    },
                    timestamp_ms: 0,
                    lossy: Vec::new(),
                })
                .map_err(codec_error)?;
                txn.put_operation(&anchor).map_err(pristine_error)?;
                txn.compare_and_set_operation_heads(scope, &[], &[anchor.id()])
                    .map_err(pristine_error)?;
                let verified = deterministic_effect_receipt(
                    &anchor,
                    None,
                    EffectReceiptKind::Verified,
                    None,
                    None,
                )?;
                txn.append_effect_receipt(&verified)
                    .map_err(pristine_error)?;
                Ok(anchor.id())
            }
            OperationHeadState::Diverged(heads) => Err(RepositoryError::OperationHeadsDiverged {
                scope: scope.to_string(),
                heads: heads.iter().map(ToString::to_string).collect(),
            }),
        }
    }

    /// Prepare and immediately persist a switch operation before external effects.
    ///
    /// Filesystem leases are preflighted, the operation/head CAS is fsync-durable,
    /// and only then are immutable old-value backups written. A returned value proves
    /// that the complete backup marker is durable and callers may start effects.
    #[cfg(test)]
    pub(super) fn prepare_switch_operation(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        before: RepoStateRef,
        after: RepoStateRef,
        effects: Vec<EffectPlan>,
        actor: ActorRef,
        timestamp_ms: i64,
    ) -> Result<PreparedSwitchOperation, RepositoryError> {
        self.prepare_working_copy_transition(
            operation_lock,
            OperationKind::SwitchView,
            None,
            before,
            after,
            effects,
            Vec::new(),
            actor,
            timestamp_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_working_copy_transition(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        kind: OperationKind,
        relation: Option<OperationRelation>,
        before: RepoStateRef,
        after: RepoStateRef,
        effects: Vec<EffectPlan>,
        evidence: Vec<Hash>,
        actor: ActorRef,
        timestamp_ms: i64,
    ) -> Result<PreparedSwitchOperation, RepositoryError> {
        let working_copy = operation_lock.working_copy();
        validate_state_working_copy(&before, working_copy)?;
        validate_state_working_copy(&after, working_copy)?;
        let parent = self.ensure_working_copy_anchor(operation_lock, before.clone())?;
        self.require_complete_single_head(working_copy, parent)?;

        let operation = Operation::new(OperationPayload {
            parents: vec![parent],
            kind,
            relation,
            working_copy: Some(working_copy),
            before,
            delta: RepoStateDelta {
                after,
                metadata: Vec::new(),
                effects,
            },
            git_observed: Vec::new(),
            evidence,
            actor,
            timestamp_ms,
            lossy: Vec::new(),
        })
        .map_err(codec_error)?;
        validate_effect_target_chains(&operation)?;

        self.preflight_filesystem_leases(working_copy, &operation)?;
        let scope = OperationScope::WorkingCopy(working_copy);
        let mut txn = operation_lock.begin_write_immediate()?;
        txn.put_operation(&operation).map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(scope, &[parent], &[operation.id()])
            .map_err(pristine_error)?;
        txn.commit()?;

        let backup_root = self.snapshot_operation_filesystem(operation_lock, &operation)?;
        Ok(PreparedSwitchOperation {
            operation,
            backup_root,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_working_copy_transition_with_metadata(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        kind: OperationKind,
        relation: Option<OperationRelation>,
        before: RepoStateRef,
        after: RepoStateRef,
        effects: Vec<EffectPlan>,
        metadata: Vec<MetadataTransition>,
        evidence: Vec<Hash>,
        actor: ActorRef,
        timestamp_ms: i64,
    ) -> Result<PreparedSwitchOperation, RepositoryError> {
        let working_copy = operation_lock.working_copy();
        validate_state_working_copy(&before, working_copy)?;
        validate_state_working_copy(&after, working_copy)?;
        for transition in &metadata {
            validate_capability_lease(&transition.target, &transition.expected_new)?;
            let observed = self.observe_metadata_value(&transition.target)?;
            if observed != transition.expected_old {
                return Err(metadata_divergence_error(transition, &observed));
            }
        }
        let parent = self.ensure_working_copy_anchor(operation_lock, before.clone())?;
        self.require_complete_single_head(working_copy, parent)?;
        let repository_parent = self.ensure_repository_anchor(operation_lock, &before)?;
        let operation = Operation::new(OperationPayload {
            parents: vec![parent, repository_parent],
            kind,
            relation,
            working_copy: Some(working_copy),
            before,
            delta: RepoStateDelta {
                after,
                metadata,
                effects,
            },
            git_observed: Vec::new(),
            evidence,
            actor,
            timestamp_ms,
            lossy: Vec::new(),
        })
        .map_err(codec_error)?;
        validate_effect_target_chains(&operation)?;

        self.preflight_filesystem_leases(working_copy, &operation)?;
        let scope = OperationScope::WorkingCopy(working_copy);
        let mut txn = operation_lock.begin_write_immediate()?;
        txn.put_operation(&operation).map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(scope, &[parent], &[operation.id()])
            .map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(
            OperationScope::Repository,
            &[repository_parent],
            &[operation.id()],
        )
        .map_err(pristine_error)?;
        txn.commit()?;

        let backup_root = self.snapshot_operation_filesystem(operation_lock, &operation)?;
        Ok(PreparedSwitchOperation {
            operation,
            backup_root,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_metadata_operation(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        kind: OperationKind,
        relation: Option<OperationRelation>,
        before: RepoStateRef,
        after: RepoStateRef,
        metadata: Vec<MetadataTransition>,
        evidence: Vec<Hash>,
        actor: ActorRef,
        timestamp_ms: i64,
    ) -> Result<Operation, RepositoryError> {
        let working_copy = operation_lock.working_copy();
        validate_state_working_copy(&before, working_copy)?;
        validate_state_working_copy(&after, working_copy)?;
        for transition in &metadata {
            validate_capability_lease(&transition.target, &transition.expected_new)?;
            let observed = self.observe_metadata_value(&transition.target)?;
            if observed != transition.expected_old {
                return Err(metadata_divergence_error(transition, &observed));
            }
        }
        let working_copy_parent =
            self.ensure_working_copy_anchor(operation_lock, before.clone())?;
        self.require_complete_single_head(working_copy, working_copy_parent)?;
        let repository_parent = self.ensure_repository_anchor(operation_lock, &before)?;
        let operation = Operation::new(OperationPayload {
            parents: vec![working_copy_parent, repository_parent],
            kind,
            relation,
            working_copy: Some(working_copy),
            before,
            delta: RepoStateDelta {
                after,
                metadata,
                effects: Vec::new(),
            },
            git_observed: Vec::new(),
            evidence,
            actor,
            timestamp_ms,
            lossy: Vec::new(),
        })
        .map_err(codec_error)?;
        let scope = OperationScope::WorkingCopy(working_copy);
        let mut txn = operation_lock.begin_write_immediate()?;
        txn.put_operation(&operation).map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(scope, &[working_copy_parent], &[operation.id()])
            .map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(
            OperationScope::Repository,
            &[repository_parent],
            &[operation.id()],
        )
        .map_err(pristine_error)?;
        txn.commit()?;
        Ok(operation)
    }

    pub(super) fn apply_operation_metadata_locked(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation_id: OperationId,
    ) -> Result<(), RepositoryError> {
        let operation = self.load_operation(operation_id)?;
        self.validate_operation_lock(operation_lock, &operation)?;
        // An operation with no metadata transitions still publishes its
        // working-copy record transition (delta.after), so the record write
        // below is not gated on metadata being non-empty.
        let mut txn = operation_lock.begin_write_immediate()?;
        let mut pending = Vec::new();
        for transition in &operation.payload().delta.metadata {
            let observed = observe_metadata_value_in_write(&txn, &transition.target)?;
            match classify_metadata_lease(
                &observed,
                &transition.expected_old,
                &transition.expected_new,
            ) {
                LeaseClassification::Apply => pending.push((transition, observed)),
                LeaseClassification::AlreadyApplied => {}
                LeaseClassification::Diverged => {
                    return Err(metadata_divergence_error(transition, &observed));
                }
            }
        }
        pending.sort_by(|(left, left_observed), (right, right_observed)| {
            metadata_application_key(left, left_observed)
                .cmp(&metadata_application_key(right, right_observed))
                .then_with(|| left.target.cmp(&right.target))
        });
        let mut affected_views = BTreeSet::new();
        for (transition, _) in pending {
            apply_metadata_value_in_write(&mut txn, &transition.target, &transition.expected_new)?;
            if let Some(view) = metadata_target_view(&transition.target) {
                affected_views.insert(view.to_string());
            }
        }
        for view in affected_views {
            self.realign_tree_projection_in_txn(&mut txn, &view)?;
        }
        if let Some(state) = &operation.payload().delta.after.working_copy {
            if state.id != operation_lock.working_copy() {
                return Err(RepositoryError::WorkingCopyIdentityMismatch {
                    requested: state.id,
                    actual: operation_lock.working_copy(),
                });
            }
            txn.put_working_copy(&WorkingCopyRecord {
                id: state.id,
                location_fingerprint: state.location_fingerprint,
                desired_view: state.desired_view,
                desired_state: state.desired_state,
                materialized_state: state.materialized_state,
                materialized_manifest: state.materialized_manifest,
            })
            .map_err(pristine_error)?;
        }
        txn.commit()
    }

    pub(super) fn abort_prepared_metadata_operation(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        original: &Operation,
    ) -> Result<OperationId, RepositoryError> {
        if !original.payload().delta.effects.is_empty() {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "operation {} has external effects and cannot use metadata-only abort",
                    original.id()
                ),
            });
        }
        let working_copy = operation_lock.working_copy();
        let recovery = self.inverse_recovery_operation(original, working_copy)?;
        let scope = OperationScope::WorkingCopy(working_copy);
        let mut txn = operation_lock.begin_write_immediate()?;
        txn.put_operation(&recovery).map_err(pristine_error)?;
        txn.compare_and_set_operation_heads(scope, &[original.id()], &[recovery.id()])
            .map_err(pristine_error)?;
        let repository_heads = txn
            .get_operation_heads(OperationScope::Repository)
            .map_err(pristine_error)?;
        if repository_heads.as_slice() == [original.id()] {
            txn.compare_and_set_operation_heads(
                OperationScope::Repository,
                &[original.id()],
                &[recovery.id()],
            )
            .map_err(pristine_error)?;
        }
        txn.commit()?;
        self.apply_operation_metadata_locked(operation_lock, recovery.id())?;
        self.finalize_operation_verified(operation_lock, recovery.id())?;
        Ok(recovery.id())
    }

    /// Append a deterministic receipt after a caller performs or observes an effect.
    ///
    /// `observed_before` is classified against the immutable plan. The receipt is
    /// appended only when `observed_after` equals expected-new; third values append a
    /// stable rejection receipt and return a typed invalid-operation error.
    /// Execute one planned working-tree path transition under its exact lease.
    ///
    /// The returned outcome is deliberately separate from receipt persistence so
    /// crash tests can exercise the effect-to-receipt window. Normal callers must
    /// immediately pass it to [`Self::record_pending_filesystem_effect`].
    pub(super) fn execute_filesystem_effect(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation_id: OperationId,
        effect_ordinal: u32,
        content: Option<&[u8]>,
    ) -> Result<PendingFilesystemEffect, RepositoryError> {
        let operation = self.load_operation(operation_id)?;
        self.validate_operation_lock(operation_lock, &operation)?;
        let effect = effect_at(&operation, effect_ordinal)?;
        if !matches!(effect.target, EffectTarget::FilesystemPath { .. }) {
            return Err(unsupported_effect_error(&effect.target));
        }
        let working_copy = operation_lock.working_copy();
        let target = self.resolve_recovery_target(working_copy, &effect.target)?;
        let observed_before = self.observe_recovery_target(&target)?;
        match classify_effect_lease(&observed_before, &effect.expected_old, &effect.expected_new) {
            LeaseClassification::AlreadyApplied => {
                return Ok(PendingFilesystemEffect {
                    ordinal: effect.ordinal,
                    observed_before: observed_before.clone(),
                    observed_after: observed_before,
                    mutated: false,
                });
            }
            LeaseClassification::Diverged => {
                return self
                    .record_effect_outcome(
                        operation_lock,
                        operation_id,
                        effect.ordinal,
                        observed_before.clone(),
                        observed_before,
                    )
                    .and_then(|_| {
                        Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "effect {} unexpectedly accepted a divergent filesystem lease",
                                effect.ordinal
                            ),
                        })
                    });
            }
            LeaseClassification::Apply => {}
        }

        let ResolvedRecoveryTarget::Filesystem { path, .. } = &target else {
            return Err(unsupported_effect_error(&effect.target));
        };
        match &effect.expected_new {
            EffectValue::Absent => remove_filesystem_effect_path(path)?,
            EffectValue::File(state) if state.kind == FileKind::Regular => {
                let bytes = content.ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "regular-file effect {} requires prepared content bytes",
                        effect.ordinal
                    ),
                })?;
                if Hash::of(bytes) != state.content {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "prepared content for effect {} does not match expected-new hash",
                            effect.ordinal
                        ),
                    });
                }
                write_atomic_regular(path, bytes, state.mode)?;
            }
            EffectValue::File(state) if state.kind == FileKind::Symlink => {
                let bytes = content.ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "symlink effect {} requires prepared target bytes",
                        effect.ordinal
                    ),
                })?;
                if Hash::of(bytes) != state.content {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "prepared symlink target for effect {} does not match expected-new hash",
                            effect.ordinal
                        ),
                    });
                }
                write_atomic_symlink(path, bytes)?;
            }
            EffectValue::File(state) if state.kind == FileKind::Gitlink => {
                let bytes = content.ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "gitlink effect {} requires prepared object-id bytes",
                        effect.ordinal
                    ),
                })?;
                if Hash::of(bytes) != state.content {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "prepared gitlink for effect {} does not match expected-new hash",
                            effect.ordinal
                        ),
                    });
                }
                write_atomic_gitlink(path, bytes, state.mode)?;
            }
            EffectValue::File(state) if state.kind == FileKind::Directory => {
                let staging = self
                    .working_copy_effect_recovery_root(working_copy, operation.id())
                    .join("staged-directories")
                    .join(format!("{:010}", effect.ordinal));
                create_directory_effect_path(path, state.mode, &staging)?;
            }
            value => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "filesystem effect {} cannot materialize expected-new value {value:?}",
                        effect.ordinal
                    ),
                })
            }
        }
        let observed_after = self.observe_recovery_target(&target)?;
        if observed_after != effect.expected_new {
            let _ = self.record_effect_outcome(
                operation_lock,
                operation_id,
                effect.ordinal,
                observed_before.clone(),
                observed_after.clone(),
            );
            return Err(lease_divergence_error(
                effect,
                &observed_before,
                Some(&observed_after),
            ));
        }
        Ok(PendingFilesystemEffect {
            ordinal: effect.ordinal,
            observed_before,
            observed_after,
            mutated: true,
        })
    }

    pub(super) fn record_pending_filesystem_effect(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation_id: OperationId,
        pending: PendingFilesystemEffect,
    ) -> Result<bool, RepositoryError> {
        self.record_effect_outcome(
            operation_lock,
            operation_id,
            pending.ordinal,
            pending.observed_before,
            pending.observed_after,
        )?;
        Ok(pending.mutated)
    }

    pub(super) fn append_rejected_effect_outcome(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        operation: &Operation,
        effect_ordinal: u32,
        observed_before: EffectValue,
        observed_after: Option<EffectValue>,
    ) -> Result<(), RepositoryError> {
        effect_at(operation, effect_ordinal)?;
        let receipt = deterministic_effect_receipt(
            operation,
            Some(effect_ordinal),
            EffectReceiptKind::LeaseRejected,
            Some(observed_before),
            observed_after,
        )?;
        txn.append_effect_receipt(&receipt).map_err(pristine_error)
    }

    pub(super) fn append_successful_effect_outcome(
        &self,
        txn: &mut atomic_core::pristine::WriteTxn<'_>,
        operation: &Operation,
        effect_ordinal: u32,
        observed_before: EffectValue,
        observed_after: EffectValue,
    ) -> Result<(), RepositoryError> {
        let effect = effect_at(operation, effect_ordinal)?;
        let classification =
            classify_effect_lease(&observed_before, &effect.expected_old, &effect.expected_new);
        if classification == LeaseClassification::Diverged || observed_after != effect.expected_new
        {
            return Err(lease_divergence_error(
                effect,
                &observed_before,
                Some(&observed_after),
            ));
        }
        let kind = match classification {
            LeaseClassification::Apply => EffectReceiptKind::Applied,
            LeaseClassification::AlreadyApplied => EffectReceiptKind::Recovered,
            LeaseClassification::Diverged => unreachable!("handled above"),
        };
        let receipt = deterministic_effect_receipt(
            operation,
            Some(effect_ordinal),
            kind,
            Some(observed_before),
            Some(observed_after),
        )?;
        txn.append_effect_receipt(&receipt).map_err(pristine_error)
    }

    pub(super) fn record_effect_outcome(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation_id: OperationId,
        effect_ordinal: u32,
        observed_before: EffectValue,
        observed_after: EffectValue,
    ) -> Result<EffectReceipt, RepositoryError> {
        let operation = self.load_operation(operation_id)?;
        self.validate_operation_lock(operation_lock, &operation)?;
        let effect = effect_at(&operation, effect_ordinal)?;
        let classification =
            classify_effect_lease(&observed_before, &effect.expected_old, &effect.expected_new);
        if classification == LeaseClassification::Diverged || observed_after != effect.expected_new
        {
            let receipt = deterministic_effect_receipt(
                &operation,
                Some(effect_ordinal),
                EffectReceiptKind::LeaseRejected,
                Some(observed_before.clone()),
                Some(observed_after.clone()),
            )?;
            self.append_receipt_immediate(operation_lock, &operation, &receipt)?;
            return Err(lease_divergence_error(
                effect,
                &observed_before,
                Some(&observed_after),
            ));
        }

        let kind = match classification {
            LeaseClassification::Apply => EffectReceiptKind::Applied,
            LeaseClassification::AlreadyApplied => EffectReceiptKind::Recovered,
            LeaseClassification::Diverged => unreachable!("handled above"),
        };
        let receipt = deterministic_effect_receipt(
            &operation,
            Some(effect_ordinal),
            kind,
            Some(observed_before),
            Some(observed_after),
        )?;
        self.append_receipt_immediate(operation_lock, &operation, &receipt)?;
        Ok(receipt)
    }

    /// Append the deterministic operation-level `Verified` receipt immediately.
    pub(super) fn finalize_operation_verified(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation_id: OperationId,
    ) -> Result<EffectReceipt, RepositoryError> {
        let operation = self.load_operation(operation_id)?;
        self.validate_operation_lock(operation_lock, &operation)?;
        let receipts = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.get_effect_receipts(operation_id)
                .map_err(pristine_error)?
        };
        let completed: BTreeSet<u32> = receipts
            .iter()
            .filter_map(|receipt| match receipt.payload().kind {
                EffectReceiptKind::Applied
                | EffectReceiptKind::Recovered
                | EffectReceiptKind::RolledBack => receipt.payload().effect_ordinal,
                EffectReceiptKind::Verified | EffectReceiptKind::LeaseRejected => None,
            })
            .collect();
        for effect in &operation.payload().delta.effects {
            if !completed.contains(&effect.ordinal) {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation {} cannot be verified before effect {} has a successful receipt",
                        operation.id(),
                        effect.ordinal
                    ),
                });
            }
        }
        for transition in &operation.payload().delta.metadata {
            let observed = self.observe_metadata_value(&transition.target)?;
            if observed != transition.expected_new {
                return Err(metadata_divergence_error(transition, &observed));
            }
        }
        if let Some(expected) = &operation.payload().delta.after.view {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            let observed = txn
                .get_view(&expected.name)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: expected.name.clone(),
                })?;
            if observed.state != expected.state {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation {} cannot be verified: view '{}' state expected {}, observed {}",
                        operation.id(),
                        expected.name,
                        expected.state,
                        observed.state
                    ),
                });
            }
        }
        let working_copy = operation_lock.working_copy();
        if let Some(expected) = &operation.payload().delta.after.working_copy {
            let observed = working_copy_state_ref(self.working_copy_record(working_copy)?);
            if &observed != expected {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation {} cannot be verified: working-copy state expected {:?}, observed {:?}",
                        operation.id(), expected, observed
                    ),
                });
            }
        }
        let mut final_targets = Vec::<(&EffectTarget, &EffectValue)>::new();
        for effect in &operation.payload().delta.effects {
            if let Some((_, value)) = final_targets
                .iter_mut()
                .find(|(target, _)| **target == effect.target)
            {
                *value = &effect.expected_new;
            } else {
                final_targets.push((&effect.target, &effect.expected_new));
            }
        }
        for (target, expected) in final_targets {
            let resolved = self.resolve_recovery_target(working_copy, target)?;
            let observed = self.observe_recovery_target(&resolved)?;
            if &observed != expected {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation {} cannot be verified: target {target:?} expected {expected:?}, observed {observed:?}",
                        operation.id()
                    ),
                });
            }
        }
        let receipt = deterministic_effect_receipt(
            &operation,
            None,
            EffectReceiptKind::Verified,
            None,
            None,
        )?;
        self.append_receipt_immediate(operation_lock, &operation, &receipt)?;
        Ok(receipt)
    }

    /// Test whether one operation has an immutable operation-level verified receipt.
    pub(super) fn operation_is_verified(
        &self,
        operation_id: OperationId,
    ) -> Result<bool, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let receipts = txn
            .get_effect_receipts(operation_id)
            .map_err(pristine_error)?;
        Ok(has_operation_verified_receipt(&receipts))
    }

    pub(super) fn repository_operation_requires_recovery(&self) -> Result<bool, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let heads = txn
            .get_operation_heads(OperationScope::Repository)
            .map_err(pristine_error)?;
        if heads.as_slice().len() > 1 {
            return Ok(true);
        }
        let Some(head) = heads.as_slice().first().copied() else {
            return Ok(false);
        };
        let receipts = txn.get_effect_receipts(head).map_err(pristine_error)?;
        Ok(!has_operation_verified_receipt(&receipts))
    }

    pub(super) fn ensure_repository_operation_safe_for(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
    ) -> Result<(), RepositoryError> {
        let working_copy = operation_lock.working_copy();
        let mut txn = operation_lock.begin_write_immediate()?;
        let head =
            match self.consolidate_operation_heads_in_txn(&mut txn, OperationScope::Repository)? {
                OperationHeadState::Empty => {
                    txn.commit()?;
                    return Ok(());
                }
                OperationHeadState::Single(head) => head,
                OperationHeadState::Diverged(heads) => {
                    return Err(RepositoryError::OperationHeadsDiverged {
                        scope: OperationScope::Repository.to_string(),
                        heads: heads.iter().map(ToString::to_string).collect(),
                    })
                }
            };
        let receipts = txn.get_effect_receipts(head).map_err(pristine_error)?;
        if has_operation_verified_receipt(&receipts) {
            txn.commit()?;
            return Ok(());
        }
        let operation = txn
            .get_operation(head)
            .map_err(pristine_error)?
            .ok_or_else(|| RepositoryError::OperationNotFound {
                selector: head.to_string(),
            })?;
        if operation.payload().working_copy == Some(working_copy) {
            txn.commit()?;
            Ok(())
        } else {
            Err(RepositoryError::OperationNotVerified {
                operation: head.to_string(),
            })
        }
    }

    /// Return whether a working-copy head requires writable recovery.
    pub(super) fn working_copy_operation_requires_recovery(
        &self,
        working_copy: WorkingCopyId,
    ) -> Result<bool, RepositoryError> {
        let scope = OperationScope::WorkingCopy(working_copy);
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let heads = txn.get_operation_heads(scope).map_err(pristine_error)?;
        if heads.as_slice().len() > 1 {
            return Ok(true);
        }
        let Some(head) = heads.as_slice().first().copied() else {
            return Ok(false);
        };
        let receipts = txn.get_effect_receipts(head).map_err(pristine_error)?;
        Ok(!has_operation_verified_receipt(&receipts))
    }

    /// Recover the sole incomplete operation head for one working copy.
    ///
    /// Incomplete non-Recover heads first receive an immutable inverse `Recover`
    /// child through an immediate head CAS. An incomplete Recover head is resumed in
    /// place. Filesystem effects are then replayed idempotently from the original
    /// operation's backups. Unsupported effects and third lease values fail closed.
    pub(super) fn recover_incomplete_operation(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
    ) -> Result<RecoveryOutcome, RepositoryError> {
        let working_copy = operation_lock.working_copy();
        let scope = OperationScope::WorkingCopy(working_copy);
        let heads = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.get_operation_heads(scope).map_err(pristine_error)?
        };
        let head = match heads.as_slice() {
            [] => return Ok(RecoveryOutcome::NoOperation),
            [head] => *head,
            many => return Err(multiple_heads_error(scope, many)),
        };
        let head_operation = self.load_operation(head)?;
        // CB-13C observability (review R5): terminal recovery failures are
        // recorded, not only successful recoveries. The journal is the
        // consent-gated automatic sink (`for_repository`).
        if let Err(error) = self.validate_operation_lock(operation_lock, &head_operation) {
            self.emit_recovery_failure(
                &head_operation,
                super::observability::RecoveryFailureCode::LockValidation,
            );
            return Err(error);
        }
        if self.operation_is_verified(head)? {
            return Ok(RecoveryOutcome::AlreadyComplete { operation: head });
        }

        let (original, recovery, created) = if head_operation.payload().kind
            == OperationKind::Recover
        {
            let original_id = sole_recovery_parent(&head_operation)?;
            (self.load_operation(original_id)?, head_operation, false)
        } else {
            let recovery = match self.inverse_recovery_operation(&head_operation, working_copy) {
                Ok(recovery) => recovery,
                Err(error) => {
                    // Review R5: the pre-recovery lease divergence
                    // ("diverged before recovery") previously had
                    // no terminal event.
                    self.emit_recovery_failure(
                        &head_operation,
                        super::observability::RecoveryFailureCode::InverseConstruction,
                    );
                    return Err(error);
                }
            };
            let mut txn = operation_lock.begin_write_immediate()?;
            txn.put_operation(&recovery).map_err(pristine_error)?;
            txn.compare_and_set_operation_heads(scope, &[head], &[recovery.id()])
                .map_err(pristine_error)?;
            let repository_heads = txn
                .get_operation_heads(OperationScope::Repository)
                .map_err(pristine_error)?;
            if repository_heads.as_slice() == [head] {
                txn.compare_and_set_operation_heads(
                    OperationScope::Repository,
                    &[head],
                    &[recovery.id()],
                )
                .map_err(pristine_error)?;
            }
            txn.commit()?;
            (head_operation, recovery, true)
        };

        if let Err(error) = self.apply_operation_metadata_locked(operation_lock, recovery.id()) {
            self.emit_recovery_failure(
                &original,
                super::observability::RecoveryFailureCode::RecoveryApply,
            );
            return Err(error);
        }
        if let Err(error) = self.replay_filesystem_recovery(operation_lock, &original, &recovery) {
            self.emit_recovery_failure(
                &original,
                super::observability::RecoveryFailureCode::FilesystemReplay,
            );
            return Err(error);
        }
        if let Err(error) = self.finalize_operation_verified(operation_lock, recovery.id()) {
            self.emit_recovery_failure(
                &original,
                super::observability::RecoveryFailureCode::Finalize,
            );
            return Err(error);
        }
        // CB-13C observability: recovery outcomes are recorded with their
        // operation IDs (validated fixed-format base32) — lossy, advisory
        // only, consent-gated.
        self.emit_recovery_outcome(&original, &recovery, created);
        Ok(RecoveryOutcome::Recovered {
            original: original.id(),
            recovery: recovery.id(),
            created,
        })
    }

    /// Record one executed recovery (consent-gated, lossy, advisory only).
    /// Skips silently when an operation ID somehow fails fixed-format
    /// validation: no free-form value can bypass the typed event surface.
    fn emit_recovery_outcome(&self, original: &Operation, recovery: &Operation, created: bool) {
        if let (Some(original), Some(recovery)) = (
            super::observability::OpIdRef::new(&original.id().to_string()),
            super::observability::OpIdRef::new(&recovery.id().to_string()),
        ) {
            super::observability::BridgeEventJournal::for_repository(self).emit_lossy(
                super::observability::BridgeEventKind::Recovery {
                    original,
                    recovery,
                    created,
                },
            );
        }
    }

    /// Record one terminal recovery failure (review R5: lease rejections
    /// and replay/finalize failures previously had no event). Consent-gated
    /// and lossy.
    fn emit_recovery_failure(
        &self,
        original: &Operation,
        reason: super::observability::RecoveryFailureCode,
    ) {
        if let Some(original) = super::observability::OpIdRef::new(&original.id().to_string()) {
            super::observability::BridgeEventJournal::for_repository(self).emit_lossy(
                super::observability::BridgeEventKind::RecoveryFailure { original, reason },
            );
        }
    }

    /// Raw bytes of the derived bridge checkpoint file, empty when absent
    /// (R3: the Checkpoint lease observes the file's content digest).
    /// Facts digest of the derived bridge checkpoint (R3): the parsed
    /// checkpoint's canonical serialization, so the lease is format-agnostic
    /// (the CLI's richer v2 writer and the simple writer carry the same
    /// facts) and a fact-preserving rewrite is idempotent.
    pub fn read_projection_checkpoint_facts_digest(&self) -> Result<Hash, RepositoryError> {
        self.read_workspace_checkpoint_facts_digest()
    }

    pub(super) fn read_workspace_checkpoint_facts_digest(
        &self,
    ) -> Result<atomic_core::Hash, RepositoryError> {
        let bytes = std::fs::read(self.dot_dir.join("bridge/workspace.json")).map_err(|error| {
            RepositoryError::InvalidRepository {
                reason: format!("cannot read the bridge checkpoint for a lease: {error}"),
            }
        })?;
        let checkpoint = read_workspace_checkpoint(&self.root)?.ok_or_else(|| {
            RepositoryError::InvalidRepository {
                reason: "the bridge checkpoint disappeared during adoption completion".to_string(),
            }
        })?;
        let _ = bytes;
        Ok(atomic_core::Hash::of(
            super::adoption::checkpoint_facts_bytes(&checkpoint)?.as_slice(),
        ))
    }

    fn observe_metadata_value(
        &self,
        target: &MetadataTarget,
    ) -> Result<MetadataValue, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        match target {
            MetadataTarget::ViewChange { view, change } => {
                let view = txn
                    .get_view(view)
                    .map_err(pristine_error)?
                    .ok_or_else(|| RepositoryError::ViewNotFound { name: view.clone() })?;
                let Some(change_id) = txn.get_internal(change).map_err(pristine_error)? else {
                    return Ok(MetadataValue::Absent);
                };
                Ok(txn
                    .get_change_seq(&view, change_id)
                    .map_err(pristine_error)?
                    .map(MetadataValue::Sequence)
                    .unwrap_or(MetadataValue::Absent))
            }
            MetadataTarget::View { name } => {
                let view = txn
                    .get_view(name)
                    .map_err(pristine_error)?
                    .ok_or_else(|| RepositoryError::ViewNotFound { name: name.clone() })?;
                encode_view_lease(&view)
            }
            MetadataTarget::Tag { view, name } => txn
                .get_tag(view, name)
                .map_err(pristine_error)?
                .map(|tag| {
                    postcard::to_allocvec(&tag)
                        .map(MetadataValue::Bytes)
                        .map_err(|error| RepositoryError::Serialization(error.to_string()))
                })
                .transpose()
                .map(|value| value.unwrap_or(MetadataValue::Absent)),
            MetadataTarget::RefMapping { view } => observe_ref_mapping_value(&txn, view),
            MetadataTarget::Capability { id } => observe_capability_value(&txn, id),
            target => Err(unsupported_metadata_error(target)),
        }
    }

    /// Re-derive the pre-completion bridge checkpoint from the original
    /// completion operation's recorded before-state (R3): the view name and
    /// state come from `before.view`, and the Git facts (symref, HEAD OID,
    /// index digest/tree) come from `before.git`, so the rollback is exact
    /// without retaining checkpoint file bytes anywhere.
    fn restore_checkpoint_from_state(
        &self,
        repo: &git2::Repository,
        original: &Operation,
    ) -> Result<WorkspaceCheckpoint, RepositoryError> {
        let before = &original.payload().before;
        let Some(view) = &before.view else {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "operation {} has no before-view to restore the checkpoint from",
                    original.id()
                ),
            });
        };
        let Some(git) = &before.git else {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "operation {} has no before-Git state to restore the checkpoint from",
                    original.id()
                ),
            });
        };
        let (symref, head_hex) = match &git.head {
            GitHeadState::Attached { symref, oid } => (Some(symref.clone()), git_object_hex(oid)?),
            GitHeadState::Detached { oid } => (None, git_object_hex(oid)?),
            other => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation {} before-Git head {other:?} cannot restore a checkpoint",
                        original.id()
                    ),
                })
            }
        };
        let oid =
            git2::Oid::from_str(&head_hex).map_err(|error| RepositoryError::InvalidRepository {
                reason: format!("cannot parse restored HEAD '{head_hex}': {error}"),
            })?;
        let commit = repo.find_commit(oid).map_err(|error| {
            RepositoryError::InvalidRepository {
                reason: format!(
                    "cannot re-read the pre-completion commit {head_hex} for the checkpoint: {error}"
                ),
            }
        })?;
        Ok(WorkspaceCheckpoint {
            version: 2,
            view: view.name.clone(),
            atomic_state: view.state.to_base32(),
            git_head_symref: symref,
            git_head: head_hex,
            git_tree: commit.tree_id().to_string(),
            git_index_tree: git
                .index
                .as_ref()
                .and_then(|index| index.tree.as_ref())
                .map(git_object_hex)
                .transpose()?,
            git_index_digest: git.index.as_ref().map(|index| index.digest.to_base32()),
        })
    }

    /// Restore one journaled projection index effect's expected-old index
    /// content (CB-8B ac-3): the exact bytes retained before the swap, or the
    /// index-file removal for an effect whose expected-old was an absent
    /// index. The caller re-observes and rejects any third value.
    fn restore_retained_index(
        &self,
        original: &Operation,
        backup_root: &Path,
        git_dir: &Path,
    ) -> Result<(), RepositoryError> {
        // Identify the original effect that inverted into this recovery
        // target: the sole GitIndex effect of the original operation.
        let original_effect = original
            .payload()
            .delta
            .effects
            .iter()
            .find(|effect| matches!(effect.target, EffectTarget::GitIndex { .. }))
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!(
                    "operation {} has no Git index effect to restore",
                    original.id()
                ),
            })?;
        let entry = backup_entry_path(backup_root, original_effect.ordinal);
        let index_path = git_dir.join("index");
        if entry.join(RECOVERY_ABSENT_MARKER).is_file() {
            return match std::fs::remove_file(&index_path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(RepositoryError::InvalidRepository {
                    reason: format!(
                        "cannot re-remove the Git index '{}': {error}",
                        index_path.display()
                    ),
                }),
            };
        }
        let bytes = std::fs::read(entry.join(BACKUP_VALUE)).map_err(|error| {
            RepositoryError::InvalidOperation {
                message: format!(
                    "the retained index bytes for operation {} effect {} are missing: {error}",
                    original.id(),
                    original_effect.ordinal
                ),
            }
        })?;
        let staging = git_dir.join(format!("atomic-index-restore.{}.tmp", std::process::id()));
        let result = (|| -> Result<(), RepositoryError> {
            write_new_synced(&staging, &bytes)?;
            std::fs::rename(&staging, &index_path).map_err(|error| {
                RepositoryError::InvalidRepository {
                    reason: format!(
                        "cannot restore the Git index '{}': {error}",
                        index_path.display()
                    ),
                }
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        result
    }

    /// Observe the exact typed value of an effect supported by this repository engine.
    pub(super) fn observe_operation_effect(
        &self,
        working_copy: WorkingCopyId,
        target: &EffectTarget,
    ) -> Result<EffectValue, RepositoryError> {
        let target = self.resolve_recovery_target(working_copy, target)?;
        self.observe_recovery_target(&target)
    }

    /// Observe the exact typed value of a working-copy or shelf filesystem effect.
    pub(super) fn observe_filesystem_effect(
        &self,
        working_copy: WorkingCopyId,
        target: &EffectTarget,
    ) -> Result<EffectValue, RepositoryError> {
        let path = self
            .filesystem_effect_path(working_copy, target)?
            .ok_or_else(|| unsupported_effect_error(target))?;
        observe_path(&path, is_recursive_filesystem_target(target))
    }

    fn require_complete_single_head(
        &self,
        working_copy: WorkingCopyId,
        expected: OperationId,
    ) -> Result<(), RepositoryError> {
        let scope = OperationScope::WorkingCopy(working_copy);
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        let heads = txn.get_operation_heads(scope).map_err(pristine_error)?;
        match heads.as_slice() {
            [actual] if *actual == expected => {
                let receipts = txn
                    .get_effect_receipts(*actual)
                    .map_err(pristine_error)?;
                if has_operation_verified_receipt(&receipts) {
                    Ok(())
                } else {
                    Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "operation head {actual} is incomplete; recover it before preparing another operation"
                        ),
                    })
                }
            }
            [actual] => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "operation head changed while preparing switch: expected {expected}, found {actual}"
                ),
            }),
            many => Err(multiple_heads_error(scope, many)),
        }
    }

    pub(super) fn load_operation(
        &self,
        operation_id: OperationId,
    ) -> Result<Operation, RepositoryError> {
        let txn = self.pristine.read_txn().map_err(pristine_error)?;
        txn.get_operation(operation_id)
            .map_err(pristine_error)?
            .ok_or_else(|| RepositoryError::InvalidOperation {
                message: format!("operation not found: {operation_id}"),
            })
    }

    fn validate_operation_lock(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation: &Operation,
    ) -> Result<(), RepositoryError> {
        self.validate_projection_lock(operation_lock, operation)
    }

    /// Lock validation exposed to the projection executor (CB-8B).
    pub(super) fn validate_projection_lock(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation: &Operation,
    ) -> Result<(), RepositoryError> {
        let locked = operation_lock.working_copy();
        match operation.payload().working_copy {
            Some(operation_working_copy) if operation_working_copy == locked => Ok(()),
            Some(operation_working_copy) => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "operation {} belongs to working copy {operation_working_copy}, but lock is for {locked}",
                    operation.id()
                ),
            }),
            None => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "repository-scoped operation {} cannot use a working-copy recovery lock",
                    operation.id()
                ),
            }),
        }
    }

    pub(super) fn append_receipt_immediate(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation: &Operation,
        receipt: &EffectReceipt,
    ) -> Result<(), RepositoryError> {
        self.validate_operation_lock(operation_lock, operation)?;
        let mut txn = operation_lock.begin_write_immediate()?;
        txn.append_effect_receipt(receipt).map_err(pristine_error)?;
        txn.commit()
    }

    fn preflight_filesystem_leases(
        &self,
        working_copy: WorkingCopyId,
        operation: &Operation,
    ) -> Result<(), RepositoryError> {
        let mut simulated: Vec<(EffectTarget, EffectValue)> = Vec::new();
        for effect in &operation.payload().delta.effects {
            let observed = if let Some((_, value)) = simulated
                .iter()
                .find(|(target, _)| *target == effect.target)
            {
                Some(value.clone())
            } else if let Some(path) = self.filesystem_effect_path(working_copy, &effect.target)? {
                Some(observe_path(
                    &path,
                    is_recursive_filesystem_target(&effect.target),
                )?)
            } else if matches!(&effect.target, EffectTarget::WorkingCopy { .. }) {
                let target = self.resolve_recovery_target(working_copy, &effect.target)?;
                Some(self.observe_recovery_target(&target)?)
            } else {
                None
            };
            if let Some(observed) = observed {
                if observed != effect.expected_old {
                    return Err(lease_divergence_error(effect, &observed, None));
                }
                if let Some((_, value)) = simulated
                    .iter_mut()
                    .find(|(target, _)| *target == effect.target)
                {
                    *value = effect.expected_new.clone();
                } else {
                    simulated.push((effect.target.clone(), effect.expected_new.clone()));
                }
            }
        }
        Ok(())
    }

    fn snapshot_operation_filesystem(
        &self,
        operation_lock: &WorkingCopyOperationLockGuard,
        operation: &Operation,
    ) -> Result<PathBuf, RepositoryError> {
        self.validate_operation_lock(operation_lock, operation)?;
        let working_copy = operation_lock.working_copy();
        let root = self.operation_recovery_root(working_copy, operation.id());
        let complete = root.join(BACKUP_COMPLETE);
        if complete.is_file() {
            self.verify_backup(operation, &root)?;
            return Ok(root);
        }

        if root.exists() {
            let mut seen = Vec::new();
            for effect in &operation.payload().delta.effects {
                let Some(path) = self.filesystem_effect_path(working_copy, &effect.target)? else {
                    continue;
                };
                if seen.contains(&effect.target) {
                    continue;
                }
                seen.push(effect.target.clone());
                let observed = observe_path(&path, is_recursive_filesystem_target(&effect.target))?;
                if observed != effect.expected_old {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "incomplete recovery backup for operation {} cannot be rebuilt after effect {} moved away from expected-old",
                            operation.id(), effect.ordinal
                        ),
                    });
                }
            }
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(root.join(BACKUP_ENTRIES_DIR))?;

        let mut backed_up = Vec::new();
        for effect in &operation.payload().delta.effects {
            let Some(source) = self.filesystem_effect_path(working_copy, &effect.target)? else {
                continue;
            };
            if backed_up.contains(&effect.target) {
                continue;
            }
            backed_up.push(effect.target.clone());
            let observed_before =
                observe_path(&source, is_recursive_filesystem_target(&effect.target))?;
            if observed_before != effect.expected_old {
                return Err(lease_divergence_error(effect, &observed_before, None));
            }
            let entry = backup_entry_path(&root, effect.ordinal);
            fs::create_dir_all(&entry)?;
            match &effect.expected_old {
                EffectValue::Absent => {
                    write_new_synced(&entry.join(BACKUP_ABSENT), BACKUP_VERSION)?;
                }
                EffectValue::File(_) => {
                    let destination = entry.join(BACKUP_VALUE);
                    copy_entry(&source, &destination)?;
                    let backup_value =
                        observe_path(&destination, is_recursive_filesystem_target(&effect.target))?;
                    if backup_value != effect.expected_old {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "filesystem backup for operation {} effect {} does not match expected-old",
                                operation.id(), effect.ordinal
                            ),
                        });
                    }
                }
                value => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                        "filesystem effect {} expected-old must be Absent or File, found {value:?}",
                        effect.ordinal
                    ),
                    })
                }
            }
            let observed_after =
                observe_path(&source, is_recursive_filesystem_target(&effect.target))?;
            if observed_after != effect.expected_old {
                return Err(lease_divergence_error(effect, &observed_after, None));
            }
            sync_directory(&entry)?;
        }

        write_atomic_regular(&complete, BACKUP_VERSION, 0o600)?;
        sync_directory(&root)?;
        if let Some(parent) = root.parent() {
            sync_directory(parent)?;
        }
        self.verify_backup(operation, &root)?;
        Ok(root)
    }

    fn verify_backup(&self, operation: &Operation, root: &Path) -> Result<(), RepositoryError> {
        let marker = fs::read(root.join(BACKUP_COMPLETE))?;
        if marker != BACKUP_VERSION {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "operation recovery backup {} has an invalid complete marker",
                    root.display()
                ),
            });
        }
        let mut verified_targets = Vec::new();
        for effect in &operation.payload().delta.effects {
            if !is_filesystem_effect(&effect.target) || verified_targets.contains(&effect.target) {
                continue;
            }
            verified_targets.push(effect.target.clone());
            let entry = backup_entry_path(root, effect.ordinal);
            match &effect.expected_old {
                EffectValue::Absent => {
                    if fs::read(entry.join(BACKUP_ABSENT))? != BACKUP_VERSION {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "operation {} effect {} has an invalid absent backup",
                                operation.id(),
                                effect.ordinal
                            ),
                        });
                    }
                }
                EffectValue::File(_) => {
                    let observed = observe_path(
                        &entry.join(BACKUP_VALUE),
                        is_recursive_filesystem_target(&effect.target),
                    )?;
                    if observed != effect.expected_old {
                        return Err(RepositoryError::InvalidOperation {
                            message: format!(
                                "operation {} effect {} backup no longer matches expected-old",
                                operation.id(),
                                effect.ordinal
                            ),
                        });
                    }
                }
                value => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                        "filesystem effect {} expected-old must be Absent or File, found {value:?}",
                        effect.ordinal
                    ),
                    })
                }
            }
        }
        Ok(())
    }

    fn replay_filesystem_recovery(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
        original: &Operation,
        recovery: &Operation,
    ) -> Result<(), RepositoryError> {
        self.validate_operation_lock(operation_lock, recovery)?;
        let working_copy = operation_lock.working_copy();
        let backup_root = self.operation_recovery_root(working_copy, original.id());
        let receipts = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.get_effect_receipts(recovery.id())
                .map_err(pristine_error)?
        };
        let completed: BTreeSet<u32> = receipts
            .iter()
            .filter_map(|receipt| match receipt.payload().kind {
                EffectReceiptKind::Applied
                | EffectReceiptKind::Recovered
                | EffectReceiptKind::RolledBack => receipt.payload().effect_ordinal,
                EffectReceiptKind::Verified | EffectReceiptKind::LeaseRejected => None,
            })
            .collect();

        let mut classified = Vec::with_capacity(recovery.payload().delta.effects.len());
        let mut simulated: Vec<(EffectTarget, EffectValue)> = Vec::new();
        for effect in &recovery.payload().delta.effects {
            let target = self.resolve_recovery_target(working_copy, &effect.target)?;
            let observed = if let Some((_, value)) = simulated
                .iter()
                .find(|(candidate, _)| *candidate == effect.target)
            {
                value.clone()
            } else {
                self.observe_recovery_target(&target)?
            };
            let classification =
                classify_effect_lease(&observed, &effect.expected_old, &effect.expected_new);
            if classification == LeaseClassification::Diverged {
                let receipt = deterministic_effect_receipt(
                    recovery,
                    Some(effect.ordinal),
                    EffectReceiptKind::LeaseRejected,
                    Some(observed.clone()),
                    None,
                )?;
                self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                return Err(lease_divergence_error(effect, &observed, None));
            }
            if let Some((_, value)) = simulated
                .iter_mut()
                .find(|(candidate, _)| *candidate == effect.target)
            {
                *value = effect.expected_new.clone();
            } else {
                simulated.push((effect.target.clone(), effect.expected_new.clone()));
            }
            classified.push((effect, target));
        }

        for (effect, target) in classified {
            let observed = self.observe_recovery_target(&target)?;
            let classification =
                classify_effect_lease(&observed, &effect.expected_old, &effect.expected_new);
            // CB-8B: a PROJECTION PUBLICATION's checkpoint effect is
            // derived evidence, never an authority. Its recovery is a
            // no-op — the checkpoint only publishes through a lease that
            // classifies against the live facts, and the retry's own
            // publication re-derives and writes it. Fabricating the
            // post-publication checkpoint here (when the operation's Git
            // effects never landed) would publish state the journal never
            // reached. Other checkpoint effects keep the adopted
            // rollback-to-before contract (CB-7B's adoption completion).
            if classification == LeaseClassification::Apply
                && matches!(
                    effect.target,
                    EffectTarget::Checkpoint {
                        kind: CheckpointKind::Bridge,
                        ..
                    }
                )
                && original.payload().kind == OperationKind::ExportGitRefs
            {
                if !completed.contains(&effect.ordinal) {
                    let receipt = deterministic_effect_receipt(
                        recovery,
                        Some(effect.ordinal),
                        EffectReceiptKind::Recovered,
                        Some(observed.clone()),
                        Some(observed),
                    )?;
                    self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                }
                continue;
            }
            match classification {
                LeaseClassification::Apply => {
                    let mut shelf_txn = if matches!(
                        effect.target,
                        EffectTarget::ShelfPath { .. } | EffectTarget::WorkspacePath { .. }
                    ) {
                        Some(operation_lock.begin_write_immediate()?.try_lock_shelf()?)
                    } else {
                        None
                    };
                    self.restore_original_effect(
                        operation_lock,
                        original,
                        effect,
                        &target,
                        &backup_root,
                    )?;
                    let restored = self.observe_recovery_target(&target)?;
                    if restored != effect.expected_new {
                        let receipt = deterministic_effect_receipt(
                            recovery,
                            Some(effect.ordinal),
                            EffectReceiptKind::LeaseRejected,
                            Some(observed.clone()),
                            Some(restored.clone()),
                        )?;
                        if let Some(mut txn) = shelf_txn {
                            txn.append_effect_receipt(&receipt)
                                .map_err(pristine_error)?;
                            txn.commit()?;
                        } else {
                            self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                        }
                        return Err(lease_divergence_error(effect, &observed, Some(&restored)));
                    }
                    let receipt = deterministic_effect_receipt(
                        recovery,
                        Some(effect.ordinal),
                        EffectReceiptKind::RolledBack,
                        Some(observed),
                        Some(restored),
                    )?;
                    if let Some(mut txn) = shelf_txn.take() {
                        txn.append_effect_receipt(&receipt)
                            .map_err(pristine_error)?;
                        txn.commit()?;
                    } else {
                        self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                    }
                }
                LeaseClassification::AlreadyApplied => {
                    if let (
                        ResolvedRecoveryTarget::WorkingCopy(id),
                        EffectValue::WorkingCopy(state),
                    ) = (&target, &effect.expected_new)
                    {
                        if *id != state.id {
                            return Err(RepositoryError::InvalidOperation {
                                message: format!(
                                    "working-copy recovery target {id} disagrees with state {}",
                                    state.id
                                ),
                            });
                        }
                        self.apply_working_copy_state_locked(operation_lock, state)?;
                    }
                    if !completed.contains(&effect.ordinal) {
                        let receipt = deterministic_effect_receipt(
                            recovery,
                            Some(effect.ordinal),
                            EffectReceiptKind::Recovered,
                            Some(observed.clone()),
                            Some(observed),
                        )?;
                        self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                    }
                }
                LeaseClassification::Diverged => {
                    let receipt = deterministic_effect_receipt(
                        recovery,
                        Some(effect.ordinal),
                        EffectReceiptKind::LeaseRejected,
                        Some(observed.clone()),
                        None,
                    )?;
                    self.append_receipt_immediate(operation_lock, recovery, &receipt)?;
                    return Err(lease_divergence_error(effect, &observed, None));
                }
            }
        }
        Ok(())
    }

    fn restore_original_effect(
        &mut self,
        operation_lock: &WorkingCopyOperationLockGuard,
        original: &Operation,
        recovery_effect: &EffectPlan,
        target: &ResolvedRecoveryTarget,
        backup_root: &Path,
    ) -> Result<(), RepositoryError> {
        let matches: Vec<&EffectPlan> = original
            .payload()
            .delta
            .effects
            .iter()
            .filter(|effect| {
                recovery_effect.target == effect.target
                    && recovery_effect.expected_new == effect.expected_old
                    && recovery_effect.expected_old == effect.expected_new
            })
            .collect();
        let original_effect = match matches.as_slice() {
            [effect] => *effect,
            _ => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                    "recovery effect {} does not identify exactly one original inverse transition",
                    recovery_effect.ordinal
                ),
                })
            }
        };

        match (&original_effect.expected_old, target) {
            (EffectValue::Absent, ResolvedRecoveryTarget::Filesystem { path, .. }) => {
                let tombstone_root =
                    if matches!(original_effect.target, EffectTarget::ShelfPath { .. }) {
                        backup_root.to_path_buf()
                    } else {
                        self.working_copy_effect_recovery_root(
                            operation_lock.working_copy(),
                            original.id(),
                        )
                    };
                remove_entry_for_recovery(
                    path,
                    &tombstone_root
                        .join("rolled-back-effects")
                        .join(format!("{:010}", original_effect.ordinal)),
                )
            }
            (EffectValue::File(state), ResolvedRecoveryTarget::Filesystem { path, .. }) => {
                self.verify_backup(original, backup_root)?;
                let source =
                    backup_entry_path(backup_root, original_effect.ordinal).join(BACKUP_VALUE);
                match state.kind {
                    FileKind::Regular => write_atomic_regular_from_file(path, &source, state.mode),
                    FileKind::Directory => replace_directory(path, &source, state.mode),
                    FileKind::Symlink => replace_symlink(path, &source),
                    FileKind::Gitlink => replace_directory(path, &source, state.mode),
                }
            }
            (EffectValue::WorkingCopy(state), ResolvedRecoveryTarget::WorkingCopy(id))
                if state.id == *id =>
            {
                self.apply_working_copy_state_locked(operation_lock, state)
            }
            // R3: the adoption completion's WIP-ref lease rolls back by
            // recreating the exact Git object identity the operation observed
            // before the drop (the commit object is content-addressed, so the
            // recreate is exact).
            (EffectValue::GitRef(target), ResolvedRecoveryTarget::GitRef(name)) => {
                let repo = git2::Repository::open(&self.root).map_err(|error| {
                    RepositoryError::InvalidRepository {
                        reason: format!("cannot open the Git repository to restore a ref: {error}"),
                    }
                })?;
                let GitRefTarget::Direct(object) = target else {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "recovery of Git ref '{name}' only supports direct object leases"
                        ),
                    });
                };
                let oid = git_oid_from_object(&repo, object)?;
                repo.reference(
                    name,
                    oid,
                    true,
                    "atomic: restore rolled-back completion ref",
                )
                .map(|_| ())
                .map_err(|error| RepositoryError::InvalidRepository {
                    reason: format!("cannot restore Git ref '{name}': {error}"),
                })
            }
            (EffectValue::Absent, ResolvedRecoveryTarget::GitRef(name)) => {
                let repo = git2::Repository::open(&self.root).map_err(|error| {
                    RepositoryError::InvalidRepository {
                        reason: format!("cannot open the Git repository to restore a ref: {error}"),
                    }
                })?;
                let found = repo.find_reference(name);
                match found {
                    Ok(mut reference) => {
                        reference
                            .delete()
                            .map_err(|error| RepositoryError::InvalidRepository {
                                reason: format!("cannot re-delete Git ref '{name}': {error}"),
                            })
                    }
                    Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(()),
                    Err(error) => Err(RepositoryError::InvalidRepository {
                        reason: format!("cannot read Git ref '{name}' during recovery: {error}"),
                    }),
                }
            }
            // CB-8B: the projection publish's HEAD lease rolls back by
            // re-pointing HEAD at the exact state the operation observed
            // before the move: attached (symbolic) or detached (direct). The
            // pre-classification guard (the caller) ensures an external
            // writer's newer HEAD is never overwritten.
            (EffectValue::GitRef(target), ResolvedRecoveryTarget::GitHead(_)) => {
                let repo = git2::Repository::open(&self.root).map_err(|error| {
                    RepositoryError::InvalidRepository {
                        reason: format!("cannot open the Git repository to restore HEAD: {error}"),
                    }
                })?;
                match target {
                    GitRefTarget::Direct(object) => {
                        let oid = git_oid_from_object(&repo, object)?;
                        repo.set_head_detached(oid).map_err(|error| {
                            RepositoryError::InvalidRepository {
                                reason: format!("cannot restore Git HEAD: {error}"),
                            }
                        })
                    }
                    GitRefTarget::Symbolic(symref) => {
                        repo.set_head(symref)
                            .map_err(|error| RepositoryError::InvalidRepository {
                                reason: format!("cannot restore attached Git HEAD: {error}"),
                            })
                    }
                }
            }
            // R3: the derived bridge checkpoint is restored to its
            // pre-completion content, re-derived from the original
            // operation's recorded before-state (view + Git facts). The
            // caller re-observes the digest and rejects any third value.
            (
                EffectValue::Digest {
                    kind: DigestKind::Checkpoint,
                    ..
                },
                ResolvedRecoveryTarget::Checkpoint { .. },
            ) => {
                let repo = git2::Repository::open(&self.root).map_err(|error| {
                    RepositoryError::InvalidRepository {
                        reason: format!(
                            "cannot open the Git repository to restore the checkpoint: {error}"
                        ),
                    }
                })?;
                let restored = self.restore_checkpoint_from_state(&repo, original)?;
                write_workspace_checkpoint(self.root(), &restored)
            }
            // CB-8B: a journaled projection index replacement rolls back by
            // restoring the exact index bytes retained before the swap (or
            // re-removing an index that did not exist). The caller
            // re-observes the lease and rejects any third value.
            (EffectValue::GitIndex(_), ResolvedRecoveryTarget::GitIndex(_)) => {
                let repo = git2::Repository::open(&self.root).map_err(|error| {
                    RepositoryError::InvalidRepository {
                        reason: format!(
                            "cannot open the Git repository to restore the index: {error}"
                        ),
                    }
                })?;
                self.restore_retained_index(original, backup_root, repo.path())
            }
            (value, target) => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "recovery cannot restore value {value:?} through resolved target {target:?}"
                ),
            }),
        }
    }

    /// The effect ordinals of `original` whose inverse must replay during
    /// a recovery or an undo (CB-8B lease-chain semantics, shared by both
    /// paths): per target, every attempt up to the furthest state the
    /// receipts prove was reached. A target whose every attempt ended in
    /// a durable LeaseRejected receipt never mutated and contributes
    /// nothing.
    fn selected_inverse_ordinals(
        &self,
        working_copy: WorkingCopyId,
        original: &Operation,
    ) -> Result<BTreeSet<u32>, RepositoryError> {
        let receipts = {
            let txn = self.pristine.read_txn().map_err(pristine_error)?;
            txn.get_effect_receipts(original.id())
                .map_err(pristine_error)?
        };
        let completed: BTreeSet<u32> = receipts
            .iter()
            .filter_map(|receipt| match receipt.payload().kind {
                EffectReceiptKind::Applied
                | EffectReceiptKind::Recovered
                | EffectReceiptKind::RolledBack => receipt.payload().effect_ordinal,
                EffectReceiptKind::Verified | EffectReceiptKind::LeaseRejected => None,
            })
            .collect();
        let rejected: BTreeSet<u32> = receipts
            .iter()
            .filter_map(|receipt| match receipt.payload().kind {
                EffectReceiptKind::LeaseRejected => receipt.payload().effect_ordinal,
                _ => None,
            })
            .collect();

        let original_effects = &original.payload().delta.effects;
        let mut selected = BTreeSet::new();
        let mut visited_targets = Vec::new();
        for first in original_effects {
            if visited_targets.contains(&first.target) {
                continue;
            }
            visited_targets.push(first.target.clone());
            // CB-8B: a PROJECTION PUBLICATION's object creations are
            // content-addressed and immutable. Their presence is idempotent,
            // they are never deleted, and unreachable objects are reclaimed
            // by CB-13A's GC — so no inverse recovery effect exists for them.
            if matches!(first.target, EffectTarget::GitObject { .. })
                && original.payload().kind == OperationKind::ExportGitRefs
            {
                continue;
            }
            let chain: Vec<&EffectPlan> = original_effects
                .iter()
                .filter(|effect| effect.target == first.target)
                .collect();
            // CB-8B: a target whose every attempt ended in a durable
            // LeaseRejected receipt never mutated (the executor records the
            // rejection only when the observed value is a third value and
            // writes nothing), and later effects on that target never ran.
            // There is nothing to roll back; the observed third value — the
            // newer external work — must simply be preserved, so the target
            // contributes no inverse effect.
            let chain_completed = chain
                .iter()
                .any(|effect| completed.contains(&effect.ordinal));
            let chain_rejected = chain
                .iter()
                .all(|effect| rejected.contains(&effect.ordinal));
            if !chain_completed && chain_rejected {
                continue;
            }
            let target = self.resolve_recovery_target(working_copy, &first.target)?;
            let observed = self.observe_recovery_target(&target)?;
            let mut states = Vec::with_capacity(chain.len() + 1);
            states.push(chain[0].expected_old.clone());
            states.extend(chain.iter().map(|effect| effect.expected_new.clone()));
            let minimum_reached = chain
                .iter()
                .enumerate()
                .filter(|(_, effect)| completed.contains(&effect.ordinal))
                .map(|(index, _)| index + 1)
                .max()
                .unwrap_or(0);
            let candidates: Vec<usize> = states
                .iter()
                .enumerate()
                .filter_map(|(index, state)| {
                    (index >= minimum_reached && state == &observed).then_some(index)
                })
                .collect();
            let reached = match candidates.as_slice() {
                [reached] => *reached,
                // The executor commits each stage's receipt before it may run
                // the next transition for the same target. With no successful
                // receipt, a cyclic endpoint equal to the initial value is
                // therefore unambiguously the not-started state.
                [first, ..] if minimum_reached == 0 && *first == 0 => 0,
                [] => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "operation {} target {:?} diverged before recovery: observed {:?}, expected one of {:?}",
                            original.id(), first.target, observed, states
                        ),
                    })
                }
                _ => {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "operation {} target {:?} has an ambiguous cyclic lease chain at observed value {:?} (expected one of {:?})",
                            original.id(), first.target, observed, states
                        ),
                    })
                }
            };
            for effect in chain.into_iter().take(reached) {
                selected.insert(effect.ordinal);
            }
        }
        Ok(selected)
    }

    fn inverse_recovery_operation(
        &self,
        original: &Operation,
        working_copy: WorkingCopyId,
    ) -> Result<Operation, RepositoryError> {
        let selected = self.selected_inverse_ordinals(working_copy, original)?;
        let original_effects = &original.payload().delta.effects;

        let mut metadata = Vec::new();
        for transition in &original.payload().delta.metadata {
            let observed = self.observe_metadata_value(&transition.target)?;
            match classify_metadata_lease(
                &observed,
                &transition.expected_old,
                &transition.expected_new,
            ) {
                LeaseClassification::Apply => {}
                LeaseClassification::AlreadyApplied => metadata.push(MetadataTransition {
                    target: transition.target.clone(),
                    expected_old: transition.expected_new.clone(),
                    expected_new: transition.expected_old.clone(),
                }),
                LeaseClassification::Diverged => {
                    return Err(metadata_divergence_error(transition, &observed));
                }
            }
        }

        let effects = original_effects
            .iter()
            .rev()
            .filter(|effect| selected.contains(&effect.ordinal))
            .enumerate()
            .map(|(ordinal, effect)| {
                Ok(EffectPlan {
                    ordinal: u32::try_from(ordinal).map_err(|_| {
                        RepositoryError::InvalidOperation {
                            message: "too many effects to construct inverse recovery operation"
                                .to_string(),
                        }
                    })?,
                    target: effect.target.clone(),
                    expected_old: effect.expected_new.clone(),
                    expected_new: effect.expected_old.clone(),
                })
            })
            .collect::<Result<Vec<_>, RepositoryError>>()?;
        Operation::new(OperationPayload {
            parents: vec![original.id()],
            kind: OperationKind::Recover,
            relation: None,
            working_copy: original.payload().working_copy,
            before: original.payload().delta.after.clone(),
            delta: RepoStateDelta {
                after: original.payload().before.clone(),
                metadata,
                effects,
            },
            git_observed: Vec::new(),
            evidence: Vec::new(),
            actor: ActorRef::System {
                name: RECOVERY_ACTOR.to_string(),
            },
            timestamp_ms: original.payload().timestamp_ms,
            lossy: Vec::new(),
        })
        .map_err(codec_error)
    }

    fn resolve_recovery_target(
        &self,
        working_copy: WorkingCopyId,
        target: &EffectTarget,
    ) -> Result<ResolvedRecoveryTarget, RepositoryError> {
        if let Some(path) = self.filesystem_effect_path(working_copy, target)? {
            return Ok(ResolvedRecoveryTarget::Filesystem {
                path,
                recursive_directory: is_recursive_filesystem_target(target),
            });
        }
        match target {
            EffectTarget::WorkingCopy {
                working_copy: target_working_copy,
            } if *target_working_copy == working_copy => {
                Ok(ResolvedRecoveryTarget::WorkingCopy(working_copy))
            }
            EffectTarget::WorkingCopy {
                working_copy: target_working_copy,
            } => Err(RepositoryError::InvalidOperation {
                message: format!(
                    "working-copy effect belongs to {target_working_copy}, operation lock is for {working_copy}"
                ),
            }),
            EffectTarget::GitRef { name } => {
                // R3 + CB-8B: Atomic-owned Git refs are recoverable by this
                // engine — the WIP recovery refs (CB-1B), the Draft view
                // reachability refs `refs/atomic/views/<name>`, and the
                // shared projection branches `refs/heads/<name>` written by
                // the journaled projection publisher. Every inverse effect
                // classifies its lease before touching the ref, so an
                // external writer's newer value is never overwritten. Other
                // namespaces stay gated (the repository never invents an
                // after-state for a foreign interrupted write).
                let owned = name.starts_with("refs/atomic/wip/")
                    || name.starts_with("refs/atomic/views/")
                    || (name.starts_with("refs/heads/") && name.len() > "refs/heads/".len());
                if !owned {
                    return Err(unsupported_effect_error(target));
                }
                Ok(ResolvedRecoveryTarget::GitRef(name.clone()))
            }
            EffectTarget::GitHead { working_copy: target_working_copy } => {
                if *target_working_copy != working_copy {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "Git HEAD effect belongs to {target_working_copy}, operation lock is for {working_copy}"
                        ),
                    });
                }
                Ok(ResolvedRecoveryTarget::GitHead(working_copy))
            }
            EffectTarget::Checkpoint {
                working_copy: target_working_copy,
                kind,
            } => {
                if *kind != CheckpointKind::Bridge {
                    return Err(unsupported_effect_error(target));
                }
                if *target_working_copy != working_copy {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "checkpoint effect belongs to {target_working_copy}, operation lock is for {working_copy}"
                        ),
                    });
                }
                Ok(ResolvedRecoveryTarget::Checkpoint { working_copy })
            }
            EffectTarget::GitObject { object } => {
                Ok(ResolvedRecoveryTarget::GitObject(object.clone()))
            }
            EffectTarget::GitIndex {
                working_copy: target_working_copy,
            } => {
                if *target_working_copy != working_copy {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "Git index effect belongs to {target_working_copy}, operation lock is for {working_copy}"
                        ),
                    });
                }
                Ok(ResolvedRecoveryTarget::GitIndex(working_copy))
            }
            _ => Err(unsupported_effect_error(target)),
        }
    }

    fn observe_recovery_target(
        &self,
        target: &ResolvedRecoveryTarget,
    ) -> Result<EffectValue, RepositoryError> {
        match target {
            ResolvedRecoveryTarget::Filesystem {
                path,
                recursive_directory,
            } => observe_path(path, *recursive_directory),
            ResolvedRecoveryTarget::WorkingCopy(id) => {
                let txn = self.pristine.read_txn().map_err(pristine_error)?;
                let record = txn
                    .get_working_copy(*id)
                    .map_err(pristine_error)?
                    .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: *id })?;
                Ok(EffectValue::WorkingCopy(working_copy_state_ref(record)))
            }
            ResolvedRecoveryTarget::GitRef(name) => {
                let repo = git2::Repository::open(&self.root).map_err(|error| {
                    RepositoryError::InvalidRepository {
                        reason: format!("cannot open the Git repository for a ref lease: {error}"),
                    }
                })?;
                let found = repo.find_reference(name);
                match found {
                    Ok(reference) => {
                        let oid = reference.target().ok_or_else(|| {
                            RepositoryError::InvalidOperation {
                                message: format!("Git ref '{name}' has no direct target"),
                            }
                        })?;
                        let algorithm = match oid.as_bytes().len() {
                            20 => GitHashAlgorithm::Sha1,
                            32 => GitHashAlgorithm::Sha256,
                            other => {
                                return Err(RepositoryError::InvalidRepository {
                                    reason: format!("unexpected Git OID width {other}"),
                                })
                            }
                        };
                        let object = GitObjectId::new(algorithm, oid.as_bytes().to_vec())
                            .map_err(codec_error)?;
                        Ok(EffectValue::GitRef(GitRefTarget::Direct(object)))
                    }
                    Err(error) if error.code() == git2::ErrorCode::NotFound => {
                        Ok(EffectValue::Absent)
                    }
                    Err(error) => Err(RepositoryError::InvalidRepository {
                        reason: format!("cannot read Git ref '{name}' for a lease: {error}"),
                    }),
                }
            }
            ResolvedRecoveryTarget::Checkpoint { .. } => Ok(EffectValue::Digest {
                kind: DigestKind::Checkpoint,
                hash: self.read_workspace_checkpoint_facts_digest()?,
            }),
            ResolvedRecoveryTarget::GitObject(object) => {
                let repo = git2::Repository::open(&self.root).map_err(|error| {
                    RepositoryError::InvalidRepository {
                        reason: format!(
                            "cannot open the Git repository for an object lease: {error}"
                        ),
                    }
                })?;
                super::projection_effects::observe_git_object_lease(&repo, object)
            }
            ResolvedRecoveryTarget::GitIndex(_) => {
                let repo = git2::Repository::open(&self.root).map_err(|error| {
                    RepositoryError::InvalidRepository {
                        reason: format!(
                            "cannot open the Git repository for an index lease: {error}"
                        ),
                    }
                })?;
                Ok(EffectValue::GitIndex(
                    super::projection_effects::observe_git_index_lease(&repo)?,
                ))
            }
            ResolvedRecoveryTarget::GitHead(_) => {
                let repo = git2::Repository::open(&self.root).map_err(|error| {
                    RepositoryError::InvalidRepository {
                        reason: format!("cannot open the Git repository for a HEAD lease: {error}"),
                    }
                })?;
                // The same lease protocol the projection executor records
                // (CB-8B ac-3): symbolic when attached, direct when detached,
                // absent when HEAD is missing or unborn. The resolved-commit
                // reading would misclassify an attached HEAD against a
                // symbolic expected-old lease.
                let observed = super::projection_effects::read_head_target(&repo)?;
                Ok(observed
                    .map(EffectValue::GitRef)
                    .unwrap_or(EffectValue::Absent))
            }
        }
    }

    fn filesystem_effect_path(
        &self,
        working_copy: WorkingCopyId,
        target: &EffectTarget,
    ) -> Result<Option<PathBuf>, RepositoryError> {
        match target {
            EffectTarget::FilesystemPath { path } => {
                let content_store_effect =
                    Path::new(path).starts_with(Path::new(".atomic/changes"));
                // CB-13B R2: `.git/hooks/<name>` is the one `.git`-rooted
                // surface a journaled effect may target — the hook
                // decommission migration lease. Exactly one hook-file
                // component; the effect machinery's symlink-parent checks
                // and per-ordinal backups apply unchanged.
                let git_hook_effect = is_git_hooks_effect_path(path);
                let relative =
                    validate_relative_path(path, content_store_effect || git_hook_effect)?;
                if relative.starts_with(Path::new(".atomic"))
                    && !relative.starts_with(Path::new(".atomic/changes"))
                {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!("operation filesystem effect cannot target '{path}'"),
                    });
                }
                resolve_without_symlink_parents(&self.root, &relative).map(Some)
            }
            EffectTarget::WorkspacePath {
                working_copy: target_working_copy,
                path,
            } => {
                if *target_working_copy != working_copy {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "workspace effect belongs to working copy {target_working_copy}, operation lock is for {working_copy}"
                        ),
                    });
                }
                let relative = validate_relative_path(path, false)?;
                resolve_without_symlink_parents(&self.root, &relative).map(Some)
            }
            EffectTarget::ShelfPath {
                working_copy: target_working_copy,
                view,
                path,
            } => {
                if *target_working_copy != working_copy {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "shelf effect belongs to working copy {target_working_copy}, operation lock is for {working_copy}"
                        ),
                    });
                }
                let view = validate_relative_path(view, true)?;
                let path = validate_relative_path(path, true)?;
                let base = self
                    .dot_dir
                    .join("working-copies")
                    .join(working_copy.to_string())
                    .join("workspaces");
                let view_root = resolve_without_symlink_parents(&base, &view)?;
                resolve_without_symlink_parents(&view_root, &path).map(Some)
            }
            _ => Ok(None),
        }
    }

    fn working_copy_effect_recovery_root(
        &self,
        _working_copy: WorkingCopyId,
        operation: OperationId,
    ) -> PathBuf {
        self.working_copy_dot_dir()
            .join(RECOVERY_DIR)
            .join(operation.to_string())
    }

    pub(super) fn operation_recovery_root(
        &self,
        working_copy: WorkingCopyId,
        original: OperationId,
    ) -> PathBuf {
        self.dot_dir
            .join("working-copies")
            .join(working_copy.to_string())
            .join(RECOVERY_DIR)
            .join(original.to_string())
    }
}

fn classify_metadata_lease(
    observed: &MetadataValue,
    expected_old: &MetadataValue,
    expected_new: &MetadataValue,
) -> LeaseClassification {
    if observed == expected_old {
        LeaseClassification::Apply
    } else if observed == expected_new {
        LeaseClassification::AlreadyApplied
    } else {
        LeaseClassification::Diverged
    }
}

/// The view field a `MetadataTarget::View` lease moves: the parent pointer.
/// View state and change count belong to the `ViewChange` leases of the same
/// operation; within the journal's serialization the parent is the only
/// field a metadata transition may move directly.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ViewLeaseValue {
    pub parent: Option<u64>,
}

fn encode_view_lease(
    view: &atomic_core::pristine::ViewState,
) -> Result<MetadataValue, RepositoryError> {
    let lease = ViewLeaseValue {
        parent: view.parent,
    };
    postcard::to_allocvec(&lease)
        .map(MetadataValue::Bytes)
        .map_err(|error| RepositoryError::Serialization(error.to_string()))
}

fn decode_view_lease(value: &MetadataValue, name: &str) -> Result<ViewLeaseValue, RepositoryError> {
    let MetadataValue::Bytes(bytes) = value else {
        return Err(RepositoryError::InvalidOperation {
            message: format!("view lease for '{name}' requires canonical bytes"),
        });
    };
    postcard::from_bytes(bytes).map_err(|error| RepositoryError::Serialization(error.to_string()))
}

fn decode_ref_mapping_value(
    value: &MetadataValue,
    view: &str,
) -> Result<atomic_core::pristine::RefMapping, RepositoryError> {
    let MetadataValue::Bytes(bytes) = value else {
        return Err(RepositoryError::InvalidOperation {
            message: format!("ref-mapping lease for '{view}' requires canonical bytes"),
        });
    };
    atomic_core::pristine::RefMapping::decode(bytes)
        .map_err(|error| RepositoryError::Serialization(error.to_string()))
}

/// The complete effective change-filter of `view` as hashes (CB-10A closure
/// containment input). Dependency-first closure with external hashes; changes
/// without a registered external hash are skipped by construction because
/// they cannot be compared against Git closure evidence.
pub(super) fn visible_change_hashes(
    txn: &atomic_core::pristine::ReadTxn,
    view: &atomic_core::pristine::ViewState,
) -> Result<BTreeSet<Hash>, RepositoryError> {
    let visibility = super::filter::graph_visibility_closure(txn, view)
        .map_err(|error| RepositoryError::Database(error.to_string()))?;
    let mut hashes = BTreeSet::new();
    for change_id in visibility.iter_dependency_first().copied() {
        if let Some(hash) = txn.get_external(change_id).map_err(pristine_error)? {
            hashes.insert(hash);
        }
    }
    Ok(hashes)
}

fn observe_metadata_value_in_write(
    txn: &atomic_core::pristine::WriteTxn<'_>,
    target: &MetadataTarget,
) -> Result<MetadataValue, RepositoryError> {
    match target {
        MetadataTarget::ViewChange { view, change } => {
            let view = txn
                .get_view(view)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ViewNotFound { name: view.clone() })?;
            let Some(change_id) = txn.get_internal(change).map_err(pristine_error)? else {
                return Ok(MetadataValue::Absent);
            };
            Ok(txn
                .get_change_seq(&view, change_id)
                .map_err(pristine_error)?
                .map(MetadataValue::Sequence)
                .unwrap_or(MetadataValue::Absent))
        }
        MetadataTarget::View { name } => {
            let view = txn
                .get_view(name)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ViewNotFound { name: name.clone() })?;
            encode_view_lease(&view)
        }
        MetadataTarget::Tag { view, name } => txn
            .get_tag(view, name)
            .map_err(pristine_error)?
            .map(|tag| {
                postcard::to_allocvec(&tag)
                    .map(MetadataValue::Bytes)
                    .map_err(|error| RepositoryError::Serialization(error.to_string()))
            })
            .transpose()
            .map(|value| value.unwrap_or(MetadataValue::Absent)),
        MetadataTarget::RefMapping { view } => observe_ref_mapping_value(txn, view),
        MetadataTarget::Capability { id } => observe_capability_value(txn, id),
        target => Err(unsupported_metadata_error(target)),
    }
}

/// Observe the durable required version of one capability (CB-13B).
///
/// `Sequence(version)` when the requirement row exists, `Absent` otherwise.
/// The lease classification compares this against the transition's
/// expected-old so an interrupted cutover is `AlreadyApplied` on resume and
/// any external requirement change is `Diverged`.
fn observe_capability_value<T: atomic_core::pristine::CapabilityTxnT>(
    txn: &T,
    id: &str,
) -> Result<MetadataValue, RepositoryError> {
    Ok(txn
        .required_capability_version(id)
        .map_err(pristine_error)?
        .map(|version| MetadataValue::Sequence(u64::from(version)))
        .unwrap_or(MetadataValue::Absent))
}

/// Observe the current ref-mapping bytes for `view` (CB-10A).
///
/// The mapping is keyed by the durable view id; when the view row is gone
/// (deleted Shared view), the lease falls back to a view-name scan so the
/// stale mapping stays observable and can be marked `Unrepresentable` or
/// removed instead of failing the whole operation.
pub(super) fn observe_ref_mapping_value<T>(
    txn: &T,
    view: &str,
) -> Result<MetadataValue, RepositoryError>
where
    T: atomic_core::pristine::RefMappingTxnT + atomic_core::pristine::ViewTxnT,
{
    if let Some(view_state) = txn.get_view(view).map_err(pristine_error)? {
        return Ok(txn
            .get_ref_mapping_bytes(view_state.id)
            .map_err(pristine_error)?
            .map(MetadataValue::Bytes)
            .unwrap_or(MetadataValue::Absent));
    }
    for (_, bytes) in txn.iter_ref_mapping_bytes().map_err(pristine_error)? {
        if let Ok(mapping) = atomic_core::pristine::RefMapping::decode(&bytes) {
            if mapping.view_name == view {
                return Ok(MetadataValue::Bytes(bytes));
            }
        }
    }
    Ok(MetadataValue::Absent)
}

/// Validate a capability lease's value shape and version support (CB-13B R3).
///
/// Called both at prepare time — so an unsatisfiable capability lease never
/// enters the operation journal — and again in the apply path as defense in
/// depth. A lease is valid when the capability is known to this build and the
/// leased `Sequence` version fits the representable range and does not exceed
/// what this build supports; deletion leases (`Absent`) are only valid for
/// capabilities this build knows, so an unknown requirement row written by a
/// newer build can never be deleted into a bypass here.
fn validate_capability_lease(
    target: &MetadataTarget,
    expected_new: &MetadataValue,
) -> Result<(), RepositoryError> {
    let MetadataTarget::Capability { id } = target else {
        return Ok(());
    };
    match expected_new {
        MetadataValue::Sequence(version) => {
            let supported = SUPPORTED_REPOSITORY_CAPABILITIES
                .iter()
                .find(|capability| capability.id() == id.as_str())
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!(
                        "capability lease requires '{id}', which this build does not support"
                    ),
                })?;
            let requested =
                u32::try_from(*version).map_err(|_| RepositoryError::InvalidOperation {
                    message: format!(
                        "capability lease requires '{id}' version {version}, which exceeds the \
                         representable capability version range"
                    ),
                })?;
            if requested > supported.minimum_version() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "capability lease requires '{id}' version {requested}, but this build \
                         supports only through version {}",
                        supported.minimum_version()
                    ),
                });
            }
            Ok(())
        }
        MetadataValue::Absent => {
            if !SUPPORTED_REPOSITORY_CAPABILITIES
                .iter()
                .any(|capability| capability.id() == id.as_str())
            {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "capability lease deletes '{id}', which this build does not support; \
                         refusing before mutation preserves the requirement row"
                    ),
                });
            }
            Ok(())
        }
        _ => Err(unsupported_metadata_error(target)),
    }
}

fn apply_metadata_value_in_write(
    txn: &mut atomic_core::pristine::WriteTxn<'_>,
    target: &MetadataTarget,
    value: &MetadataValue,
) -> Result<(), RepositoryError> {
    match (target, value) {
        (MetadataTarget::View { name }, MetadataValue::Bytes(_)) => {
            let lease = decode_view_lease(value, name)?;
            let mut view = txn
                .get_view(name)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ViewNotFound { name: name.clone() })?;
            // The parent was already lease-verified at prepare time; sibling
            // transitions in the same operation may legitimately have moved
            // state and count since. The transition itself only moves the
            // parent pointer.
            view.parent = lease.parent;
            txn.update_view(&view).map_err(pristine_error)
        }
        (MetadataTarget::ViewChange { view, change }, MetadataValue::Absent) => {
            let mut view_state = txn
                .get_view(view)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ViewNotFound { name: view.clone() })?;
            let change_id = txn
                .get_internal(change)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: change.to_string(),
                })?;
            txn.del_change(&mut view_state, change_id, change)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ChangeNotInView {
                    hash: change.to_string(),
                    view: view.clone(),
                })?;
            txn.update_view(&view_state).map_err(pristine_error)
        }
        (MetadataTarget::ViewChange { view, change }, MetadataValue::Sequence(sequence)) => {
            let mut view_state = txn
                .get_view(view)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ViewNotFound { name: view.clone() })?;
            let change_id = txn
                .get_internal(change)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ChangeNotFound {
                    hash: change.to_string(),
                })?;
            if *sequence == view_state.change_count {
                txn.put_change(&mut view_state, change_id, change)
                    .map_err(pristine_error)?;
            } else {
                txn.reinsert_change(&mut view_state, change_id, change, *sequence)
                    .map_err(pristine_error)?;
            }
            txn.update_view(&view_state).map_err(pristine_error)
        }
        (MetadataTarget::Tag { view, name }, MetadataValue::Absent) => {
            txn.del_tag(view, name).map_err(pristine_error)?;
            Ok(())
        }
        (MetadataTarget::Tag { view, name }, MetadataValue::Bytes(bytes)) => {
            let tag: atomic_core::pristine::TagRecord = postcard::from_bytes(bytes)
                .map_err(|error| RepositoryError::Serialization(error.to_string()))?;
            if tag.view != *view || tag.name != *name {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "tag metadata value identifies '{}:{}', expected '{view}:{name}'",
                        tag.view, tag.name
                    ),
                });
            }
            txn.put_tag(&tag).map_err(pristine_error)?;
            Ok(())
        }
        (MetadataTarget::RefMapping { view }, MetadataValue::Bytes(bytes)) => {
            let mapping = decode_ref_mapping_value(&MetadataValue::Bytes(bytes.clone()), view)?;
            if mapping.view_name != *view {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "ref-mapping lease names view '{}' but targets '{view}'",
                        mapping.view_name
                    ),
                });
            }
            let view_state = txn
                .get_view(view)
                .map_err(pristine_error)?
                .ok_or_else(|| RepositoryError::ViewNotFound { name: view.clone() })?;
            if mapping.view_id != view_state.id {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "ref-mapping lease for '{view}' carries view id {}, but the durable view id is {}",
                        mapping.view_id, view_state.id
                    ),
                });
            }
            txn.put_ref_mapping_bytes(mapping.view_id, bytes)
                .map_err(pristine_error)
        }
        (MetadataTarget::RefMapping { view }, MetadataValue::Absent) => {
            if let Some(view_state) = txn.get_view(view).map_err(pristine_error)? {
                txn.del_ref_mapping(view_state.id).map_err(pristine_error)?;
                return Ok(());
            }
            // The view is gone: remove any stale rows still naming it.
            let mut stale_ids = Vec::new();
            for (id, bytes) in txn.iter_ref_mapping_bytes().map_err(pristine_error)? {
                if let Ok(mapping) = atomic_core::pristine::RefMapping::decode(&bytes) {
                    if mapping.view_name == *view {
                        stale_ids.push(id);
                    }
                }
            }
            for id in stale_ids {
                txn.del_ref_mapping(id).map_err(pristine_error)?;
            }
            Ok(())
        }
        (MetadataTarget::Capability { id }, MetadataValue::Sequence(version)) => {
            // Unknown capabilities are errors, never silently stored fields
            // (CB-13B constraint), and a requirement above what this build
            // supports cannot be satisfied by this build's apply path.
            validate_capability_lease(
                &MetadataTarget::Capability { id: id.clone() },
                &MetadataValue::Sequence(*version),
            )?;
            // Write exactly the leased version: substituting this build's
            // supported version here wrote a different value than the lease,
            // so replay observed a third value and diverged, and older valid
            // leases were silently raised to the build maximum (CB-13B R3).
            let requested =
                u32::try_from(*version).map_err(|_| RepositoryError::InvalidOperation {
                    message: format!(
                        "capability lease requires '{id}' version {version}, which exceeds the \
                     representable capability version range"
                    ),
                })?;
            txn.put_required_capability_exact(id, requested)
                .map_err(pristine_error)?;
            Ok(())
        }
        (MetadataTarget::Capability { id }, MetadataValue::Absent) => {
            // Fail closed: an unknown requirement row was written by a newer
            // build, and deleting it here would bypass that fence instead of
            // reporting the unsupported rollback (CB-13B constraint).
            validate_capability_lease(
                &MetadataTarget::Capability { id: id.clone() },
                &MetadataValue::Absent,
            )?;
            txn.del_required_capability(id).map_err(pristine_error)?;
            Ok(())
        }
        (target, _) => Err(unsupported_metadata_error(target)),
    }
}

fn metadata_application_key(
    transition: &MetadataTransition,
    observed: &MetadataValue,
) -> (u8, u64) {
    match (&transition.expected_new, observed) {
        (MetadataValue::Absent, MetadataValue::Sequence(sequence)) => (0, u64::MAX - sequence),
        (MetadataValue::Sequence(sequence), _) => (1, *sequence),
        _ => (2, 0),
    }
}

fn metadata_target_view(target: &MetadataTarget) -> Option<&str> {
    match target {
        MetadataTarget::ViewChange { view, .. } => Some(view),
        MetadataTarget::View { name } => Some(name),
        // A ref-mapping row is pure bookkeeping: it never moves view
        // membership, so the tree projection does not need realignment (and
        // the view row may legitimately be gone — deleted view lifecycle).
        MetadataTarget::RefMapping { .. } => None,
        // A capability requirement is repository-scoped; no view tree
        // projection follows it (CB-13B).
        MetadataTarget::Capability { .. } => None,
        MetadataTarget::Tag { .. } => None,
        MetadataTarget::Remote { .. } => None,
    }
}

fn metadata_divergence_error(
    transition: &MetadataTransition,
    observed: &MetadataValue,
) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!(
            "metadata lease {:?} diverged: expected old {:?} or new {:?}, observed {:?}",
            transition.target, transition.expected_old, transition.expected_new, observed
        ),
    }
}

fn unsupported_metadata_error(target: &MetadataTarget) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!("operation metadata target {target:?} is not yet executable"),
    }
}

fn operation_map_reaches(
    start: OperationId,
    target: OperationId,
    operations: &BTreeMap<OperationId, Operation>,
) -> Result<bool, RepositoryError> {
    let mut pending = vec![start];
    let mut visited = BTreeSet::new();
    while let Some(operation_id) = pending.pop() {
        if operation_id == target {
            return Ok(true);
        }
        if !visited.insert(operation_id) {
            continue;
        }
        let operation =
            operations
                .get(&operation_id)
                .ok_or_else(|| RepositoryError::OperationNotFound {
                    selector: operation_id.to_string(),
                })?;
        pending.extend(operation.payload().parents.iter().copied());
    }
    Ok(false)
}

fn apply_snapshot_transition(
    snapshot: &mut BTreeMap<MetadataTarget, MetadataValue>,
    transition: &MetadataTransition,
) {
    if let MetadataTarget::ViewChange { view, .. } = &transition.target {
        if let MetadataValue::Sequence(old_sequence) = transition.expected_old {
            let shifted: Vec<MetadataTarget> = snapshot
                .iter()
                .filter_map(|(target, value)| match (target, value) {
                    (
                        MetadataTarget::ViewChange {
                            view: target_view, ..
                        },
                        MetadataValue::Sequence(sequence),
                    ) if target_view == view && *sequence > old_sequence => Some(target.clone()),
                    _ => None,
                })
                .collect();
            for target in shifted {
                if let Some(MetadataValue::Sequence(sequence)) = snapshot.get_mut(&target) {
                    *sequence -= 1;
                }
            }
            snapshot.remove(&transition.target);
        }
        if let MetadataValue::Sequence(new_sequence) = transition.expected_new {
            let shifted: Vec<MetadataTarget> = snapshot
                .iter()
                .filter_map(|(target, value)| match (target, value) {
                    (
                        MetadataTarget::ViewChange {
                            view: target_view, ..
                        },
                        MetadataValue::Sequence(sequence),
                    ) if target_view == view && *sequence >= new_sequence => Some(target.clone()),
                    _ => None,
                })
                .collect();
            for target in shifted {
                if let Some(MetadataValue::Sequence(sequence)) = snapshot.get_mut(&target) {
                    *sequence += 1;
                }
            }
            snapshot.insert(
                transition.target.clone(),
                MetadataValue::Sequence(new_sequence),
            );
        }
        return;
    }
    if transition.expected_new == MetadataValue::Absent {
        snapshot.remove(&transition.target);
    } else {
        snapshot.insert(transition.target.clone(), transition.expected_new.clone());
    }
}

fn fold_operation_metadata_snapshot(
    operation_id: OperationId,
    operations: &BTreeMap<OperationId, Operation>,
    memo: &mut BTreeMap<OperationId, BTreeMap<MetadataTarget, MetadataValue>>,
    visiting: &mut BTreeSet<OperationId>,
) -> Result<BTreeMap<MetadataTarget, MetadataValue>, RepositoryError> {
    if let Some(snapshot) = memo.get(&operation_id) {
        return Ok(snapshot.clone());
    }
    if !visiting.insert(operation_id) {
        return Err(RepositoryError::InvalidOperation {
            message: format!("operation metadata history contains a cycle at {operation_id}"),
        });
    }
    let operation =
        operations
            .get(&operation_id)
            .ok_or_else(|| RepositoryError::OperationNotFound {
                selector: operation_id.to_string(),
            })?;
    let mut maximal_parents = Vec::new();
    for candidate in &operation.payload().parents {
        let mut dominated = false;
        for other in &operation.payload().parents {
            if other != candidate && operation_map_reaches(*other, *candidate, operations)? {
                dominated = true;
                break;
            }
        }
        if !dominated {
            maximal_parents.push(*candidate);
        }
    }
    let mut snapshot = BTreeMap::new();
    for parent in maximal_parents {
        let parent_snapshot = fold_operation_metadata_snapshot(parent, operations, memo, visiting)?;
        for (target, value) in parent_snapshot {
            if let Some(existing) = snapshot.get(&target) {
                if existing != &value {
                    return Err(RepositoryError::InvalidOperation {
                        message: format!(
                            "operation {operation_id} parents disagree on metadata target {target:?}"
                        ),
                    });
                }
            } else {
                snapshot.insert(target, value);
            }
        }
    }
    let mut transitions: Vec<&MetadataTransition> =
        operation.payload().delta.metadata.iter().collect();
    transitions.sort_by(|left, right| {
        metadata_application_key(left, &left.expected_old)
            .cmp(&metadata_application_key(right, &right.expected_old))
            .then_with(|| left.target.cmp(&right.target))
    });
    for transition in transitions {
        apply_snapshot_transition(&mut snapshot, transition);
    }
    visiting.remove(&operation_id);
    memo.insert(operation_id, snapshot.clone());
    Ok(snapshot)
}

fn operation_reaches(
    txn: &atomic_core::pristine::WriteTxn<'_>,
    start: OperationId,
    target: OperationId,
) -> Result<bool, RepositoryError> {
    let mut pending = vec![start];
    let mut visited = BTreeSet::new();
    while let Some(operation_id) = pending.pop() {
        if operation_id == target {
            return Ok(true);
        }
        if !visited.insert(operation_id) {
            continue;
        }
        let operation = txn
            .get_operation(operation_id)
            .map_err(pristine_error)?
            .ok_or_else(|| RepositoryError::OperationNotFound {
                selector: operation_id.to_string(),
            })?;
        pending.extend(operation.payload().parents.iter().copied());
    }
    Ok(false)
}

fn merge_commuting_operation_states(
    operations: &[Operation],
    scope: OperationScope,
) -> Option<RepoStateRef> {
    if scope == OperationScope::Repository {
        return operations
            .iter()
            .all(|operation| operation.payload().delta.effects.is_empty())
            .then_some(RepoStateRef::EMPTY);
    }
    let first = operations.first()?;
    let common_parents = &first.payload().parents;
    let base = &first.payload().before;
    let mut merged = base.clone();
    let mut view_changed = false;
    let mut working_copy_changed = false;
    let mut git_changed = false;

    for operation in operations {
        if operation.payload().parents != *common_parents || operation.payload().before != *base {
            return None;
        }
        let after = &operation.payload().delta.after;
        if !merge_state_component(&base.view, &after.view, &mut merged.view, &mut view_changed)
            || !merge_state_component(
                &base.working_copy,
                &after.working_copy,
                &mut merged.working_copy,
                &mut working_copy_changed,
            )
            || !merge_state_component(&base.git, &after.git, &mut merged.git, &mut git_changed)
        {
            return None;
        }
    }
    Some(merged)
}

fn merge_state_component<T: Clone + PartialEq>(
    base: &Option<T>,
    after: &Option<T>,
    merged: &mut Option<T>,
    changed: &mut bool,
) -> bool {
    if after == base {
        return true;
    }
    if *changed {
        return false;
    }
    *merged = after.clone();
    *changed = true;
    true
}

fn operation_write_sets_are_disjoint(operations: &[Operation]) -> bool {
    let mut metadata_targets = BTreeSet::<MetadataTarget>::new();
    let mut effect_targets = Vec::<EffectTarget>::new();
    for operation in operations {
        for transition in &operation.payload().delta.metadata {
            if !metadata_targets.insert(transition.target.clone()) {
                return false;
            }
        }
        for effect in &operation.payload().delta.effects {
            if effect_targets.contains(&effect.target) {
                return false;
            }
            effect_targets.push(effect.target.clone());
        }
    }
    true
}

pub(super) fn current_operation_timestamp_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn operation_not_reversible(operation: &Operation, reason: &str) -> RepositoryError {
    RepositoryError::OperationNotReversible {
        operation: operation.id().to_string(),
        kind: format!("{:?}", operation.payload().kind),
        reason: reason.to_string(),
    }
}

fn operation_verification_state(receipts: &[EffectReceipt]) -> OperationVerificationState {
    if has_operation_verified_receipt(receipts) {
        return OperationVerificationState::Verified;
    }
    if receipts
        .iter()
        .any(|receipt| receipt.payload().kind == EffectReceiptKind::LeaseRejected)
    {
        return OperationVerificationState::LeaseRejected;
    }
    if receipts.iter().any(|receipt| {
        matches!(
            receipt.payload().kind,
            EffectReceiptKind::Applied
                | EffectReceiptKind::RolledBack
                | EffectReceiptKind::Recovered
        )
    }) {
        OperationVerificationState::InProgress
    } else {
        OperationVerificationState::Prepared
    }
}

fn sort_receipts(receipts: &mut [EffectReceipt]) {
    receipts.sort_by_key(|receipt| {
        let payload = receipt.payload();
        (
            payload.effect_ordinal.unwrap_or(u32::MAX),
            payload.attempt,
            receipt_kind_order(payload.kind),
            receipt.id(),
        )
    });
}

fn receipt_kind_order(kind: EffectReceiptKind) -> u8 {
    match kind {
        EffectReceiptKind::Applied => 0,
        EffectReceiptKind::Recovered => 1,
        EffectReceiptKind::RolledBack => 2,
        EffectReceiptKind::LeaseRejected => 3,
        EffectReceiptKind::Verified => 4,
    }
}

fn sole_recovery_parent(recovery: &Operation) -> Result<OperationId, RepositoryError> {
    match recovery.payload().parents.as_slice() {
        [parent] => Ok(*parent),
        parents => Err(RepositoryError::InvalidOperation {
            message: format!(
                "Recover operation {} must have exactly one parent for CB-1B, found {}",
                recovery.id(),
                parents.len()
            ),
        }),
    }
}

fn validate_effect_target_chains(operation: &Operation) -> Result<(), RepositoryError> {
    let mut states: Vec<(&EffectTarget, &EffectValue)> = Vec::new();
    for effect in &operation.payload().delta.effects {
        if let Some((_, previous_new)) = states
            .iter_mut()
            .find(|(target, _)| **target == effect.target)
        {
            if **previous_new != effect.expected_old {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "switch operation {} repeats effect target at ordinal {} without chaining expected-old from the previous expected-new value",
                        operation.id(), effect.ordinal
                    ),
                });
            }
            *previous_new = &effect.expected_new;
        } else {
            states.push((&effect.target, &effect.expected_new));
        }
    }
    Ok(())
}

fn effect_at(operation: &Operation, ordinal: u32) -> Result<&EffectPlan, RepositoryError> {
    operation
        .payload()
        .delta
        .effects
        .get(ordinal as usize)
        .filter(|effect| effect.ordinal == ordinal)
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "operation {} has no effect ordinal {ordinal}",
                operation.id()
            ),
        })
}

pub(super) fn working_copy_state_ref(record: WorkingCopyRecord) -> WorkingCopyStateRef {
    WorkingCopyStateRef {
        id: record.id,
        location_fingerprint: record.location_fingerprint,
        desired_view: record.desired_view,
        desired_state: record.desired_state,
        materialized_state: record.materialized_state,
        materialized_manifest: record.materialized_manifest,
    }
}

fn validate_state_working_copy(
    state: &RepoStateRef,
    working_copy: WorkingCopyId,
) -> Result<(), RepositoryError> {
    match &state.working_copy {
        Some(value) if value.id == working_copy => Ok(()),
        Some(value) => Err(RepositoryError::InvalidOperation {
            message: format!(
                "operation state belongs to working copy {}, lock is for {working_copy}",
                value.id
            ),
        }),
        None => Err(RepositoryError::InvalidOperation {
            message: format!(
                "working-copy operation for {working_copy} requires working-copy state"
            ),
        }),
    }
}

fn multiple_heads_error(scope: OperationScope, heads: &[OperationId]) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!(
            "{scope} has {} operation heads; CB-1B refuses consolidation until CB-1C: {}",
            heads.len(),
            heads
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn lease_divergence_error(
    effect: &EffectPlan,
    observed_before: &EffectValue,
    observed_after: Option<&EffectValue>,
) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: match observed_after {
            Some(after) => format!(
                "effect {} diverged: expected old {:?} and new {:?}, observed before {:?} and after {:?}",
                effect.ordinal,
                effect.expected_old,
                effect.expected_new,
                observed_before,
                after
            ),
            None => format!(
                "effect {} diverged: expected old {:?} or new {:?}, observed {:?}",
                effect.ordinal, effect.expected_old, effect.expected_new, observed_before
            ),
        },
    }
}

pub(super) fn git_object_hex(object: &GitObjectId) -> Result<String, RepositoryError> {
    let hex: String = object
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(hex)
}

pub(super) fn git_oid_from_object(
    repo: &git2::Repository,
    object: &GitObjectId,
) -> Result<git2::Oid, RepositoryError> {
    let hex = git_object_hex(object)?;
    let oid = git2::Oid::from_str(&hex).map_err(|error| RepositoryError::InvalidRepository {
        reason: format!("cannot parse Git object id '{hex}': {error}"),
    })?;
    let expected_width = match object.algorithm() {
        GitHashAlgorithm::Sha1 => 20,
        GitHashAlgorithm::Sha256 => 32,
    };
    if oid.as_bytes().len() != expected_width {
        return Err(RepositoryError::InvalidRepository {
            reason: format!(
                "Git object id '{hex}' width {} disagrees with the repository format",
                oid.as_bytes().len()
            ),
        });
    }
    let _ = repo;
    Ok(oid)
}

fn unsupported_effect_error(target: &EffectTarget) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!(
            "CB-1B repository recovery cannot execute effect target {target:?}; parent integration must provide its lease-safe executor"
        ),
    }
}

pub(super) fn codec_error(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::InvalidOperation {
        message: format!("invalid operation journal object: {error}"),
    }
}

pub(super) fn pristine_error(error: impl std::fmt::Display) -> RepositoryError {
    RepositoryError::Database(error.to_string())
}

fn is_filesystem_effect(target: &EffectTarget) -> bool {
    matches!(
        target,
        EffectTarget::FilesystemPath { .. }
            | EffectTarget::WorkspacePath { .. }
            | EffectTarget::ShelfPath { .. }
    )
}

fn is_recursive_filesystem_target(target: &EffectTarget) -> bool {
    matches!(
        target,
        EffectTarget::WorkspacePath { .. } | EffectTarget::ShelfPath { .. }
    )
}

pub(super) fn backup_entry_path(root: &Path, ordinal: u32) -> PathBuf {
    root.join(BACKUP_ENTRIES_DIR).join(format!("{ordinal:010}"))
}

/// Whether `path` is exactly `.git/hooks/<name>` — the single `.git`-rooted
/// surface a journaled filesystem effect may target (CB-13B R2 hook
/// decommission lease). One normal hook-file component; no dotfiles, no
/// nesting, no repeats.
fn is_git_hooks_effect_path(path: &str) -> bool {
    let mut components = Path::new(path).components();
    let git = matches!(
        components.next(),
        Some(Component::Normal(value)) if value == OsStr::new(".git")
    );
    let hooks = matches!(
        components.next(),
        Some(Component::Normal(value)) if value == OsStr::new("hooks")
    );
    let hook_file = matches!(
        components.next(),
        Some(Component::Normal(value))
            if !value.to_string_lossy().starts_with('.')
                && value != OsStr::new("hooks")
                && value != OsStr::new(".git")
    );
    git && hooks && hook_file && components.next().is_none()
}

fn validate_relative_path(path: &str, allow_vcs_names: bool) -> Result<PathBuf, RepositoryError> {
    if path.is_empty() {
        return Err(RepositoryError::InvalidOperation {
            message: "operation effect path cannot be empty".to_string(),
        });
    }
    let candidate = Path::new(path);
    let mut clean = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(value) => clean.push(value),
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!("unsafe operation effect path '{path}'"),
                })
            }
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(RepositoryError::InvalidOperation {
            message: format!("unsafe operation effect path '{path}'"),
        });
    }
    if !allow_vcs_names {
        let first = clean
            .components()
            .next()
            .and_then(|component| match component {
                Component::Normal(value) => Some(value),
                _ => None,
            });
        if first == Some(OsStr::new(".atomic")) || first == Some(OsStr::new(".git")) {
            return Err(RepositoryError::InvalidOperation {
                message: format!("operation filesystem effect cannot target '{path}'"),
            });
        }
    }
    Ok(clean)
}

fn resolve_without_symlink_parents(
    base: &Path,
    relative: &Path,
) -> Result<PathBuf, RepositoryError> {
    let mut resolved = base.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(value) = component else {
            return Err(RepositoryError::InvalidOperation {
                message: format!("unsafe operation path '{}'", relative.display()),
            });
        };
        resolved.push(value);
        if index + 1 == components.len() {
            continue;
        }
        match fs::symlink_metadata(&resolved) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "operation path '{}' traverses symlink parent '{}'",
                        relative.display(),
                        resolved.display()
                    ),
                })
            }
            Ok(_) => {}
            Err(error) if is_absent_path_error(&error) => {}
            Err(error) => return Err(RepositoryError::Io(error)),
        }
    }
    Ok(resolved)
}

fn is_absent_path_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn observe_path(path: &Path, recursive_directory: bool) -> Result<EffectValue, RepositoryError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if is_absent_path_error(&error) => {
            return Ok(EffectValue::Absent);
        }
        Err(error) => return Err(RepositoryError::Io(error)),
    };
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        FileKind::Symlink
    } else if file_type.is_file() {
        FileKind::Regular
    } else if file_type.is_dir() && path.join(".git").is_file() && !recursive_directory {
        FileKind::Gitlink
    } else if file_type.is_dir() {
        FileKind::Directory
    } else {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "operation cannot classify special filesystem entry '{}'",
                path.display()
            ),
        });
    };
    let content = match kind {
        FileKind::Regular => Hash::of(&fs::read(path)?),
        FileKind::Symlink => Hash::of(&os_str_bytes(fs::read_link(path)?.as_os_str())),
        FileKind::Directory if recursive_directory => directory_content_hash(path)?,
        FileKind::Directory => Hash::of(b"atomic:filesystem-directory-entry:v1\0"),
        FileKind::Gitlink => Hash::of(&fs::read(path.join(".git"))?),
    };
    Ok(EffectValue::File(FileState {
        kind,
        mode: metadata_mode(&metadata),
        content,
    }))
}

fn directory_content_hash(path: &Path) -> Result<Hash, RepositoryError> {
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| {
        os_str_bytes(left.file_name().as_os_str()).cmp(&os_str_bytes(right.file_name().as_os_str()))
    });
    let mut canonical = b"atomic:operation-directory:v1\0".to_vec();
    for entry in entries {
        let name = os_str_bytes(entry.file_name().as_os_str());
        let value = observe_path(&entry.path(), true)?;
        let EffectValue::File(state) = value else {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "directory entry '{}' disappeared while hashing",
                    entry.path().display()
                ),
            });
        };
        canonical.extend_from_slice(&(name.len() as u64).to_le_bytes());
        canonical.extend_from_slice(&name);
        canonical.push(match state.kind {
            FileKind::Regular => 1,
            FileKind::Directory => 2,
            FileKind::Symlink => 3,
            FileKind::Gitlink => 4,
        });
        canonical.extend_from_slice(&state.mode.to_le_bytes());
        canonical.extend_from_slice(state.content.as_bytes());
    }
    Ok(Hash::of(&canonical))
}

fn copy_entry(source: &Path, destination: &Path) -> Result<(), RepositoryError> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        create_symlink(&fs::read_link(source)?, destination, source)?;
    } else if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut input = File::open(source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        std::io::copy(&mut input, &mut output)?;
        set_mode(destination, metadata_mode(&metadata))?;
        output.sync_all()?;
    } else if metadata.is_dir() {
        fs::create_dir(destination)?;
        let mut entries = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by(|left, right| {
            os_str_bytes(left.file_name().as_os_str())
                .cmp(&os_str_bytes(right.file_name().as_os_str()))
        });
        for entry in entries {
            copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
        }
        set_mode(destination, metadata_mode(&metadata))?;
        sync_directory(destination)?;
    } else {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "operation cannot back up special filesystem entry '{}'",
                source.display()
            ),
        });
    }
    Ok(())
}

pub(super) fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), RepositoryError> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_atomic_regular(path: &Path, bytes: &[u8], mode: u32) -> Result<(), RepositoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "cannot atomically replace path without parent: {}",
                path.display()
            ),
        })?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.as_file_mut().write_all(bytes)?;
    set_mode(temporary.path(), mode)?;
    temporary.as_file_mut().sync_all()?;
    if matches!(fs::symlink_metadata(path), Ok(metadata) if metadata.is_dir())
        && !remove_materialized_gitlink(path)?
    {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "refusing to replace directory '{}' with a regular file",
                path.display()
            ),
        });
    }
    temporary
        .persist(path)
        .map_err(|error| RepositoryError::Io(error.error))?;
    sync_directory(parent)
}

fn write_atomic_symlink(path: &Path, target: &[u8]) -> Result<(), RepositoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("cannot replace symlink without parent: {}", path.display()),
        })?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".atomic-link-")
        .tempdir_in(parent)?;
    // The staged path is referenced by both cfg arms (symlink on unix,
    // unused-variable suppression on windows), so it must be unconditional.
    let prepared = staging.path().join(BACKUP_VALUE);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::os::unix::fs::symlink(std::ffi::OsString::from_vec(target.to_vec()), &prepared)?;
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        let _ = staging;
        // Unreachable on this platform, but the compiler cannot see through
        // the cfg gate — reference the locals so -D warnings is satisfied.
        let _ = &prepared;
        return Err(RepositoryError::InvalidOperation {
            message: "symlink effects are unsupported on this platform".to_string(),
        });
    }
    #[cfg(unix)]
    {
        remove_filesystem_effect_path(path)?;
        fs::rename(&prepared, path)?;
        sync_directory(parent)
    }
}

fn write_atomic_gitlink(path: &Path, object_id: &[u8], mode: u32) -> Result<(), RepositoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("cannot replace gitlink without parent: {}", path.display()),
        })?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".atomic-gitlink-")
        .tempdir_in(parent)?;
    let prepared = staging.path().join(BACKUP_VALUE);
    fs::create_dir(&prepared)?;
    write_new_synced(&prepared.join(".git"), object_id)?;
    set_mode(&prepared, mode)?;
    remove_filesystem_effect_path(path)?;
    fs::rename(&prepared, path)?;
    sync_directory(parent)
}

fn write_atomic_regular_from_file(
    path: &Path,
    source: &Path,
    mode: u32,
) -> Result<(), RepositoryError> {
    let mut input = File::open(source)?;
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    write_atomic_regular(path, &bytes, mode)
}

fn replace_directory(path: &Path, source: &Path, mode: u32) -> Result<(), RepositoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "cannot replace directory without parent: {}",
                path.display()
            ),
        })?;
    fs::create_dir_all(parent)?;
    if fs::symlink_metadata(path).is_ok() {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "directory recovery target must be absent before replacement: {}",
                path.display()
            ),
        });
    }
    let staging = tempfile::Builder::new()
        .prefix(".atomic-recover-dir-")
        .tempdir_in(parent)?;
    let prepared = staging.path().join(BACKUP_VALUE);
    copy_entry(source, &prepared)?;
    set_mode(&prepared, mode)?;
    fs::rename(&prepared, path)?;
    sync_directory(parent)
}

fn replace_symlink(path: &Path, source: &Path) -> Result<(), RepositoryError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("cannot replace symlink without parent: {}", path.display()),
        })?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".atomic-recover-link-")
        .tempdir_in(parent)?;
    let prepared = staging.path().join(BACKUP_VALUE);
    let target = fs::read_link(source)?;
    create_symlink(&target, &prepared, source)?;
    fs::rename(&prepared, path)?;
    sync_directory(parent)
}

fn create_directory_effect_path(
    path: &Path,
    mode: u32,
    staging: &Path,
) -> Result<(), RepositoryError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "refusing to replace existing entry '{}' with a directory",
                    path.display()
                ),
            })
        }
        Err(error) => return Err(RepositoryError::Io(error)),
    }
    let parent = path
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!("directory effect has no parent: {}", path.display()),
        })?;
    if !parent.is_dir() {
        return Err(RepositoryError::InvalidOperation {
            message: format!(
                "directory effect parent '{}' is not materialized",
                parent.display()
            ),
        });
    }
    if staging.exists() {
        fs::remove_dir_all(staging)?;
    }
    let staging_parent = staging
        .parent()
        .ok_or_else(|| RepositoryError::InvalidOperation {
            message: format!(
                "directory staging path has no parent: {}",
                staging.display()
            ),
        })?;
    fs::create_dir_all(staging_parent)?;
    fs::create_dir(staging)?;
    set_mode(staging, mode)?;
    sync_directory(staging)?;
    sync_directory(staging_parent)?;
    fs::rename(staging, path)?;
    sync_directory(parent)
}

fn remove_materialized_gitlink(path: &Path) -> Result<bool, RepositoryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => return Ok(false),
    };
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    if entries.len() != 1 || entries[0].file_name() != ".git" || !entries[0].file_type()?.is_file()
    {
        return Ok(false);
    }
    let git_file = entries.pop().expect("one checked gitlink entry").path();
    fs::remove_file(git_file)?;
    fs::remove_dir(path)?;
    Ok(true)
}

fn remove_filesystem_effect_path(path: &Path) -> Result<(), RepositoryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            if remove_materialized_gitlink(path)? {
                if let Some(parent) = path.parent() {
                    sync_directory(parent)?;
                }
                return Ok(());
            }
            fs::remove_dir(path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::DirectoryNotEmpty {
                    RepositoryError::InvalidOperation {
                        message: format!(
                            "cannot remove directory '{}': untracked children are never removed recursively",
                            path.display()
                        ),
                    }
                } else {
                    RepositoryError::Io(error)
                }
            })?
        }
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(RepositoryError::Io(error)),
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn remove_entry_for_recovery(path: &Path, tombstone: &Path) -> Result<(), RepositoryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            let parent = tombstone
                .parent()
                .ok_or_else(|| RepositoryError::InvalidOperation {
                    message: format!("recovery tombstone has no parent: {}", tombstone.display()),
                })?;
            fs::create_dir_all(parent)?;
            if tombstone.exists() {
                return Err(RepositoryError::InvalidOperation {
                    message: format!(
                        "recovery tombstone already exists while target is present: {}",
                        tombstone.display()
                    ),
                });
            }
            fs::rename(path, tombstone)?;
            if let Some(source_parent) = path.parent() {
                sync_directory(source_parent)?;
            }
            sync_directory(parent)?;
        }
        Ok(_) => {
            fs::remove_file(path)?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(RepositoryError::Io(error)),
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(
    target: &Path,
    destination: &Path,
    _source: &Path,
) -> Result<(), RepositoryError> {
    std::os::unix::fs::symlink(target, destination).map_err(RepositoryError::Io)
}

#[cfg(windows)]
fn create_symlink(target: &Path, destination: &Path, source: &Path) -> Result<(), RepositoryError> {
    let target_is_dir = fs::metadata(source)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false);
    if target_is_dir {
        std::os::windows::fs::symlink_dir(target, destination).map_err(RepositoryError::Io)
    } else {
        std::os::windows::fs::symlink_file(target, destination).map_err(RepositoryError::Io)
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(
    _target: &Path,
    destination: &Path,
    _source: &Path,
) -> Result<(), RepositoryError> {
    Err(RepositoryError::InvalidOperation {
        message: format!(
            "symbolic-link recovery is unsupported on this platform: {}",
            destination.display()
        ),
    })
}

#[cfg(unix)]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), RepositoryError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(RepositoryError::Io)
}

#[cfg(not(unix))]
fn set_mode(path: &Path, mode: u32) -> Result<(), RepositoryError> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(mode & 0o200 == 0);
    fs::set_permissions(path, permissions).map_err(RepositoryError::Io)
}

#[cfg(unix)]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(windows)]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(not(any(unix, windows)))]
fn os_str_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

fn sync_directory(path: &Path) -> Result<(), RepositoryError> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

//! Journaled, lease-validated reconciliation of one physical working copy's
//! persistent registration with an existing view.
//!
//! A working-copy record stores the *desired* view and the exact state of that
//! view the working copy was registered against, plus an optional *verified
//! materialized* claim. If a view advances after the registration is written
//! (or a legacy compatibility file and the persistent record disagree), an
//! ordinary writable open refuses the whole workspace with
//! [`RepositoryError::InvalidRepository`] before any command runs.
//!
//! This module implements the supported, journaled way out of that state. It:
//!
//! 1. diagnoses the mismatch read-only ([`Repository::inspect_working_copy_registration`]);
//! 2. acquires the ordered common → working-copy operation locks;
//! 3. re-reads the full expected-old registration and the target view state
//!    under those locks and refuses on a third value, a location mismatch, a
//!    missing target, or incomplete/ambiguous operation heads;
//! 4. journals an immutable metadata-only [`OperationKind::ReconcileWorkingCopy`]
//!    operation carrying the typed before/after working-copy state and the
//!    target view state, applies the working-copy record transition, records
//!    the effect receipt, and finalizes the operation verified.
//!
//! It never writes working-tree bytes, moves Git refs/indexes, touches shelves,
//! or changes view membership: a stale `materialized_state`/`materialized_manifest`
//! is cleared (desired ≠ verified) rather than presumed good. The derived
//! `.atomic/current_view` compatibility pointer is only rewritten when it is
//! missing or still exactly the previously registered view — a third value is
//! preserved and refused, never clobbered.

use super::operation::{
    current_operation_timestamp_ms, working_copy_state_ref, PreparedSwitchOperation,
};
use super::*;
use atomic_core::operation::{
    ActorRef, EffectPlan, EffectTarget, EffectValue, OperationKind, RepoStateRef, ViewStateRef,
    WorkingCopyStateRef,
};
use atomic_core::pristine::{ViewState, WorkingCopyRecord};
use atomic_core::OperationId;

const RECONCILE_ACTOR: &str = "working-copy-registration-reconcile";
const CURRENT_VIEW_FILE: &str = "current_view";

/// Read-only diagnosis of a working copy's registration against one target view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingCopyRegistrationDiagnosis {
    /// Stable identity of the physical working copy.
    pub working_copy: WorkingCopyId,
    /// Location fingerprint stored in the persistent record.
    pub location_fingerprint: Hash,
    /// Whether the stored fingerprint matches this handle's canonical location.
    pub location_matches: bool,
    /// Repository-local ID of the view the record currently desires.
    pub desired_view_id: u64,
    /// Name of the desired view, when it still exists.
    pub desired_view_name: Option<String>,
    /// State recorded for the desired view.
    pub desired_state: Merkle,
    /// Current state of the desired view, when it still exists.
    pub observed_desired_view_state: Option<Merkle>,
    /// Verified materialized state recorded for the working copy.
    pub materialized_state: Option<Merkle>,
    /// Verified materialized manifest recorded for the working copy.
    pub materialized_manifest: Option<Hash>,
    /// Repository-local ID of the requested target view.
    pub target_view_id: u64,
    /// Name of the requested target view.
    pub target_view_name: String,
    /// Current state of the requested target view.
    pub target_view_state: Merkle,
    /// Whether the desired view's state has advanced past `desired_state`.
    pub desired_view_is_stale: bool,
    /// Whether the record already exactly matches the target view/materialized
    /// baseline (so reconciliation would be a no-op).
    pub already_reconciled: bool,
}

impl WorkingCopyRegistrationDiagnosis {
    /// Whether this working copy needs a registration transition to `target`.
    pub fn needs_reconciliation(&self) -> bool {
        !self.already_reconciled
    }
}

/// Result of a working-copy registration reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkingCopyReconcileOutcome {
    /// The registration already matched the target; nothing was written.
    AlreadyReconciled {
        /// Diagnosis captured at the decision point.
        diagnosis: WorkingCopyRegistrationDiagnosis,
    },
    /// A journaled metadata operation rebinding the registration was verified.
    Reconciled {
        /// Immutable operation identity of the reconciliation.
        operation: OperationId,
        /// View the record desired before the transition.
        previous_view: String,
        /// View the record now desires.
        target_view: String,
    },
}

impl WorkingCopyReconcileOutcome {
    /// The immutable operation identity, when a mutation was performed.
    pub fn operation(&self) -> Option<OperationId> {
        match self {
            Self::AlreadyReconciled { .. } => None,
            Self::Reconciled { operation, .. } => Some(*operation),
        }
    }
}

impl Repository {
    /// Diagnose a working copy's registration against `target_view` read-only.
    ///
    /// This performs no recovery, migration, or mutation: it reads the
    /// persistent record, the desired view, and the requested target view. Use
    /// it for `--dry-run` and for preflight display.
    pub fn inspect_working_copy_registration(
        &self,
        working_copy: WorkingCopyId,
        target_view: &str,
    ) -> Result<WorkingCopyRegistrationDiagnosis, RepositoryError> {
        let location_fingerprint = self.registration_location_fingerprint()?;
        let txn = self
            .pristine
            .read_txn()
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let record = txn
            .get_working_copy(working_copy)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?;
        let desired_view = txn
            .get_view_by_id(record.desired_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?;
        let target = txn
            .get_view(target_view)
            .map_err(|error| RepositoryError::Database(error.to_string()))?
            .ok_or_else(|| RepositoryError::ViewNotFound {
                name: target_view.to_string(),
            })?;

        Ok(Self::registration_diagnosis(
            &record,
            desired_view.as_ref(),
            &target,
            location_fingerprint,
        ))
    }

    /// Reconcile a working copy's registration with an existing `target_view`
    /// through a journaled, leased, metadata-only operation.
    ///
    /// # Errors
    ///
    /// Refuses before any mutation when the working copy is unknown, bound to a
    /// different canonical location, its desired view is missing, the target
    /// view is missing, or the working-copy/repository operation heads are
    /// incomplete or ambiguous. A concurrent change to the record or the target
    /// view state surfaces as a typed lease/divergence refusal.
    pub fn reconcile_working_copy_registration(
        &mut self,
        working_copy: WorkingCopyId,
        target_view: &str,
    ) -> Result<WorkingCopyReconcileOutcome, RepositoryError> {
        // Ordered locks: repository-common → per-working-copy operation lock.
        // The pristine write transaction is only opened inside the prepare step.
        let operation_lock = self.try_lock_operation(working_copy)?;

        // Refuse incomplete or ambiguous operation heads before any mutation.
        if self.repository_operation_requires_recovery()? {
            return Err(RepositoryError::InvalidOperation {
                message: "repository operation head is incomplete; recover it before \
                          reconciling a working-copy registration"
                    .to_string(),
            });
        }
        if self.working_copy_operation_requires_recovery(working_copy)? {
            return Err(RepositoryError::InvalidOperation {
                message: format!(
                    "working-copy {working_copy} operation head is incomplete; recover it \
                     before reconciling its registration"
                ),
            });
        }

        let location_fingerprint = self.registration_location_fingerprint()?;
        let (record, desired_view, target) = {
            let txn = self
                .pristine
                .read_txn()
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let record = txn
                .get_working_copy(working_copy)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or(RepositoryError::WorkingCopyRecordNotFound { id: working_copy })?;
            let desired_view = txn
                .get_view_by_id(record.desired_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?;
            let target = txn
                .get_view(target_view)
                .map_err(|error| RepositoryError::Database(error.to_string()))?
                .ok_or_else(|| RepositoryError::ViewNotFound {
                    name: target_view.to_string(),
                })?;
            (record, desired_view, target)
        };

        if record.location_fingerprint != location_fingerprint {
            return Err(RepositoryError::WorkingCopyLocationMismatch { id: working_copy });
        }
        let desired_view = desired_view.ok_or_else(|| RepositoryError::InvalidRepository {
            reason: format!(
                "working-copy record {working_copy} references missing desired view {}",
                record.desired_view
            ),
        })?;

        let diagnosis = Self::registration_diagnosis(
            &record,
            Some(&desired_view),
            &target,
            location_fingerprint,
        );

        let before_ref = working_copy_state_ref(record.clone());
        let mut after_ref = before_ref.clone();
        after_ref.desired_view = target.id;
        after_ref.desired_state = target.state;
        after_ref.materialized_state = None;
        after_ref.materialized_manifest = None;

        if before_ref == after_ref {
            return Ok(WorkingCopyReconcileOutcome::AlreadyReconciled { diagnosis });
        }

        let prepared = self.prepare_registration_transition(
            &operation_lock,
            working_copy,
            &desired_view,
            &target,
            &before_ref,
            &after_ref,
        )?;
        let operation_id = prepared.operation().id();

        let effect = Self::registration_effect(working_copy, &before_ref, &after_ref);
        let observed_before = self.observe_operation_effect(working_copy, &effect.target)?;
        // The journaled metadata transition is the only mutation of this path:
        // it writes the working-copy record and never touches TREE, files, shelves,
        // refs, or view membership.
        self.apply_operation_metadata_locked(&operation_lock, operation_id)?;
        let observed_after = self.observe_operation_effect(working_copy, &effect.target)?;
        self.record_effect_outcome(
            &operation_lock,
            operation_id,
            effect.ordinal,
            observed_before,
            observed_after,
        )?;
        self.finalize_operation_verified(&operation_lock, operation_id)?;

        // Successor state is authoritative in the record; the compatibility file
        // is a derived artifact and must never overwrite a third writer's value.
        self.reconcile_compatibility_pointer(&target.name, &desired_view.name)?;
        self.current_view = target.name.clone();

        Ok(WorkingCopyReconcileOutcome::Reconciled {
            operation: operation_id,
            previous_view: desired_view.name,
            target_view: target.name,
        })
    }

    fn registration_location_fingerprint(&self) -> Result<Hash, RepositoryError> {
        let layout = working_copy::layout_for_paths(
            self.root.clone(),
            self.dot_dir.clone(),
            self.working_copy_dot_dir(),
            false,
        )?;
        Ok(layout.location_fingerprint)
    }

    fn registration_diagnosis(
        record: &WorkingCopyRecord,
        desired_view: Option<&ViewState>,
        target: &ViewState,
        location_fingerprint: Hash,
    ) -> WorkingCopyRegistrationDiagnosis {
        let desired_view_is_stale = desired_view
            .map(|view| view.state != record.desired_state)
            .unwrap_or(true);
        let already_reconciled = record.desired_view == target.id
            && record.desired_state == target.state
            && record.materialized_state.is_none()
            && record.materialized_manifest.is_none();
        WorkingCopyRegistrationDiagnosis {
            working_copy: record.id,
            location_fingerprint: record.location_fingerprint,
            location_matches: record.location_fingerprint == location_fingerprint,
            desired_view_id: record.desired_view,
            desired_view_name: desired_view.map(|view| view.name.clone()),
            desired_state: record.desired_state,
            observed_desired_view_state: desired_view.map(|view| view.state),
            materialized_state: record.materialized_state,
            materialized_manifest: record.materialized_manifest,
            target_view_id: target.id,
            target_view_name: target.name.clone(),
            target_view_state: target.state,
            desired_view_is_stale,
            already_reconciled,
        }
    }

    fn registration_effect(
        working_copy: WorkingCopyId,
        before: &WorkingCopyStateRef,
        after: &WorkingCopyStateRef,
    ) -> EffectPlan {
        EffectPlan {
            ordinal: 0,
            target: EffectTarget::WorkingCopy { working_copy },
            expected_old: EffectValue::WorkingCopy(before.clone()),
            expected_new: EffectValue::WorkingCopy(after.clone()),
        }
    }

    fn prepare_registration_transition(
        &self,
        operation_lock: &super::locks::WorkingCopyOperationLockGuard,
        working_copy: WorkingCopyId,
        previous_view: &ViewState,
        target: &ViewState,
        before_ref: &WorkingCopyStateRef,
        after_ref: &WorkingCopyStateRef,
    ) -> Result<PreparedSwitchOperation, RepositoryError> {
        let before_state = RepoStateRef {
            view: Some(ViewStateRef {
                name: previous_view.name.clone(),
                state: previous_view.state,
                set_id: None,
            }),
            working_copy: Some(before_ref.clone()),
            git: None,
        };
        let after_state = RepoStateRef {
            view: Some(ViewStateRef {
                name: target.name.clone(),
                state: target.state,
                set_id: None,
            }),
            working_copy: Some(after_ref.clone()),
            git: None,
        };
        let effect = Self::registration_effect(working_copy, before_ref, after_ref);
        self.prepare_working_copy_transition(
            operation_lock,
            OperationKind::ReconcileWorkingCopy,
            None,
            before_state,
            after_state,
            vec![effect],
            Vec::new(),
            ActorRef::System {
                name: RECONCILE_ACTOR.to_string(),
            },
            current_operation_timestamp_ms(),
        )
    }

    /// Rewrite the derived `.atomic/current_view` file only when it is missing
    /// or still exactly the previously registered view. Any other observed value
    /// is a third writer's state and refuses the transition.
    fn reconcile_compatibility_pointer(
        &self,
        target_view: &str,
        previous_view: &str,
    ) -> Result<(), RepositoryError> {
        let path = self.working_copy_dot_dir().join(CURRENT_VIEW_FILE);
        let observed = match std::fs::read_to_string(&path) {
            Ok(content) => Some(content.trim().to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(RepositoryError::Io(error)),
        };
        match observed.as_deref() {
            Some(value) if value == target_view => Ok(()),
            Some(value) if value == previous_view => self.write_current_view(target_view),
            None => self.write_current_view(target_view),
            Some(other) => Err(RepositoryError::InvalidRepository {
                reason: format!(
                    "refusing to overwrite compatibility pointer '{other}' \
                 (expected '{previous_view}' or '{target_view}')"
                ),
            }),
        }
    }
}

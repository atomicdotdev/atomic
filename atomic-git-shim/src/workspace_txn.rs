//! Shared CB-5B/CB-5C entry adapter for working-copy command boundaries.

use atomic_repository::{
    ReconcileEffectBudget, Repository, WorkspaceRemediation, WorkspaceTxn, WorkspaceTxnMode,
    WorkspaceTxnStart,
};

use crate::error::{ShimError, ShimResult};

/// Select the explicit workspace transaction mode for a network boundary.
///
/// Dry-run/reporting boundaries observe without mutation; every other
/// selection reconciles safe drift and refuses unsafe states.
pub(crate) fn boundary_mode(dry_run: bool) -> WorkspaceTxnMode {
    if dry_run {
        WorkspaceTxnMode::Observe
    } else {
        WorkspaceTxnMode::Reconcile
    }
}

/// Enter a stable workspace transaction or convert remediation into one CLI refusal.
pub(crate) fn enter_workspace(
    repository: &mut Repository,
    mode: WorkspaceTxnMode,
) -> ShimResult<WorkspaceTxn> {
    match repository
        .begin_workspace_txn(mode)
        .map_err(ShimError::Repository)?
    {
        WorkspaceTxnStart::Ready(transaction) => Ok(transaction),
        WorkspaceTxnStart::Remediation(remediation) => Err(remediation_error(remediation)),
    }
}

/// Enter the explicit repair boundary for the shim remediation path.
///
/// The shim remediation command is what reconciles unanchored workspaces, so
/// it cannot require an anchored baseline; it still refuses Git-owned
/// operations, index locks, and diverged operation heads through the typed
/// remediation value.
pub(crate) fn enter_remediation_workspace(
    repository: &mut Repository,
) -> ShimResult<Result<WorkspaceTxn, WorkspaceRemediation>> {
    match repository
        .begin_remediation_txn()
        .map_err(ShimError::Repository)?
    {
        WorkspaceTxnStart::Ready(transaction) => Ok(Ok(transaction)),
        WorkspaceTxnStart::Remediation(remediation) => Ok(Err(remediation)),
    }
}

/// Enter the repair boundary under an explicit effect budget (CB-13D).
///
/// Under [`ReconcileEffectBudget::MetadataOnly`] the entry refuses
/// effect-bearing adoption and pending recovery before any of it runs; a
/// `RepositoryError::ReactiveDeferred` surfaces as the typed CLI error.
pub(crate) fn enter_remediation_workspace_budgeted(
    repository: &mut Repository,
    budget: ReconcileEffectBudget,
) -> ShimResult<Result<WorkspaceTxn, WorkspaceRemediation>> {
    match repository
        .begin_remediation_txn_budgeted(budget)
        .map_err(ShimError::Repository)?
    {
        WorkspaceTxnStart::Ready(transaction) => Ok(Ok(transaction)),
        WorkspaceTxnStart::Remediation(remediation) => Ok(Err(remediation)),
    }
}

pub(crate) fn remediation_error(remediation: WorkspaceRemediation) -> ShimError {
    ShimError::StaleBaseline {
        report: format!(
            "workspace reconciliation required: {}",
            remediation.describe()
        ),
    }
}

/// Observe a workspace boundary while allowing forensic commands to report unsafe state.
pub(crate) fn observe_workspace(
    repository: &mut Repository,
) -> ShimResult<Result<WorkspaceTxn, WorkspaceRemediation>> {
    match repository
        .begin_workspace_txn(WorkspaceTxnMode::Observe)
        .map_err(ShimError::Repository)?
    {
        WorkspaceTxnStart::Ready(transaction) => Ok(Ok(transaction)),
        WorkspaceTxnStart::Remediation(remediation) => Ok(Err(remediation)),
    }
}

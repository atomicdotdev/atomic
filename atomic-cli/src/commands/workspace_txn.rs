//! Shared CB-5B/CB-5C entry adapter for working-copy command boundaries.

use atomic_repository::{
    ReconcileEffectBudget, Repository, WorkspaceRemediation, WorkspaceTxn, WorkspaceTxnMode,
    WorkspaceTxnStart,
};

use crate::error::{CliError, CliResult};

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
) -> CliResult<WorkspaceTxn> {
    match repository
        .begin_workspace_txn(mode)
        .map_err(CliError::Repository)?
    {
        WorkspaceTxnStart::Ready(transaction) => Ok(transaction),
        WorkspaceTxnStart::Remediation(remediation) => Err(remediation_error(remediation)),
    }
}

/// Observe a workspace boundary while allowing forensic commands to report unsafe state.
pub(crate) fn observe_workspace(
    repository: &mut Repository,
) -> CliResult<Result<WorkspaceTxn, WorkspaceRemediation>> {
    match repository
        .begin_workspace_txn(WorkspaceTxnMode::Observe)
        .map_err(CliError::Repository)?
    {
        WorkspaceTxnStart::Ready(transaction) => Ok(Ok(transaction)),
        WorkspaceTxnStart::Remediation(remediation) => Ok(Err(remediation)),
    }
}

/// Enter the explicit repair boundary for the bridge remediation path.
///
/// The bridge remediation command is what reconciles unanchored workspaces, so
/// it cannot require an anchored baseline; it still refuses Git-owned
/// operations, index locks, and diverged operation heads through the typed
/// remediation value.
pub(crate) fn enter_remediation_workspace(
    repository: &mut Repository,
) -> CliResult<Result<WorkspaceTxn, WorkspaceRemediation>> {
    match repository
        .begin_remediation_txn()
        .map_err(CliError::Repository)?
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
) -> CliResult<Result<WorkspaceTxn, WorkspaceRemediation>> {
    match repository
        .begin_remediation_txn_budgeted(budget)
        .map_err(CliError::Repository)?
    {
        WorkspaceTxnStart::Ready(transaction) => Ok(Ok(transaction)),
        WorkspaceTxnStart::Remediation(remediation) => Ok(Err(remediation)),
    }
}

pub(crate) fn remediation_error(remediation: WorkspaceRemediation) -> CliError {
    CliError::StaleBaseline {
        report: format!(
            "workspace reconciliation required: {}",
            remediation.describe()
        ),
    }
}

/// The CLI owns the separate Git coordination boundary, including ordinary
/// status/diff adoption. Release it before issuing an observation-only RPC;
/// the handler then independently verifies its workspace lease. Forensic
/// flags and record dry-run never enter this mutating boundary.
pub(crate) fn prepare_service_query(allow_git_operation: bool) -> CliResult<bool> {
    let Ok(root) = super::find_repository_root() else {
        return Ok(true);
    };
    let repo = Repository::open_readonly_wait(&root, atomic_repository::database_lock_wait())
        .map_err(CliError::Repository)?;
    if !repo
        .bridge_workspace_active()
        .map_err(CliError::Repository)?
    {
        return Ok(true);
    }
    drop(repo);
    let mut repo = Repository::open_for_workspace_transaction_wait(
        &root,
        atomic_repository::database_lock_wait(),
    )
    .map_err(CliError::Repository)?;
    match repo
        .begin_workspace_txn(WorkspaceTxnMode::Reconcile)
        .map_err(CliError::Repository)?
    {
        WorkspaceTxnStart::Ready(_) => Ok(true),
        // The wire cannot carry the #207 Git-operation diagnostic. Its native
        // renderer still reports this state without changing the working copy.
        WorkspaceTxnStart::Remediation(WorkspaceRemediation::GitOperationInProgress { .. })
            if allow_git_operation =>
        {
            Ok(false)
        }
        WorkspaceTxnStart::Remediation(remediation) => Err(remediation_error(remediation)),
    }
}

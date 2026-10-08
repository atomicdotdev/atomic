//! MaintenanceService handlers: repository repair and consistency.
//!
//! `atomic doctor` folds into Repair (the mutating actions) and
//! CheckRepository (the read-only consistency check). The read/write split
//! the state gate defers lands here for the read side: CheckRepository
//! opens the repository READ-ONLY without acquiring the per-repo gate.
//! Separate read-only handles share redb's file lock with other readers;
//! an exclusive writable handle still prevents these opens. Repair uses the
//! mutation gate. Compaction additionally requires exclusive access to the
//! physical database, including exclusion of independent reader handles.

use std::sync::Arc;

use crate::atomic::maintenance_service_server::MaintenanceService;
use crate::atomic::*;
use atomic_repository::CrdtMaterializeOptions;
use tonic::{Request, Response, Status};

use super::services::default_ref;
use super::state::{domain_status, repository_error, DaemonState};

mod compact;

pub struct MaintenanceImpl {
    pub state: Arc<DaemonState>,
}

fn repair_action(action: i32) -> Result<RepairAction, Status> {
    use RepairAction as Action;
    Ok(match action {
        x if x == Action::RebuildDependencyIndex as i32 => Action::RebuildDependencyIndex,
        x if x == Action::MaterializeCrdt as i32 => Action::MaterializeCrdt,
        x if x == Action::ReindexWorkingCopy as i32 => Action::ReindexWorkingCopy,
        _ => {
            return Err(domain_status(
                ErrorCode::InvalidArgument,
                "repair action is required (REBUILD_DEPENDENCY_INDEX, MATERIALIZE_CRDT, \
                 or REINDEX_WORKING_COPY)",
            ))
        }
    })
}

#[tonic::async_trait]
impl MaintenanceService for MaintenanceImpl {
    async fn compact_database(
        &self,
        request: Request<CompactDatabaseRequest>,
    ) -> Result<Response<CompactDatabaseResponse>, Status> {
        compact::compact(
            &self.state,
            request,
            atomic_agent::turn::orchestrator::wait_budget::database_wait(),
        )
        .await
        .map(Response::new)
    }

    async fn repair(
        &self,
        request: Request<RepairRequest>,
    ) -> Result<Response<RepairResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("Repair", Some(&handle));
        let action = repair_action(request.action)?;
        let force = request.force;
        let view = request.view;
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || -> Result<RepairResponse, Status> {
            let repo = handle.repository()?;
            match action {
                RepairAction::RebuildDependencyIndex => {
                    let (indexed, skipped, failed) = repo
                        .repair_change_dependency_index(force)
                        .map_err(repository_error)?;
                    Ok(RepairResponse {
                        findings: Vec::new(),
                        repaired: indexed as u64,
                        indexed: indexed as u64,
                        skipped: skipped as u64,
                        failed: failed as u64,
                        reindexed: 0,
                        ..Default::default()
                    })
                }
                RepairAction::ReindexWorkingCopy => {
                    // `status --reindex`: rebuild the working-copy index so
                    // stale rows stop producing false positives.
                    let reindexed = repo.reindex_working_copy().map_err(repository_error)?;
                    Ok(RepairResponse {
                        findings: Vec::new(),
                        reindexed: reindexed as u64,
                        ..Default::default()
                    })
                }
                RepairAction::MaterializeCrdt => {
                    let outcome = repo
                        .materialize_crdt_from_changes(CrdtMaterializeOptions {
                            view: if view.is_empty() { None } else { Some(view) },
                            force,
                        })
                        .map_err(repository_error)?;
                    Ok(RepairResponse {
                        findings: Vec::new(),
                        repaired: outcome.changes_applied as u64,
                        indexed: 0,
                        skipped: 0,
                        failed: 0,
                        reindexed: 0,
                        changes_scanned: outcome.changes_scanned as u64,
                        changes_applied: outcome.changes_applied as u64,
                        file_ops_applied: outcome.file_ops_applied as u64,
                        file_ops_already_materialized: outcome.file_ops_already_materialized as u64,
                        file_ops_skipped: outcome.file_ops_skipped as u64,
                        elapsed_ms: outcome.elapsed_ms as u64,
                        trunks_created: outcome.stats.trunks_created as u64,
                        branches_created: outcome.stats.branches_created as u64,
                        leaves_created: outcome.stats.leaves_created as u64,
                        skip_stats: Some(CrdtSkipStats {
                            non_create_trunk: outcome.skip_stats.non_create_trunk as u64,
                            unresolved_path: outcome.skip_stats.unresolved_path as u64,
                            unresolved_line: outcome.skip_stats.unresolved_line as u64,
                            missing_content_range: outcome.skip_stats.missing_content_range as u64,
                            non_insert_branch: outcome.skip_stats.non_insert_branch as u64,
                            non_insert_leaf: outcome.skip_stats.non_insert_leaf as u64,
                        }),
                        skip_samples: outcome.skip_samples.clone(),
                        meta: None,
                    })
                }
                RepairAction::Unspecified => unreachable!("validated above"),
            }
        })
        .await
        .map_err(|error| {
            domain_status(
                ErrorCode::Internal,
                format!("repair task panicked: {error}"),
            )
        })??;
        Ok(Response::new(result))
    }

    async fn check_repository(
        &self,
        request: Request<CheckRepositoryRequest>,
    ) -> Result<Response<CheckRepositoryResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("CheckRepository", Some(&handle));
        // No mutation gate. redb's shared file lock still refuses this
        // open while a writable handle (including compaction) is live.
        let root = handle.root.clone();
        let result = tokio::task::spawn_blocking(move || {
            let repo = atomic_repository::Repository::open_readonly(&root).map_err(|error| {
                domain_status(
                    ErrorCode::Repository,
                    format!("failed to open repository read-only: {error}"),
                )
            })?;
            let report = repo.verify_working_copy().map_err(repository_error)?;
            Ok::<_, Status>(CheckRepositoryResponse {
                findings: report.problems.iter().map(|p| p.to_string()).collect(),
                consistent: report.is_healthy(),
                clean_files_checked: report.clean_files_checked as u64,
                uncommitted_skipped: report.uncommitted_skipped as u64,
                conflicted_files: report.conflicted_files as u64,
            })
        })
        .await
        .map_err(|error| {
            domain_status(
                ErrorCode::Internal,
                format!("consistency check task panicked: {error}"),
            )
        })??;
        Ok(Response::new(result))
    }

    async fn reindex_workspace(
        &self,
        _request: Request<ReindexWorkspaceRequest>,
    ) -> Result<Response<ReindexWorkspaceResponse>, Status> {
        Err(Status::unimplemented(
            "ReindexWorkspace lands with its slice",
        ))
    }
}

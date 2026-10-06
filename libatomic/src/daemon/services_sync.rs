//! SyncService handlers — the remote registry (`atomic remote`), with the
//! push/pull remote-sync handlers riding the same service.
//!
//! The remote registry is persisted IN the repository database, so its CRUD
//! is service-layer work: `ListRemotes` serves the read-only listing and
//! `ManageRemotes` the mutating actions (add/remove/rename/set-url/set-
//! default — one action enum), each running the exact domain call the
//! local CLI body makes.

use std::sync::Arc;

use crate::atomic::ErrorCode;
use crate::atomic::*;
use atomic_repository::Repository;
use tonic::{Request, Response, Status};

use super::services_agent::response_meta;
use super::state::{domain_status, repository_error, DaemonState};

pub struct SyncImpl {
    pub state: Arc<DaemonState>,
}

fn remote_info(name: &str, entry: &atomic_repository::remote::RemoteEntry) -> RemoteInfo {
    RemoteInfo {
        name: name.to_string(),
        url: entry.url.clone(),
        default: entry.default,
    }
}

#[tonic::async_trait]
impl sync_service_server::SyncService for SyncImpl {
    async fn push_changes(
        &self,
        request: Request<PushChangesRequest>,
    ) -> Result<Response<PushChangesResponse>, Status> {
        super::services_sync_remote::push_changes_impl(self.state.clone(), request).await
    }

    async fn pull_changes(
        &self,
        request: Request<PullChangesRequest>,
    ) -> Result<Response<PullChangesResponse>, Status> {
        super::services_sync_remote::pull_changes_impl(self.state.clone(), request).await
    }

    async fn list_remotes(
        &self,
        request: Request<ListRemotesRequest>,
    ) -> Result<Response<ListRemotesResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ListRemotes", Some(&handle));
        let remotes = tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            repo.list_remotes().map_err(repository_error)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(ListRemotesResponse {
            remotes: remotes
                .iter()
                .map(|(name, entry)| remote_info(name, entry))
                .collect(),
        }))
    }

    async fn manage_remotes(
        &self,
        request: Request<ManageRemotesRequest>,
    ) -> Result<Response<ManageRemotesResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ManageRemotes", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let action = RemoteAction::try_from(request.action)
            .map_err(|_| domain_status(ErrorCode::InvalidArgument, "unknown remote action"))?;
        let name = request.name.clone();
        let url = request.url.clone();
        let new_name = request.new_name.clone();
        let set_default = request.set_default;
        let remotes = tokio::task::spawn_blocking(move || {
            let repo: Repository = handle.repository()?;
            let missing_name = || domain_status(ErrorCode::InvalidArgument, "remote name required");
            match action {
                RemoteAction::Add => {
                    let name = name.as_ref().ok_or_else(missing_name)?;
                    let url = url.as_ref().ok_or_else(|| {
                        domain_status(ErrorCode::InvalidArgument, "remote url required")
                    })?;
                    if set_default {
                        repo.add_remote_default(name, url)
                            .map_err(repository_error)?;
                    } else {
                        repo.add_remote(name, url).map_err(repository_error)?;
                    }
                }
                RemoteAction::Remove => {
                    let name = name.as_ref().ok_or_else(missing_name)?;
                    repo.remove_remote(name).map_err(repository_error)?;
                }
                RemoteAction::Rename => {
                    let name = name.as_ref().ok_or_else(missing_name)?;
                    let new_name = new_name.as_ref().ok_or_else(|| {
                        domain_status(ErrorCode::InvalidArgument, "new remote name required")
                    })?;
                    repo.rename_remote(name, new_name)
                        .map_err(repository_error)?;
                }
                RemoteAction::SetUrl => {
                    let name = name.as_ref().ok_or_else(missing_name)?;
                    let url = url.as_ref().ok_or_else(|| {
                        domain_status(ErrorCode::InvalidArgument, "remote url required")
                    })?;
                    repo.set_remote_url(name, url).map_err(repository_error)?;
                }
                RemoteAction::SetDefault => {
                    let name = name.as_ref().ok_or_else(missing_name)?;
                    repo.set_default_remote(name).map_err(repository_error)?;
                }
                RemoteAction::Unspecified => {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        "remote action required",
                    ));
                }
            }
            repo.list_remotes().map_err(repository_error)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(ManageRemotesResponse {
            remotes: remotes
                .iter()
                .map(|(name, entry)| remote_info(name, entry))
                .collect(),
            meta: response_meta(&meta),
        }))
    }

    async fn bind_remote_project(
        &self,
        request: Request<BindRemoteProjectRequest>,
    ) -> Result<Response<BindRemoteProjectResponse>, Status> {
        super::services_sync_remote::bind_remote_project_impl(self.state.clone(), request).await
    }
}

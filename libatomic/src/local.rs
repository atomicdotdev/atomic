//! In-process access to the canonical service contract. No client channel,
//! transport listener, or service process is constructed.

// Keep the existing handler error type intact at this shared service boundary.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use prost::Message;
use atomic_remote::{auth, HttpRemoteConfig};
use tonic::{Request, Status};

use crate::atomic::{
    attestation_service_server::AttestationService, daemon_service_server::DaemonService,
    knowledge_service_server::KnowledgeService,
    repository_mutation_service_server::RepositoryMutationService,
    repository_query_service_server::RepositoryQueryService, sync_service_server::SyncService,
    vault_service_server::VaultService,
    view_service_server::ViewService, AddFilesRequest, CreateViewRequest, DeleteViewRequest,
    GetChangeRequest, GetVaultContextRequest, GetVaultEntryRequest, GetWorkflowSnapshotRequest,
    ListVaultEntriesRequest, ListViewsRequest, LogRequest, PreviewRestoreRequest,
    QueryGraphRequest, RecordRequest, ReplayVaultEvidenceRequest, ResolveRepositoryRequest,
    StatusRequest, SwitchViewRequest, VerifyAttestationRequest, InsertChangesRequest,
    PushChangesRequest, PullChangesRequest, ListRemotesRequest, ManageRemotesRequest,
};
use crate::daemon::{
    services::{DaemonImpl, MutationImpl, QueryImpl},
    services_agent::{AttestationImpl, KnowledgeImpl, VaultImpl},
    services_query::ViewImpl,
    services_sync::SyncImpl,
    state::DaemonState,
    state::repository_error,
};

/// Embedded service session for trusted local callers.
///
/// Clones share repository routing and serialization gates, not open database
/// handles. Keep one instance per application and reuse it across calls.
/// This is not a replacement for remote authentication middleware.
///
/// Supports repository resolution, basic working-copy/change/view operations,
/// change insertion, configured remote registry management and authenticated push/pull,
/// read-only restore previews (including recorded file bytes),
/// read-only knowledge queries, vault get/list/context, attestation verification, and
/// bounded evidence replay. View-scoped vault reads are not supported; unsupported
/// methods fail explicitly with `Unimplemented`.
#[derive(Clone)]
pub struct LocalService {
    state: Arc<DaemonState>,
}

impl Default for LocalService {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalService {
    pub fn new() -> Self {
        Self {
            state: Arc::new(DaemonState::new()),
        }
    }

    /// Dispatch protobuf bytes to an existing handler in this process.
    /// Method names are canonical gRPC paths, but no RPC transport is used.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use libatomic::{LocalService, proto};
    /// use prost::Message;
    /// # async fn example() -> Result<(), tonic::Status> {
    /// let service = LocalService::new();
    /// let request = proto::ResolveRepositoryRequest { path: "/work/project".into() };
    /// let bytes = service.call(
    ///     "/atomic.DaemonService/ResolveRepository", &request.encode_to_vec(),
    /// ).await?;
    /// let resolved = proto::ResolveRepositoryResponse::decode(bytes.as_slice())
    ///     .map_err(|e| tonic::Status::internal(e.to_string()))?;
    /// let request = proto::StatusRequest { repository: resolved.repository, ..Default::default() };
    /// let bytes = service.call(
    ///     "/atomic.RepositoryQueryService/Status", &request.encode_to_vec(),
    /// ).await?;
    /// let status = proto::StatusResponse::decode(bytes.as_slice())
    ///     .map_err(|e| tonic::Status::internal(e.to_string()))?;
    /// println!("{}", status.view);
    /// # Ok(()) }
    /// ```
    pub async fn call(&self, method: &str, request: &[u8]) -> Result<Vec<u8>, Status> {
        match method {
            "/atomic.DaemonService/ResolveRepository" => {
                let mut request = decode::<ResolveRepositoryRequest>(request)?;
                // Filesystem discovery belongs to libatomic, not a foreign
                // binding. Canonicalize without blocking the async executor.
                let path = tokio::task::spawn_blocking(move || std::fs::canonicalize(request.path))
                    .await
                    .map_err(|e| Status::internal(format!("resolve task failed: {e}")))?
                    .map_err(|e| Status::not_found(e.to_string()))?;
                request.path = path
                    .into_os_string()
                    .into_string()
                    .map_err(|_| Status::invalid_argument("repository path is not UTF-8"))?;
                let response = DaemonImpl {
                    state: self.state.clone(),
                }
                .resolve_repository(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.RepositoryQueryService/Status" => {
                let request = decode::<StatusRequest>(request)?;
                // Reject a missing reference rather than trying ambient cwd.
                if request.repository.is_none() {
                    return Err(Status::invalid_argument("repository reference is required"));
                }
                let response = QueryImpl {
                    state: self.state.clone(),
                }
                .status(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.RepositoryMutationService/AddFiles" => {
                let request = decode::<AddFilesRequest>(request)?;
                require_repository(&request.repository)?;
                let response = MutationImpl {
                    state: self.state.clone(),
                }
                .add_files(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.RepositoryMutationService/Record" => {
                let request = decode::<RecordRequest>(request)?;
                require_repository(&request.repository)?;
                let response = MutationImpl {
                    state: self.state.clone(),
                }
                .record(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.RepositoryMutationService/InsertChanges" => {
                let request = decode::<InsertChangesRequest>(request)?;
                require_repository(&request.repository)?;
                let response = MutationImpl { state: self.state.clone() }
                    .insert_changes(Request::new(request)).await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.SyncService/ListRemotes" => {
                let request = decode::<ListRemotesRequest>(request)?;
                require_repository(&request.repository)?;
                let response = SyncImpl { state: self.state.clone() }
                    .list_remotes(Request::new(request)).await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.SyncService/ManageRemotes" => {
                let request = decode::<ManageRemotesRequest>(request)?;
                require_repository(&request.repository)?;
                let response = SyncImpl { state: self.state.clone() }
                    .manage_remotes(Request::new(request)).await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.SyncService/PushChanges" => {
                let mut request = decode::<PushChangesRequest>(request)?;
                require_repository(&request.repository)?;
                reject_transport_credentials(&request.auth_headers, &request.remote_url)?;
                let (name, url, config) = self.sync_credentials(
                    request.repository.as_ref().unwrap(), &request.remote,
                    request.identity.clone(), true,
                ).await?;
                request.remote = name;
                request.remote_url = Some(url);
                request.auth_headers = config.extra_headers.into_iter().collect();
                let response = SyncImpl { state: self.state.clone() }
                    .push_changes(Request::new(request)).await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.SyncService/PullChanges" => {
                let mut request = decode::<PullChangesRequest>(request)?;
                require_repository(&request.repository)?;
                reject_transport_credentials(&request.auth_headers, &request.remote_url)?;
                let (name, url, config) = self.sync_credentials(
                    request.repository.as_ref().unwrap(), &request.remote,
                    request.identity.clone(), false,
                ).await?;
                request.remote = name;
                request.remote_url = Some(url);
                request.auth_headers = config.extra_headers.into_iter().collect();
                let response = SyncImpl { state: self.state.clone() }
                    .pull_changes(Request::new(request)).await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.RepositoryQueryService/Log" => {
                let request = decode::<LogRequest>(request)?;
                require_repository(&request.repository)?;
                let response = QueryImpl {
                    state: self.state.clone(),
                }
                .log(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.RepositoryQueryService/GetChange" => {
                let request = decode::<GetChangeRequest>(request)?;
                require_repository(&request.repository)?;
                let response = QueryImpl {
                    state: self.state.clone(),
                }
                .get_change(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.RepositoryQueryService/PreviewRestore" => {
                let request = decode::<PreviewRestoreRequest>(request)?;
                require_repository(&request.repository)?;
                let response = QueryImpl {
                    state: self.state.clone(),
                }
                .preview_restore(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.ViewService/ListViews" => {
                let request = decode::<ListViewsRequest>(request)?;
                require_repository(&request.repository)?;
                let response = ViewImpl {
                    state: self.state.clone(),
                }
                .list_views(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.ViewService/CreateView" => {
                let request = decode::<CreateViewRequest>(request)?;
                require_repository(&request.repository)?;
                let response = ViewImpl {
                    state: self.state.clone(),
                }
                .create_view(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.ViewService/SwitchView" => {
                let request = decode::<SwitchViewRequest>(request)?;
                require_repository(&request.repository)?;
                let response = ViewImpl {
                    state: self.state.clone(),
                }
                .switch_view(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.ViewService/DeleteView" => {
                let request = decode::<DeleteViewRequest>(request)?;
                require_repository(&request.repository)?;
                let response = ViewImpl {
                    state: self.state.clone(),
                }
                .delete_view(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.VaultService/GetVaultEntry" => {
                let request = decode::<GetVaultEntryRequest>(request)?;
                require_repository(&request.repository)?;
                let response = VaultImpl {
                    state: self.state.clone(),
                }
                .get_vault_entry(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.VaultService/ListVaultEntries" => {
                let request = decode::<ListVaultEntriesRequest>(request)?;
                require_repository(&request.repository)?;
                let response = VaultImpl {
                    state: self.state.clone(),
                }
                .list_vault_entries(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.VaultService/GetVaultContext" => {
                let request = decode::<GetVaultContextRequest>(request)?;
                require_repository(&request.repository)?;
                let response = VaultImpl {
                    state: self.state.clone(),
                }
                .get_vault_context(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.VaultService/ReplayVaultEvidence" => {
                let request = decode::<ReplayVaultEvidenceRequest>(request)?;
                require_repository(&request.repository)?;
                let response = VaultImpl {
                    state: self.state.clone(),
                }
                .replay_vault_evidence(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.KnowledgeService/QueryGraph" => {
                let request = decode::<QueryGraphRequest>(request)?;
                require_repository(&request.repository)?;
                let response = KnowledgeImpl {
                    state: self.state.clone(),
                }
                .query_graph(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.KnowledgeService/GetWorkflowSnapshot" => {
                let request = decode::<GetWorkflowSnapshotRequest>(request)?;
                require_repository(&request.repository)?;
                let response = KnowledgeImpl {
                    state: self.state.clone(),
                }
                .get_workflow_snapshot(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            "/atomic.AttestationService/VerifyAttestation" => {
                let request = decode::<VerifyAttestationRequest>(request)?;
                require_repository(&request.repository)?;
                let response = AttestationImpl {
                    state: self.state.clone(),
                }
                .verify_attestation(Request::new(request))
                .await?;
                Ok(response.into_inner().encode_to_vec())
            }
            _ => Err(Status::unimplemented(format!(
                "local method is not supported: {method}"
            ))),
        }
    }

    /// Resolve all ambient configuration only at the trusted local boundary.
    async fn sync_credentials(
        &self,
        repository: &crate::atomic::RepositoryRef,
        remote: &str,
        identity: Option<String>,
        push: bool,
    ) -> Result<(String, String, HttpRemoteConfig), Status> {
        let handle = self.state.resolve(repository)?;
        let remote = (!remote.is_empty()).then(|| remote.to_owned());
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            let name = remote.unwrap_or_else(|| {
                repo.get_default_remote()
                    .map(|(name, _)| name)
                    .unwrap_or_else(|_| "origin".to_owned())
            });
            let url = if name.contains("://") {
                name.clone()
            } else {
                repo.get_remote(&name).map_err(|error| match error {
                    atomic_repository::RepositoryError::RemoteNotFound { .. } =>
                        crate::daemon::state::domain_status(
                            crate::atomic::ErrorCode::NotFound, format!("remote '{name}' not found"),
                        ),
                    other => repository_error(other),
                })?.url
            };
            if push {
                auth::check_push_credentials(&url, identity.as_deref()).map_err(auth_status)?;
            }
            drop(repo);
            let config = runtime.block_on(auth::attach_identity(
                HttpRemoteConfig::new(), &url, identity.as_deref(),
            ));
            Ok::<_, Status>((name, url, config))
        }).await.map_err(|e| Status::internal(format!("credentials task failed: {e}")))?
    }
}

fn reject_transport_credentials(
    headers: &std::collections::HashMap<String, String>,
    url: &Option<String>,
) -> Result<(), Status> {
    if !headers.is_empty() || url.is_some() {
        return Err(Status::invalid_argument(
            "trusted local sync does not accept auth_headers or remote_url",
        ));
    }
    Ok(())
}

fn auth_status(error: auth::AuthError) -> Status {
    match error {
        auth::AuthError::Internal(error) => Status::internal(error.to_string()),
        auth::AuthError::AuthenticationFailed { remote } =>
            Status::unauthenticated(format!("authentication failed for {remote}")),
        auth::AuthError::DelegationError { message } => Status::permission_denied(message),
    }
}

fn require_repository(reference: &Option<crate::proto::RepositoryRef>) -> Result<(), Status> {
    if reference.is_none() {
        return Err(Status::invalid_argument("repository reference is required"));
    }
    Ok(())
}

fn decode<M: Message + Default>(bytes: &[u8]) -> Result<M, Status> {
    M::decode(bytes).map_err(|e| Status::invalid_argument(format!("invalid protobuf request: {e}")))
}

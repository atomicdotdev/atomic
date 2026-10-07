//! The CLI's service area: one interface, two backends.
//!
//! Every wired command speaks the `atomic` contract through [`Service`];
//! the backend is chosen once per invocation by [`mode()`]:
//!
//!  * **local** (the default, `ATOMIC_SERVICE=local`) — the handlers run
//!    IN THIS PROCESS: the same libatomic service impls the atomicd
//!    transport serves over the socket, called directly on a locally
//!    constructed [`DaemonState`]. No socket is probed, no daemon is
//!    spawned, no client is constructed — not even a health check. The
//!    redb single-writer invariant holds by assumption (no concurrent
//!    daemon on the same machine).
//!  * **reactor** (`ATOMIC_SERVICE=reactor`) — today's D4 semantics: the
//!    generated clients over the atomic-client socket session, starting
//!    the atomicd transport when it is down (start-or-retry). Reactor
//!    mode never constructs local handlers in this process.
//!
//! `ATOMIC_RPC` remains as a legacy alias: `ATOMIC_RPC=1` is
//! `ATOMIC_SERVICE=reactor`; anything else routes local. The routing
//! default is local — the CLI no longer probes for a reachable daemon.
//!
//! Every wired flag-form opens a service session — including the ones
//! that once had no wire shape (the contract was extended add-only, so
//! `insert --deps=false` and the rest now route).

use std::path::Path;
use std::sync::Arc;

use atomic_client::proto as pb;
use libatomic::daemon::services;
use libatomic::daemon::services_agent;
use libatomic::daemon::services_maintenance;
use libatomic::daemon::services_query;
use libatomic::daemon::services_sandbox;
use libatomic::daemon::services_sync;
use libatomic::daemon::services_tag;
use libatomic::daemon::services_triage;
use libatomic::daemon::state::DaemonState;
// The tonic server traits: in scope so the local backend calls the very
// same trait methods the transport serves.
use libatomic::atomic::attestation_service_server::AttestationService as _;
use libatomic::atomic::daemon_service_server::DaemonService as _;
use libatomic::atomic::knowledge_service_server::KnowledgeService as _;
use libatomic::atomic::maintenance_service_server::MaintenanceService as _;
use libatomic::atomic::provenance_service_server::ProvenanceService as _;
use libatomic::atomic::repository_mutation_service_server::RepositoryMutationService as _;
use libatomic::atomic::repository_query_service_server::RepositoryQueryService as _;
use libatomic::atomic::sandbox_service_server::SandboxService as _;
use libatomic::atomic::sync_service_server::SyncService as _;
use libatomic::atomic::tag_service_server::TagService as _;
use libatomic::atomic::triage_service_server::TriageService as _;
use libatomic::atomic::vault_service_server::VaultService as _;
use libatomic::atomic::view_service_server::ViewService as _;
use tonic::transport::Channel;
use tonic::{Request, Status};

use crate::error::{CliError, CliResult};

/// The service-area selector. `ATOMIC_SERVICE` wins; `ATOMIC_RPC` is the
/// legacy alias (1 → reactor, anything else → local); unset routes local.
pub const ENV_SERVICE: &str = "ATOMIC_SERVICE";

/// The legacy alias, honored verbatim: `ATOMIC_RPC=1` routed strictly over
/// the daemon socket, `ATOMIC_RPC=0` never routed.
pub const ENV_RPC: &str = "ATOMIC_RPC";

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Mode {
    /// The handlers run in-process (libatomic, no socket, no daemon).
    Local,
    /// The handlers run in the atomicd transport over the socket (D4).
    Reactor,
}

/// Resolve the service mode from the environment. See the module docs:
/// local is the default; unknown values route local (no socket probing —
/// an `ATOMIC_SERVICE=auto` refinement is deliberately out of scope).
pub fn mode() -> Mode {
    match std::env::var(ENV_SERVICE).ok().as_deref() {
        Some("reactor") => return Mode::Reactor,
        Some("local") => return Mode::Local,
        _ => {}
    }
    match std::env::var(ENV_RPC).ok().as_deref() {
        Some("1") => Mode::Reactor,
        _ => Mode::Local,
    }
}

/// One backend's live session state.
enum Backend {
    /// The in-process handler state — libatomic's registry, gates, and
    /// handlers, exactly what the transport serves.
    Local(Arc<DaemonState>),
    /// A connected channel to the atomicd transport.
    Reactor(Channel),
}

/// A connected session for one command invocation: the resolved
/// persistent repository reference (path discovery happens once, before
/// any domain call) plus the backend the mode selected.
pub struct Service {
    mode: Mode,
    /// The persistent repository reference every request echoes.
    pub reference: pb::RepositoryRef,
    local: Option<Arc<DaemonState>>,
    reactor: Option<Channel>,
    /// The runtime driving the handlers (both backends are async; tonic
    /// spawns the connection's background tasks on the connecting
    /// runtime — dropping it severs the channel, so it lives as long as
    /// the session).
    rt: tokio::runtime::Runtime,
}

impl Service {
    /// Open the service session for the ambient repository. Returns
    /// `None` outside a repository — the caller falls through to its own
    /// (proper) not-a-repository error path.
    pub fn open() -> CliResult<Option<Service>> {
        let Ok(root) = crate::commands::find_repository_root() else {
            return Ok(None);
        };
        Self::open_root(&root)
    }

    /// Open the service session for an explicit repository path (the
    /// Reactor's typed dispatch always passes the canonical root).
    pub fn open_root(root: &Path) -> CliResult<Option<Service>> {
        let mode = mode();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| CliError::Internal(anyhow::anyhow!("service runtime: {error}")))?;
        match mode {
            Mode::Local => {
                // In-process: construct the handler state and resolve the
                // repository through the SAME handler the transport serves.
                // No socket, no daemon, no client — ever.
                let state = Arc::new(DaemonState::new());
                let daemon = services::DaemonImpl {
                    state: state.clone(),
                };
                let resolved = rt
                    .block_on(daemon.resolve_repository(Request::new(
                        pb::ResolveRepositoryRequest {
                            path: root.display().to_string(),
                        },
                    )))
                    .map_err(|error| {
                        CliError::Internal(anyhow::anyhow!("local service resolve failed: {error}"))
                    })?
                    .into_inner();
                let reference = resolved.repository.ok_or_else(|| {
                    CliError::Internal(anyhow::anyhow!("local service resolved no repository"))
                })?;
                Ok(Some(Service {
                    mode,
                    reference,
                    local: Some(state),
                    reactor: None,
                    rt,
                }))
            }
            Mode::Reactor => {
                // D4 start-or-retry: connect, starting the transport when
                // it is down, then resolve the repository over the socket.
                // Strict — there is no auto fallback.
                let connect = async {
                    let client = atomic_client::AtomicClient::connect_or_start()
                        .await
                        .map_err(|e| anyhow::anyhow!(e))?;
                    let mut daemon =
                        pb::daemon_service_client::DaemonServiceClient::new(client.channel.clone());
                    let resolved = daemon
                        .resolve_repository(pb::ResolveRepositoryRequest {
                            path: root.display().to_string(),
                        })
                        .await?
                        .into_inner();
                    let reference = resolved
                        .repository
                        .ok_or_else(|| anyhow::anyhow!("daemon resolved no repository"))?;
                    Ok::<_, anyhow::Error>((client.channel, reference))
                };
                let (channel, reference) = rt.block_on(connect).map_err(|error| {
                    CliError::Internal(anyhow::anyhow!("reactor service routing failed: {error}"))
                })?;
                Ok(Some(Service {
                    mode,
                    reference,
                    local: None,
                    reactor: Some(channel),
                    rt,
                }))
            }
        }
    }

    fn backend(&self) -> Backend {
        match self.mode {
            Mode::Local => Backend::Local(self.local.clone().expect("local backend state")),
            Mode::Reactor => {
                Backend::Reactor(self.reactor.clone().expect("reactor backend channel"))
            }
        }
    }

    /// Drive one service exchange on the session's runtime, mapping the
    /// transport/domain status to the CLI's error vocabulary.
    fn call<T, F, Fut>(&self, exchange: F) -> CliResult<T>
    where
        F: FnOnce(Backend) -> Fut,
        Fut: std::future::Future<Output = Result<T, Status>>,
    {
        let backend = self.backend();
        self.rt.block_on(exchange(backend)).map_err(status_message)
    }

    /// The mode this session was opened in (the journal sink and other
    /// mode-aware components ask).
    pub fn mode(&self) -> Mode {
        self.mode
    }
}

/// A transport/domain refusal surfaced with its contract vocabulary. The
/// handler already encodes the stable error code and the human message
/// (`atomic:VIEW: ...`); the gRPC plumbing around it is transport noise.
/// User-fixable refusals (preconditions, bad arguments, not-found,
/// permission) exit as user errors — only genuine internal failures are
/// bugs.
pub fn status_message(error: tonic::Status) -> CliError {
    use tonic::Code;
    let message = error.message().to_string();
    match error.code() {
        Code::FailedPrecondition
        | Code::InvalidArgument
        | Code::NotFound
        | Code::AlreadyExists
        | Code::PermissionDenied
        | Code::Unauthenticated
        | Code::ResourceExhausted
        | Code::Cancelled
        | Code::Unavailable => CliError::ServiceRefusal { message },
        _ => CliError::Internal(anyhow::anyhow!("{message}")),
    }
}

// ---------------------------------------------------------------------------
// DaemonService (session keep-alive)
// ---------------------------------------------------------------------------

// (ResolveRepository runs inside Service::open on both backends.)

// ---------------------------------------------------------------------------
// RepositoryQueryService
// ---------------------------------------------------------------------------

impl Service {
    pub fn status(&self, request: pb::StatusRequest) -> CliResult<pb::StatusResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services::QueryImpl { state }
                    .status(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut query =
                        pb::repository_query_service_client::RepositoryQueryServiceClient::new(
                            channel,
                        );
                    query
                        .status(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn log(&self, request: pb::LogRequest) -> CliResult<pb::LogResponse> {
        // all_views / path_filter / tags_only ride the request as-is.
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services::QueryImpl { state }
                    .log(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut query =
                        pb::repository_query_service_client::RepositoryQueryServiceClient::new(
                            channel,
                        );
                    query
                        .log(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn get_change(&self, request: pb::GetChangeRequest) -> CliResult<pb::GetChangeResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services::QueryImpl { state }
                    .get_change(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut query =
                        pb::repository_query_service_client::RepositoryQueryServiceClient::new(
                            channel,
                        );
                    query
                        .get_change(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    /// DiffWorkingCopy, collected: both backends stream; the CLI renders
    /// the whole listing.
    pub fn diff_working_copy(
        &self,
        request: pb::DiffWorkingCopyRequest,
    ) -> CliResult<Vec<pb::DiffChunk>> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    let stream = services::QueryImpl { state }
                        .diff_working_copy(Request::new(request))
                        .await?
                        .into_inner();
                    collect_chunks(Box::pin(stream)).await
                }
                Backend::Reactor(channel) => {
                    let mut query =
                        pb::repository_query_service_client::RepositoryQueryServiceClient::new(
                            channel,
                        );
                    let stream = query.diff_working_copy(request).await?.into_inner();
                    collect_streaming(stream).await
                }
            }
        })
    }

    /// The view-pair Diff, collected (same rendering as the working copy).
    pub fn diff(&self, request: pb::DiffRequest) -> CliResult<Vec<pb::DiffChunk>> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    let stream = services::QueryImpl { state }
                        .diff(Request::new(request))
                        .await?
                        .into_inner();
                    collect_chunks(Box::pin(stream)).await
                }
                Backend::Reactor(channel) => {
                    let mut query =
                        pb::repository_query_service_client::RepositoryQueryServiceClient::new(
                            channel,
                        );
                    let stream = query.diff(request).await?.into_inner();
                    collect_streaming(stream).await
                }
            }
        })
    }

    pub fn list_conflicts(
        &self,
        request: pb::ListConflictsRequest,
    ) -> CliResult<pb::ListConflictsResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services::QueryImpl { state }
                    .list_conflicts(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut query =
                        pb::repository_query_service_client::RepositoryQueryServiceClient::new(
                            channel,
                        );
                    query
                        .list_conflicts(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn preview_mutation(
        &self,
        request: pb::PreviewMutationRequest,
    ) -> CliResult<pb::PreviewMutationResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services::QueryImpl { state }
                    .preview_mutation(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut query =
                        pb::repository_query_service_client::RepositoryQueryServiceClient::new(
                            channel,
                        );
                    query
                        .preview_mutation(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn preview_restore(
        &self,
        request: pb::PreviewRestoreRequest,
    ) -> CliResult<pb::PreviewMutationResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services::QueryImpl { state }
                    .preview_restore(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut query =
                        pb::repository_query_service_client::RepositoryQueryServiceClient::new(
                            channel,
                        );
                    query
                        .preview_restore(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

/// Collect a local handler's item stream (each item is already a
/// `Result`).
async fn collect_chunks(
    mut stream: std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<pb::DiffChunk, Status>> + Send>,
    >,
) -> Result<Vec<pb::DiffChunk>, Status> {
    let mut chunks = Vec::new();
    while let Some(chunk) = tokio_stream::StreamExt::next(&mut stream).await {
        chunks.push(chunk?);
    }
    Ok(chunks)
}

/// Collect a reactor client's tonic streaming response.
async fn collect_streaming(
    mut stream: tonic::Streaming<pb::DiffChunk>,
) -> Result<Vec<pb::DiffChunk>, Status> {
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.message().await? {
        chunks.push(chunk);
    }
    Ok(chunks)
}

// ---------------------------------------------------------------------------
// RepositoryMutationService
// ---------------------------------------------------------------------------

impl Service {
    pub fn add_files(&self, request: pb::AddFilesRequest) -> CliResult<pb::AddFilesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .add_files(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.add_files(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn record(&self, request: pb::RecordRequest) -> CliResult<pb::RecordResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .record(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.record(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn remove_files(
        &self,
        request: pb::RemoveFilesRequest,
    ) -> CliResult<pb::RemoveFilesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .remove_files(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.remove_files(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn move_file(&self, request: pb::MoveFileRequest) -> CliResult<pb::MoveFileResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .move_file(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.move_file(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn unrecord(&self, request: pb::UnrecordRequest) -> CliResult<pb::UnrecordResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .unrecord(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.unrecord(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn restore(&self, request: pb::RestoreRequest) -> CliResult<pb::RestoreResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .restore(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.restore(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn create_stash(
        &self,
        request: pb::CreateStashRequest,
    ) -> CliResult<pb::CreateStashResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .create_stash(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.create_stash(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn apply_stash(&self, request: pb::ApplyStashRequest) -> CliResult<pb::ApplyStashResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .apply_stash(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.apply_stash(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn list_stashes(
        &self,
        request: pb::ListStashesRequest,
    ) -> CliResult<pb::ListStashesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .list_stashes(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.list_stashes(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn drop_stash(&self, request: pb::DropStashRequest) -> CliResult<pb::DropStashResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .drop_stash(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.drop_stash(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn insert_changes(
        &self,
        request: pb::InsertChangesRequest,
    ) -> CliResult<pb::InsertChangesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .insert_changes(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.insert_changes(request).await.map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn revise(&self, request: pb::ReviseRequest) -> CliResult<pb::ReviseResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => {
                    services::MutationImpl { state }
                        .revise(Request::new(request))
                        .await
                        .map(|response| response.into_inner())
                }
                Backend::Reactor(channel) => {
                    let mut mutation =
                        pb::repository_mutation_service_client::RepositoryMutationServiceClient::new(channel);
                    mutation.revise(request).await.map(|response| response.into_inner())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// VaultService
// ---------------------------------------------------------------------------

impl Service {
    pub fn create_vault_entity(
        &self,
        request: pb::CreateVaultEntityRequest,
    ) -> CliResult<pb::CreateVaultEntityResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .create_vault_entity(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .create_vault_entity(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn update_vault_entity(
        &self,
        request: pb::UpdateVaultEntityRequest,
    ) -> CliResult<pb::UpdateVaultEntityResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .update_vault_entity(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .update_vault_entity(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn get_vault_entry(
        &self,
        request: pb::GetVaultEntryRequest,
    ) -> CliResult<pb::GetVaultEntryResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .get_vault_entry(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .get_vault_entry(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn list_vault_entries(
        &self,
        request: pb::ListVaultEntriesRequest,
    ) -> CliResult<pb::ListVaultEntriesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .list_vault_entries(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .list_vault_entries(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn validate_vault_entity(
        &self,
        request: pb::ValidateVaultEntityRequest,
    ) -> CliResult<pb::ValidateVaultEntityResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .validate_vault_entity(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .validate_vault_entity(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn delete_vault_entity(
        &self,
        request: pb::DeleteVaultEntityRequest,
    ) -> CliResult<pb::DeleteVaultEntityResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .delete_vault_entity(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .delete_vault_entity(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn link_vault_entities(
        &self,
        request: pb::LinkVaultEntitiesRequest,
    ) -> CliResult<pb::LinkVaultEntitiesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .link_vault_entities(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .link_vault_entities(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn init_vault(&self, request: pb::InitVaultRequest) -> CliResult<pb::InitVaultResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .init_vault(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .init_vault(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn export_vault(
        &self,
        request: pb::ExportVaultRequest,
    ) -> CliResult<pb::ExportVaultResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .export_vault(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .export_vault(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn sync_vault(&self, request: pb::SyncVaultRequest) -> CliResult<pb::SyncVaultResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .sync_vault(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .sync_vault(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn get_vault_context(
        &self,
        request: pb::GetVaultContextRequest,
    ) -> CliResult<pb::GetVaultContextResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::VaultImpl { state }
                    .get_vault_context(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut vault = pb::vault_service_client::VaultServiceClient::new(channel);
                    vault
                        .get_vault_context(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// AttestationService
// ---------------------------------------------------------------------------

impl Service {
    pub fn record_attestation(
        &self,
        request: pb::RecordAttestationRequest,
    ) -> CliResult<pb::RecordAttestationResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::AttestationImpl { state }
                    .record_attestation(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut attestation =
                        pb::attestation_service_client::AttestationServiceClient::new(channel);
                    attestation
                        .record_attestation(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn verify_attestation(
        &self,
        request: pb::VerifyAttestationRequest,
    ) -> CliResult<pb::VerifyAttestationResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::AttestationImpl { state }
                    .verify_attestation(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut attestation =
                        pb::attestation_service_client::AttestationServiceClient::new(channel);
                    attestation
                        .verify_attestation(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn list_attestations(
        &self,
        request: pb::ListAttestationsRequest,
    ) -> CliResult<pb::ListAttestationsResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::AttestationImpl { state }
                    .list_attestations(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut attestation =
                        pb::attestation_service_client::AttestationServiceClient::new(channel);
                    attestation
                        .list_attestations(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// KnowledgeService
// ---------------------------------------------------------------------------

impl Service {
    pub fn query_graph(&self, request: pb::QueryGraphRequest) -> CliResult<pb::QueryGraphResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::KnowledgeImpl { state }
                    .query_graph(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut knowledge =
                        pb::knowledge_service_client::KnowledgeServiceClient::new(channel);
                    knowledge
                        .query_graph(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn maintain_knowledge_graph(
        &self,
        request: pb::MaintainKnowledgeGraphRequest,
    ) -> CliResult<pb::MaintainKnowledgeGraphResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::KnowledgeImpl { state }
                    .maintain_knowledge_graph(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut knowledge =
                        pb::knowledge_service_client::KnowledgeServiceClient::new(channel);
                    knowledge
                        .maintain_knowledge_graph(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// ProvenanceService
// ---------------------------------------------------------------------------

impl Service {
    pub fn dispatch_turn_event(
        &self,
        request: pb::DispatchTurnEventRequest,
    ) -> CliResult<pb::DispatchTurnEventResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::ProvenanceImpl { state }
                    .dispatch_turn_event(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut provenance =
                        pb::provenance_service_client::ProvenanceServiceClient::new(channel);
                    provenance
                        .dispatch_turn_event(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn list_sessions(
        &self,
        request: pb::ListSessionsRequest,
    ) -> CliResult<pb::ListSessionsResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::ProvenanceImpl { state }
                    .list_sessions(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut provenance =
                        pb::provenance_service_client::ProvenanceServiceClient::new(channel);
                    provenance
                        .list_sessions(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn get_session(&self, request: pb::GetSessionRequest) -> CliResult<pb::GetSessionResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::ProvenanceImpl { state }
                    .get_session(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut provenance =
                        pb::provenance_service_client::ProvenanceServiceClient::new(channel);
                    provenance
                        .get_session(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn export_provenance(
        &self,
        request: pb::ExportProvenanceRequest,
    ) -> CliResult<pb::ExportProvenanceResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::ProvenanceImpl { state }
                    .export_provenance(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut provenance =
                        pb::provenance_service_client::ProvenanceServiceClient::new(channel);
                    provenance
                        .export_provenance(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn fork_session(
        &self,
        request: pb::ForkSessionRequest,
    ) -> CliResult<pb::ForkSessionResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::ProvenanceImpl { state }
                    .fork_session(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut provenance =
                        pb::provenance_service_client::ProvenanceServiceClient::new(channel);
                    provenance
                        .fork_session(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn rebuild_session_index(
        &self,
        request: pb::RebuildSessionIndexRequest,
    ) -> CliResult<pb::RebuildSessionIndexResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::ProvenanceImpl { state }
                    .rebuild_session_index(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut provenance =
                        pb::provenance_service_client::ProvenanceServiceClient::new(channel);
                    provenance
                        .rebuild_session_index(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn explain_turns(
        &self,
        request: pb::ExplainTurnsRequest,
    ) -> CliResult<pb::ExplainTurnsResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_agent::ProvenanceImpl { state }
                    .explain_turns(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut provenance =
                        pb::provenance_service_client::ProvenanceServiceClient::new(channel);
                    provenance
                        .explain_turns(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// MaintenanceService
// ---------------------------------------------------------------------------

impl Service {
    pub fn check_repository(
        &self,
        request: pb::CheckRepositoryRequest,
    ) -> CliResult<pb::CheckRepositoryResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_maintenance::MaintenanceImpl { state }
                    .check_repository(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut maintenance =
                        pb::maintenance_service_client::MaintenanceServiceClient::new(channel);
                    maintenance
                        .check_repository(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn repair(&self, request: pb::RepairRequest) -> CliResult<pb::RepairResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_maintenance::MaintenanceImpl { state }
                    .repair(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut maintenance =
                        pb::maintenance_service_client::MaintenanceServiceClient::new(channel);
                    maintenance
                        .repair(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// ViewService
// ---------------------------------------------------------------------------

impl Service {
    pub fn list_views(&self, request: pb::ListViewsRequest) -> CliResult<pb::ListViewsResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_query::ViewImpl { state }
                    .list_views(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut views = pb::view_service_client::ViewServiceClient::new(channel);
                    views
                        .list_views(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn create_view(&self, request: pb::CreateViewRequest) -> CliResult<pb::CreateViewResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_query::ViewImpl { state }
                    .create_view(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut views = pb::view_service_client::ViewServiceClient::new(channel);
                    views
                        .create_view(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn switch_view(&self, request: pb::SwitchViewRequest) -> CliResult<pb::SwitchViewResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_query::ViewImpl { state }
                    .switch_view(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut views = pb::view_service_client::ViewServiceClient::new(channel);
                    views
                        .switch_view(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn delete_view(&self, request: pb::DeleteViewRequest) -> CliResult<pb::DeleteViewResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_query::ViewImpl { state }
                    .delete_view(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut views = pb::view_service_client::ViewServiceClient::new(channel);
                    views
                        .delete_view(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn set_view_scope(
        &self,
        request: pb::SetViewScopeRequest,
    ) -> CliResult<pb::SetViewScopeResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_query::ViewImpl { state }
                    .set_view_scope(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut views = pb::view_service_client::ViewServiceClient::new(channel);
                    views
                        .set_view_scope(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn split_view(&self, request: pb::SplitViewRequest) -> CliResult<pb::SplitViewResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_query::ViewImpl { state }
                    .split_view(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut views = pb::view_service_client::ViewServiceClient::new(channel);
                    views
                        .split_view(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// TagService
// ---------------------------------------------------------------------------

impl Service {
    pub fn create_tag(&self, request: pb::CreateTagRequest) -> CliResult<pb::CreateTagResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_tag::TagImpl { state }
                    .create_tag(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut tags = pb::tag_service_client::TagServiceClient::new(channel);
                    tags.create_tag(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn delete_tag(&self, request: pb::DeleteTagRequest) -> CliResult<pb::DeleteTagResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_tag::TagImpl { state }
                    .delete_tag(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut tags = pb::tag_service_client::TagServiceClient::new(channel);
                    tags.delete_tag(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn list_tags(&self, request: pb::ListTagsRequest) -> CliResult<pb::ListTagsResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_tag::TagImpl { state }
                    .list_tags(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut tags = pb::tag_service_client::TagServiceClient::new(channel);
                    tags.list_tags(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn get_tag(&self, request: pb::GetTagRequest) -> CliResult<pb::GetTagResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_tag::TagImpl { state }
                    .get_tag(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut tags = pb::tag_service_client::TagServiceClient::new(channel);
                    tags.get_tag(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// SyncService
// ---------------------------------------------------------------------------

impl Service {
    pub fn list_remotes(
        &self,
        request: pb::ListRemotesRequest,
    ) -> CliResult<pb::ListRemotesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_sync::SyncImpl { state }
                    .list_remotes(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut sync = pb::sync_service_client::SyncServiceClient::new(channel);
                    sync.list_remotes(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn manage_remotes(
        &self,
        request: pb::ManageRemotesRequest,
    ) -> CliResult<pb::ManageRemotesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_sync::SyncImpl { state }
                    .manage_remotes(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut sync = pb::sync_service_client::SyncServiceClient::new(channel);
                    sync.manage_remotes(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn push_changes(
        &self,
        request: pb::PushChangesRequest,
    ) -> CliResult<pb::PushChangesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_sync::SyncImpl { state }
                    .push_changes(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut sync = pb::sync_service_client::SyncServiceClient::new(channel);
                    sync.push_changes(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn pull_changes(
        &self,
        request: pb::PullChangesRequest,
    ) -> CliResult<pb::PullChangesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_sync::SyncImpl { state }
                    .pull_changes(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut sync = pb::sync_service_client::SyncServiceClient::new(channel);
                    sync.pull_changes(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// SandboxService
// ---------------------------------------------------------------------------

impl Service {
    pub fn create_sandbox_tree(
        &self,
        request: pb::CreateSandboxTreeRequest,
    ) -> CliResult<pb::CreateSandboxTreeResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_sandbox::SandboxImpl { state }
                    .create_sandbox_tree(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut sandbox =
                        pb::sandbox_service_client::SandboxServiceClient::new(channel);
                    sandbox
                        .create_sandbox_tree(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn stage_sandbox_image(
        &self,
        request: pb::StageSandboxImageRequest,
    ) -> CliResult<pb::StageSandboxImageResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_sandbox::SandboxImpl { state }
                    .stage_sandbox_image(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut sandbox =
                        pb::sandbox_service_client::SandboxServiceClient::new(channel);
                    sandbox
                        .stage_sandbox_image(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }

    pub fn seal_sandbox_image(
        &self,
        request: pb::SealSandboxImageRequest,
    ) -> CliResult<pb::SealSandboxImageResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_sandbox::SandboxImpl { state }
                    .seal_sandbox_image(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut sandbox =
                        pb::sandbox_service_client::SandboxServiceClient::new(channel);
                    sandbox
                        .seal_sandbox_image(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// TriageService
// ---------------------------------------------------------------------------

impl Service {
    pub fn list_triage_candidates(
        &self,
        request: pb::ListTriageCandidatesRequest,
    ) -> CliResult<pb::ListTriageCandidatesResponse> {
        self.call(move |backend| async move {
            match backend {
                Backend::Local(state) => services_triage::TriageImpl { state }
                    .list_triage_candidates(Request::new(request))
                    .await
                    .map(|response| response.into_inner()),
                Backend::Reactor(channel) => {
                    let mut triage = pb::triage_service_client::TriageServiceClient::new(channel);
                    triage
                        .list_triage_candidates(request)
                        .await
                        .map(|response| response.into_inner())
                }
            }
        })
    }
}

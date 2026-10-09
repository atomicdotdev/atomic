//! The full-turn service handlers: the domain half of the service layer.
//!
//! Each RPC is a complete domain operation (RFC D5): it opens its own
//! server-side transaction through the repository handle, never exposes
//! tables or transactions over the wire, and keeps presentation out of the
//! daemon (D6). Handlers run the domain logic on the blocking pool so redb
//! work never stalls the async transport.

use std::sync::Arc;

use crate::atomic::ErrorCode;
use crate::atomic::*;
use atomic_core::change::Author;
use atomic_core::change::ChangeHeader;
use atomic_core::pristine::ViewTxnT;
use atomic_identity::IdentityStore;
use atomic_repository::apply::{CrossViewInsertOptions, InsertOptions};
use atomic_repository::record::RecordOptions;
use atomic_repository::status::StatusOptions;
use atomic_repository::tracking::TrackingOptions;
use atomic_repository::{HistoryOptions, Repository};
use tonic::{Request, Response, Status};

use super::convert::*;
use super::state::{domain_status, repository_error, DaemonState};

// ---------------------------------------------------------------------------
// DaemonService
// ---------------------------------------------------------------------------

pub struct DaemonImpl {
    pub state: Arc<DaemonState>,
}

const METHOD_MATRIX: &[(&str, &str)] = &[
    ("DaemonService", "Health"),
    ("DaemonService", "GetCapabilities"),
    ("DaemonService", "ResolveRepository"),
    ("DaemonService", "RegisterRepository"),
    ("DaemonService", "ListRepositories"),
    ("DaemonService", "Shutdown"),
    ("RepositoryQueryService", "Status"),
    ("RepositoryQueryService", "Log"),
    ("RepositoryQueryService", "ListConflicts"),
    ("RepositoryQueryService", "PreviewMutation"),
    ("RepositoryQueryService", "PreviewRestore"),
    ("RepositoryQueryService", "Diff"),
    ("RepositoryMutationService", "AddFiles"),
    ("RepositoryMutationService", "Record"),
    ("RepositoryMutationService", "RemoveFiles"),
    ("RepositoryMutationService", "MoveFile"),
    ("RepositoryMutationService", "Unrecord"),
    ("RepositoryMutationService", "Restore"),
    ("RepositoryMutationService", "CreateStash"),
    ("RepositoryMutationService", "ApplyStash"),
    ("RepositoryMutationService", "ListStashes"),
    ("RepositoryMutationService", "DropStash"),
    ("RepositoryMutationService", "InsertChanges"),
    ("RepositoryMutationService", "Revise"),
    ("VaultService", "CreateVaultEntity"),
    ("VaultService", "UpdateVaultEntity"),
    ("VaultService", "GetVaultEntry"),
    ("VaultService", "ListVaultEntries"),
    ("VaultService", "ValidateVaultEntity"),
    ("VaultService", "SyncVault"),
    ("AttestationService", "RecordAttestation"),
    ("AttestationService", "VerifyAttestation"),
    ("KnowledgeService", "QueryGraph"),
    ("ProvenanceService", "DispatchTurnEvent"),
    ("ProvenanceService", "ReserveTurn"),
    ("ProvenanceService", "AppendEnvelopes"),
    ("ProvenanceService", "PrepareCheckpoint"),
    ("ProvenanceService", "LoadFrozenEnvelopes"),
    ("ProvenanceService", "BindCheckpointHash"),
    ("ProvenanceService", "AcknowledgeCheckpoint"),
    ("ProvenanceService", "UpdateTurn"),
    ("ProvenanceService", "GetTurn"),
    ("ProvenanceService", "GetSession"),
    ("ProvenanceService", "ListSessions"),
    ("MaintenanceService", "Repair"),
    ("MaintenanceService", "CompactDatabase"),
    ("MaintenanceService", "CheckRepository"),
    ("ViewService", "ListViews"),
    ("ViewService", "CreateView"),
    ("ViewService", "SwitchView"),
    ("ViewService", "DeleteView"),
    ("ViewService", "SetViewScope"),
    ("ViewService", "SplitView"),
];

#[tonic::async_trait]
impl daemon_service_server::DaemonService for DaemonImpl {
    async fn health(
        &self,
        _request: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        Ok(Response::new(HealthResponse {
            status: ServingStatus::Serving as i32,
            pid: std::process::id(),
            features: vec![
                "full-turn-slice".to_string(),
                "repository-registry".to_string(),
            ],
        }))
    }

    async fn get_capabilities(
        &self,
        _request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<GetCapabilitiesResponse>, Status> {
        let methods = METHOD_MATRIX
            .iter()
            .map(|(service, method)| {
                if *service == "MaintenanceService" && *method == "CompactDatabase" {
                    MethodDescriptorInfo {
                        service: service.to_string(),
                        method: method.to_string(),
                        scope: ExecutionScope::Local as i32,
                        required_capabilities: vec!["maintenance.admin".to_string()],
                        allowed_callers: vec![CallerClass::Local as i32],
                        effects: vec![RpcEffect::RepositoryWrite as i32],
                    }
                } else {
                    MethodDescriptorInfo {
                        service: service.to_string(),
                        method: method.to_string(),
                        scope: ExecutionScope::Both as i32,
                        required_capabilities: Vec::new(),
                        allowed_callers: Vec::new(),
                        effects: Vec::new(),
                    }
                }
            })
            .collect();
        Ok(Response::new(GetCapabilitiesResponse {
            protocol_version: 2u32,
            implementation_version: env!("CARGO_PKG_VERSION").to_string(),
            endpoint_kind: EndpointKind::LocalDaemon as i32,
            methods,
            features: vec!["full-turn-slice".to_string()],
            limits: Some(Limits {
                max_message_bytes: 64 * 1024 * 1024,
                max_frozen_page_fragments: 256,
                frozen_page_bytes: 1024 * 1024,
                append_chunk_budget_bytes: 4 * 1024 * 1024,
                max_changes_per_request: 64,
                max_inodes_per_request: 1024,
                max_write_queue_depth: 64,
            }),
            auth: Some(AuthRequirements {
                tls_required: false,
                audience_binding: false,
                identity_schemes: vec!["did:atomic".to_string()],
                credential_schemes: vec!["local-socket-uid".to_string()],
            }),
        }))
    }

    async fn resolve_repository(
        &self,
        request: Request<ResolveRepositoryRequest>,
    ) -> Result<Response<ResolveRepositoryResponse>, Status> {
        let request = request.into_inner();
        let root = Repository::find_root(std::path::Path::new(&request.path))
            .map_err(|error| domain_status(ErrorCode::RepositoryNotFound, error.to_string()))?;
        let handle = self.state.register(root.clone());
        self.state.log_rpc("ResolveRepository", Some(&handle));
        // Resolve is a pure path walk + registry entry — it never opens a
        // repository database, so it must NOT queue behind the per-repo
        // serialization gate (a CLI command resolving while a turn records
        // would otherwise block for the record's whole duration).
        let relative_cwd = std::path::Path::new(&request.path)
            .strip_prefix(&handle.root)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Ok(Response::new(ResolveRepositoryResponse {
            repository: Some(handle.repository_ref()),
            workspace: Some(WorkspaceInfo {
                workspace_id: "default".to_string(),
                root_path: handle.root.display().to_string(),
                relative_cwd,
                current_view: Some(ViewRef {
                    view_id: blake3::hash(handle.current_view().as_bytes())
                        .as_bytes()
                        .to_vec(),
                    name: Some(handle.current_view()),
                }),
            }),
        }))
    }

    async fn register_repository(
        &self,
        request: Request<RegisterRepositoryRequest>,
    ) -> Result<Response<RegisterRepositoryResponse>, Status> {
        let request = request.into_inner();
        let meta = request.meta.unwrap_or_default();
        let root = Repository::find_root(std::path::Path::new(&request.path))
            .map_err(|error| domain_status(ErrorCode::RepositoryNotFound, error.to_string()))?;
        let handle = self.state.register(root);
        self.state.log_rpc("RegisterRepository", Some(&handle));
        // Registry-only: no repository database is opened (contract:
        // "without opening a repository write transaction").
        Ok(Response::new(RegisterRepositoryResponse {
            repository: Some(handle.repository_ref()),
            workspace: None,
            meta: Some(ResponseMeta {
                request_id: meta.request_id,
                replayed: false,
                snapshot: None,
            }),
        }))
    }

    async fn list_repositories(
        &self,
        _request: Request<ListRepositoriesRequest>,
    ) -> Result<Response<ListRepositoriesResponse>, Status> {
        let repositories = self
            .state
            .list()
            .into_iter()
            .map(|handle| RepositorySummary {
                repository: Some(handle.repository_ref()),
                open: true,
                local_path: Some(handle.root.display().to_string()),
            })
            .collect();
        Ok(Response::new(ListRepositoriesResponse { repositories }))
    }

    async fn shutdown(
        &self,
        request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        let request = request.into_inner();
        let meta = request.meta.unwrap_or_default();
        let state = self.state.clone();
        state.shutdown.notify_one();
        Ok(Response::new(ShutdownResponse {
            pid: std::process::id(),
            meta: Some(ResponseMeta {
                request_id: meta.request_id,
                replayed: false,
                snapshot: None,
            }),
        }))
    }
}

// ---------------------------------------------------------------------------
// RepositoryQueryService
// ---------------------------------------------------------------------------

pub struct QueryImpl {
    pub state: Arc<DaemonState>,
}

#[tonic::async_trait]
impl repository_query_service_server::RepositoryQueryService for QueryImpl {
    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("Status", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || {
            // The service query remains observation-only; the CLI's separate
            // Git coordinator performs ordinary reconciliation before calling.
            let (repo, workspace) =
                handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Observe)?;
            let working_copy = workspace.working_copy();
            let status = repo
                .status(working_copy, StatusOptions::all())
                .map_err(repository_error)?;
            let files = status
                .entries()
                .iter()
                .map(|entry| FileStatusEntry {
                    path: entry.path().display().to_string(),
                    status: file_status_proto(entry.status()) as i32,
                    inode: None,
                    recorded_hash: entry.recorded_hash().map(hash_proto),
                    current_hash: entry.current_hash().map(hash_proto),
                    details: super::convert::file_status_details(entry),
                })
                .collect();
            Ok::<_, Status>(StatusResponse {
                view: status.view().to_string(),
                view_merkle: status.state().map(hash_proto),
                needs_reindex: status.needs_reindex(),
                files,
                modified_count: status.modified_count() as u32,
                deleted_count: status.deleted_count() as u32,
                untracked_count: status.untracked_count() as u32,
                added_count: status.added_count() as u32,
                conflicted_count: status.conflicted_count() as u32,
                stale_index_count: status.stale_index_count() as u32,
                snapshot: None,
            })
        })
        .await
        .map_err(|error| Status::internal(format!("status task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn log(&self, request: Request<LogRequest>) -> Result<Response<LogResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("Log", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let view = if request.all_views {
            None
        } else {
            Some(
                request
                    .view
                    .clone()
                    .unwrap_or_else(|| handle.current_view()),
            )
        };
        let include_inherited = request.all_views;
        let tags_only = request.tags_only;
        let path_filter = request.path_filter.clone();
        let from_sequence = request.cursor.as_ref().map_or(0, |c| c.seq);
        let limit = request
            .budget
            .as_ref()
            .and_then(|b| b.max_items)
            .map(|max| max as usize);
        let result = tokio::task::spawn_blocking(move || {
            // A query: read-only open with the read-concurrency wait.
            let repo = handle.repository_readonly()?;
            let mut options = HistoryOptions::with_headers().from_sequence(from_sequence);
            if let Some(view) = &view {
                options = options.view(view.clone());
            }
            if include_inherited {
                options = options.include_inherited(true);
            }
            if tags_only {
                options = options.tagged_only(true);
            }
            if let Some(limit) = limit {
                options = options.limit(limit);
            }
            let entries = repo.reverse_log(options).map_err(repository_error)?;
            // `--path`: keep the entries whose changes touch the path —
            // change content is loaded here (server-side), not per client.
            let entries = if path_filter.is_empty() {
                entries
            } else {
                entries
                    .into_iter()
                    .filter(|entry| {
                        if entry.is_tagged {
                            return false; // tag rows carry no hunks
                        }
                        repo.load_change(&entry.hash)
                            .map(|change| {
                                change.hashed.hunks.iter().any(|op| {
                                    op.path().is_some_and(|p| {
                                        path_filter.iter().any(|filter| {
                                            p == filter || p.starts_with(&format!("{filter}/"))
                                        })
                                    })
                                })
                            })
                            .unwrap_or(false)
                    })
                    .collect()
            };
            let entries = entries
                .into_iter()
                .map(|entry| ChangeLogEntry {
                    hash: Some(hash_proto(&entry.hash)),
                    sequence: entry.sequence,
                    state: Some(hash_proto(&entry.state)),
                    message: entry.message().map(str::to_string),
                    description: entry.description().map(str::to_string),
                    authors: entry
                        .authors()
                        .map(|authors| authors.iter().map(author_proto).collect())
                        .unwrap_or_default(),
                    recorded_at: entry.timestamp().map(timestamp_proto),
                    dependencies: Vec::new(),
                    view: view.clone(),
                    is_tagged: entry.is_tagged,
                })
                .collect();
            Ok::<_, Status>(LogResponse {
                entries,
                next_cursor: None,
            })
        })
        .await
        .map_err(|error| Status::internal(format!("log task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn get_change(
        &self,
        request: Request<GetChangeRequest>,
    ) -> Result<Response<GetChangeResponse>, Status> {
        crate::daemon::services_query::get_change_impl(&self.state, request).await
    }

    type DiffStream = tokio_stream::wrappers::ReceiverStream<Result<DiffChunk, Status>>;
    async fn diff(
        &self,
        request: Request<DiffRequest>,
    ) -> Result<Response<Self::DiffStream>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("Diff", Some(&handle));
        let refs = match request.scope {
            Some(diff_request::Scope::Refs(refs)) => refs,
            _ => {
                return Err(Status::invalid_argument(
                    "diff scope required (from/to view pair)",
                ))
            }
        };
        let stat_only = request.stat_only;
        let from_view = refs.from_view;
        let to_view = refs.to_view;

        // Streaming read transaction: read-only open, no gate — never
        // queued behind writers (the preview pattern).
        let (sender, receiver) = tokio::sync::mpsc::channel::<Result<DiffChunk, Status>>(16);
        let root = handle.root.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                compute_ref_pair_diff(&root, &from_view, &to_view, stat_only)
            })
            .await
            .unwrap_or_else(|error| Err(Status::internal(error.to_string())));
            match result {
                Ok(chunks) => {
                    for chunk in chunks {
                        if sender.send(Ok(chunk)).await.is_err() {
                            break; // client went away
                        }
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                }
            }
        });
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            receiver,
        )))
    }

    type DiffWorkingCopyStream =
        std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<DiffChunk, Status>> + Send>>;
    async fn diff_working_copy(
        &self,
        request: Request<DiffWorkingCopyRequest>,
    ) -> Result<Response<Self::DiffWorkingCopyStream>, Status> {
        let response =
            crate::daemon::services_query::diff_working_copy_impl(&self.state, request).await?;
        Ok(Response::new(response.into_inner()))
    }

    async fn list_conflicts(
        &self,
        request: Request<ListConflictsRequest>,
    ) -> Result<Response<ListConflictsResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("ListConflicts", Some(&handle));
        // Read transaction: read-only open (with the read-concurrency
        // wait), never queued behind writers on the gate.
        let result =
            tokio::task::spawn_blocking(move || -> Result<ListConflictsResponse, Status> {
                let (repo, workspace) =
                    handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Observe)?;
                let working_copy = workspace.working_copy();
                let conflicts = repo
                    .list_conflicts(working_copy)
                    .map_err(repository_error)?;
                let mut infos = Vec::new();
                for (path, records) in &conflicts {
                    for record in records {
                        infos.push(ConflictInfo {
                            path: path.clone(),
                            view: None,
                            base: None,
                            kind: Some(format!("{:?}", record.kind).to_lowercase()),
                            line: record.line.map(|line| line as u64),
                            sides: record.sides.clone(),
                        });
                    }
                }
                Ok(ListConflictsResponse { conflicts: infos })
            })
            .await
            .map_err(|error| Status::internal(format!("conflicts task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn preview_mutation(
        &self,
        request: Request<PreviewMutationRequest>,
    ) -> Result<Response<PreviewMutationResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("PreviewMutation", Some(&handle));
        // Read transaction: read-only open, never queued behind writers.
        // The unrecord preview opens its own WRITABLE handle below (the
        // domain's dry-run rides a write transaction, and the gate is
        // held for this request).
        let root = handle.root.clone();
        let current_view = handle.current_view();
        let unrecord_handle = handle.clone();
        let result =
            tokio::task::spawn_blocking(move || -> Result<PreviewMutationResponse, Status> {
                match request.operation {
                    Some(preview_mutation_request::Operation::Insert(preview)) => {
                        // The would-insert listing: missing changes between
                        // the source and the target (tag cutoff through the
                        // domain dry-run). Read-only open, never queued
                        // behind writers.
                        let repo = Repository::open_readonly(&root).map_err(|error| {
                            domain_status(
                                ErrorCode::Repository,
                                format!("failed to open repository read-only: {error}"),
                            )
                        })?;
                        let target = if preview.target_view.is_empty() {
                            current_view
                        } else {
                            preview.target_view.clone()
                        };
                        let missing = match preview.source {
                            Some(insert_preview::Source::FromView(source)) => repo
                                .get_missing_changes_between(&source, Some(&target))
                                .map_err(repository_error)?,
                            Some(insert_preview::Source::UpToTag(tag)) => {
                                let options = CrossViewInsertOptions::new(&tag, &target)
                                    .up_to_tag(&tag)
                                    .dry_run(true);
                                repo.insert_from_view(options)
                                    .map_err(repository_error)?
                                    .applied_hashes
                            }
                            Some(insert_preview::Source::SingleChange(_)) | None => {
                                return Err(domain_status(
                                    ErrorCode::InvalidArgument,
                                    "insert preview takes a from-view or up-to-tag",
                                ))
                            }
                        };
                        let mut changes = Vec::with_capacity(missing.len());
                        for hash in &missing {
                            let change = repo.load_change(hash).map_err(|error| {
                                domain_status(ErrorCode::Repository, error.to_string())
                            })?;
                            changes.push(change_info(&change, hash));
                        }
                        Ok(PreviewMutationResponse {
                            affected_paths: Vec::new(),
                            changes,
                            conflicts: Vec::new(),
                            snapshot: None,
                            untracked: Vec::new(),
                            pristine_content: None,
                        })
                    }
                    Some(preview_mutation_request::Operation::Unrecord(preview)) => {
                        // The would-unrecord listing: the domain dry-run
                        // resolves the target (or the view's last change)
                        // and its cascade without mutating anything.
                        let view = preview
                            .view
                            .and_then(|v| v.name)
                            .filter(|name| !name.is_empty())
                            .unwrap_or_else(|| current_view.clone());
                        let repo = unrecord_handle.repository()?;
                        let target_hash = match preview.target.and_then(|target| target.kind) {
                            Some(change_ref::Kind::Hash(hash)) => {
                                let mut bytes = [0u8; 32];
                                if hash.value.len() != 32 {
                                    return Err(domain_status(
                                        ErrorCode::InvalidArgument,
                                        "hash must be 32 bytes",
                                    ));
                                }
                                bytes.copy_from_slice(&hash.value);
                                Some(atomic_core::types::Merkle(bytes))
                            }
                            // Case-insensitive unique-prefix resolution —
                            // same resolver as the mutating twin.
                            Some(change_ref::Kind::Prefix(prefix)) => {
                                let prefix = prefix.trim().to_ascii_uppercase();
                                match repo.find_change_by_prefix(&prefix) {
                                    Ok(Some(hash)) => Some(hash),
                                    Ok(None) => {
                                        return Err(domain_status(
                                            ErrorCode::NotFound,
                                            format!("no change found matching '{prefix}'"),
                                        ))
                                    }
                                    Err(atomic_repository::RepositoryError::AmbiguousHash {
                                        prefix,
                                        matches,
                                    }) => {
                                        return Err(domain_status(
                                            ErrorCode::NotFound,
                                            format!(
                                                "ambiguous change prefix '{prefix}' (matches: {})",
                                                matches.join(", ")
                                            ),
                                        ))
                                    }
                                    Err(error) => return Err(repository_error(error)),
                                }
                            }
                            Some(change_ref::Kind::Sequence(sequence)) => {
                                let txn = repo.pristine().read_txn().map_err(|error| {
                                    domain_status(ErrorCode::Repository, error.to_string())
                                })?;
                                let view_state = txn
                                    .get_view(&view)
                                    .map_err(|error| {
                                        domain_status(ErrorCode::View, error.to_string())
                                    })?
                                    .ok_or_else(|| {
                                        domain_status(
                                            ErrorCode::View,
                                            format!("view '{view}' not found"),
                                        )
                                    })?;
                                Some(
                                    atomic_repository::history::get_change_at_sequence(
                                        &txn,
                                        &view_state,
                                        sequence,
                                    )
                                    .map_err(|error| {
                                        domain_status(ErrorCode::NotFound, error.to_string())
                                    })?
                                    .hash,
                                )
                            }
                            None => None,
                        };
                        // The would-unrecord preview is the SAME domain call
                        // as the mutating unrecord's dry_run — one guard
                        // surface (membership, dependents, emptiness), not
                        // a second read-path approximation of it. The
                        // domain's dry-run opens a write transaction, so
                        // this arm uses the request's writable handle (the
                        // gate is held) for BOTH the target resolution and
                        // the preview.
                        let options = atomic_repository::UnrecordOptions::dry_run().view(view);
                        let outcome = match target_hash {
                            Some(hash) => repo.unrecord(&hash, options).map_err(|error| {
                                domain_status(ErrorCode::PreconditionFailed, error.to_string())
                            })?,
                            None => repo.unrecord_last(options).map_err(|error| {
                                domain_status(ErrorCode::PreconditionFailed, error.to_string())
                            })?,
                        };
                        let mut changes = Vec::with_capacity(outcome.unrecorded.len());
                        for hash in &outcome.unrecorded {
                            let change = repo.load_change(hash).map_err(|error| {
                                domain_status(ErrorCode::Repository, error.to_string())
                            })?;
                            changes.push(change_info(&change, hash));
                        }
                        Ok(PreviewMutationResponse {
                            affected_paths: Vec::new(),
                            changes,
                            conflicts: Vec::new(),
                            snapshot: None,
                            untracked: Vec::new(),
                            pristine_content: None,
                        })
                    }
                    None => Err(domain_status(
                        ErrorCode::InvalidArgument,
                        "preview operation required (insert or unrecord)",
                    )),
                }
            })
            .await
            .map_err(|error| Status::internal(format!("preview task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn preview_restore(
        &self,
        request: Request<PreviewRestoreRequest>,
    ) -> Result<Response<PreviewMutationResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("PreviewRestore", Some(&handle));
        // Read transaction: read-only open, never queued behind writers.
        let root = handle.root.clone();
        let result =
            tokio::task::spawn_blocking(move || -> Result<PreviewMutationResponse, Status> {
                let mut repo = Repository::open_readonly(&root).map_err(|error| {
                    domain_status(
                        ErrorCode::Repository,
                        format!("failed to open repository read-only: {error}"),
                    )
                })?;
                let workspace = super::state::enter_workspace(
                    &mut repo,
                    atomic_repository::WorkspaceTxnMode::Observe,
                )?;
                let working_copy = workspace.working_copy();
                // The Restore classification WITHOUT mutation: tracked-only
                // status pass; Added files would be untracked (kept on
                // disk), Modified/Deleted would be restored.
                let status = repo
                    .status(
                        working_copy,
                        StatusOptions {
                            include_untracked: false,
                            ..StatusOptions::default()
                        },
                    )
                    .map_err(repository_error)?;
                let mut affected: Vec<String> = Vec::new();
                let mut untracked: Vec<String> = Vec::new();
                for entry in status.entries() {
                    let path = entry.path().to_string_lossy().to_string();
                    if !request.paths.is_empty()
                        && !request.paths.iter().any(|p| path.starts_with(p.as_str()))
                    {
                        continue;
                    }
                    match entry.status() {
                        atomic_repository::status::FileStatus::Added => untracked.push(path),
                        atomic_repository::status::FileStatus::Modified
                        | atomic_repository::status::FileStatus::Deleted => affected.push(path),
                        _ => {}
                    }
                }
                // The single-file dry-run arm (add-only): the pristine
                // bytes `restore --dry-run <file>` dumps to stdout — the
                // SAME domain read the local body makes. Absent content is
                // the CLI's not-found error, not a failure here.
                let pristine_content = match request.content_path {
                    Some(ref path) if !path.is_empty() => repo
                        .get_file_content(std::path::Path::new(path))
                        .map_err(repository_error)?,
                    _ => None,
                };
                Ok(PreviewMutationResponse {
                    affected_paths: affected,
                    changes: Vec::new(),
                    conflicts: Vec::new(),
                    snapshot: None,
                    untracked,
                    pristine_content,
                })
            })
            .await
            .map_err(|error| Status::internal(format!("preview task failed: {error}")))??;
        Ok(Response::new(result))
    }
}

/// The view-to-view diff: a pure read (read-only repository open) over
/// each side's visible file set, unified-diffed with `similar` — the
/// same hunk computation the working-copy diff performs.
fn compute_ref_pair_diff(
    root: &std::path::Path,
    from_view: &str,
    to_view: &str,
    stat_only: bool,
) -> Result<Vec<DiffChunk>, Status> {
    let repo = Repository::open_readonly_wait(root, super::state::database_open_wait()).map_err(
        |error| {
            domain_status(
                ErrorCode::Repository,
                format!("failed to open repository read-only: {error}"),
            )
        },
    )?;
    for view in [from_view, to_view] {
        if !repo.view_exists(view).map_err(repository_error)? {
            return Err(domain_status(
                ErrorCode::View,
                format!("view '{view}' not found"),
            ));
        }
    }
    let from_files = repo
        .visible_file_paths(from_view)
        .map_err(repository_error)?;
    let to_files = repo.visible_file_paths(to_view).map_err(repository_error)?;
    // Symmetric difference (present on exactly one side) plus the common
    // paths whose content actually differs.
    let mut all_paths: Vec<String> = from_files
        .symmetric_difference(&to_files)
        .cloned()
        .collect();
    for path in from_files.intersection(&to_files) {
        let differs = match (
            repo.get_file_content_on_view(path, from_view),
            repo.get_file_content_on_view(path, to_view),
        ) {
            (Ok(Some(a)), Ok(Some(b))) => a != b,
            _ => true,
        };
        if differs {
            all_paths.push(path.clone());
        }
    }
    all_paths.sort();

    let mut chunks = Vec::new();
    for path in all_paths {
        let in_from = from_files.contains(&path);
        let in_to = to_files.contains(&path);
        let status = match (in_from, in_to) {
            (true, true) => "modified",
            (false, true) => "added",
            (true, false) => "deleted",
            (false, false) => continue,
        };
        let old = repo
            .get_file_content_on_view(&path, from_view)
            .map_err(repository_error)?
            .unwrap_or_default();
        let new = repo
            .get_file_content_on_view(&path, to_view)
            .map_err(repository_error)?;
        let old_binary = old.contains(&0);
        let new_binary = new.as_ref().map(|c| c.contains(&0)).unwrap_or(false);
        if old_binary || new_binary {
            chunks.push(DiffChunk {
                path: path.clone(),
                patch: None,
                additions: 0,
                deletions: 0,
                binary: true,
                status: Some(status.to_string()),
                old_content: None,
                new_content: None,
            });
            continue;
        }
        let old_text = String::from_utf8_lossy(&old).into_owned();
        let new_text = new
            .as_ref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
        let diff = similar::TextDiff::from_lines(&old_text, &new_text);
        let mut additions = 0u64;
        let mut deletions = 0u64;
        for change in diff.iter_all_changes() {
            match change.tag() {
                similar::ChangeTag::Insert => additions += 1,
                similar::ChangeTag::Delete => deletions += 1,
                similar::ChangeTag::Equal => {}
            }
        }
        let patch = if stat_only {
            None
        } else {
            Some(
                similar::TextDiff::from_lines(&old_text, &new_text)
                    .unified_diff()
                    .context_radius(3)
                    .to_string()
                    .into_bytes(),
            )
        };
        let (old_payload, new_payload) = if stat_only {
            (None, None)
        } else {
            (Some(old.clone()), new.clone())
        };
        chunks.push(DiffChunk {
            path,
            patch,
            additions,
            deletions,
            binary: false,
            status: Some(status.to_string()),
            old_content: old_payload,
            new_content: new_payload,
        });
    }
    Ok(chunks)
}

/// Minimal wire render of a stored change (hash, message, authors,
/// timestamp) — the record-response shape.
fn change_info(
    change: &atomic_core::change::Change,
    hash: &atomic_core::types::Merkle,
) -> ChangeInfo {
    ChangeInfo {
        hash: Some(hash_proto(hash)),
        message: Some(change.hashed.header.message.clone()),
        description: change.hashed.header.description.clone(),
        authors: change
            .hashed
            .header
            .authors
            .iter()
            .map(author_proto)
            .collect(),
        recorded_at: Some(timestamp_proto(change.hashed.header.timestamp)),
        dependencies: Vec::new(),
        graph_section_count: 0,
        semantic_section_count: 0,
        content_chunk_count: 0,
        has_provenance: change.has_provenance(),
        has_unhashed: change.unhashed.is_some(),
        has_signature: change.signature.is_some(),
    }
}

/// The hash-only report entry for a change the listing could not load —
/// the local previews print the hash alone in that case, never an error.
fn bare_change_info(hash: &atomic_core::types::Merkle) -> ChangeInfo {
    ChangeInfo {
        hash: Some(hash_proto(hash)),
        message: None,
        description: None,
        authors: Vec::new(),
        recorded_at: None,
        dependencies: Vec::new(),
        graph_section_count: 0,
        semantic_section_count: 0,
        content_chunk_count: 0,
        has_provenance: false,
        has_unhashed: false,
        has_signature: false,
    }
}

/// A wire Hash → the domain Merkle, refusing a malformed value.
fn wire_hash(hash: &crate::atomic::Hash) -> Result<atomic_core::types::Merkle, Status> {
    let mut bytes = [0u8; 32];
    if hash.value.len() != 32 {
        return Err(domain_status(
            ErrorCode::InvalidArgument,
            "hash must be 32 bytes",
        ));
    }
    bytes.copy_from_slice(&hash.value);
    Ok(atomic_core::types::Merkle(bytes))
}

/// Resolve one raw change reference with the local insert bodies'
/// semantics: a full base32 hash parses directly; otherwise a
/// case-insensitive unique prefix over the change store, refusing with
/// the local messages on no match / ambiguity.
fn resolve_change_reference(
    repo: &Repository,
    reference: &str,
) -> Result<atomic_core::types::Merkle, Status> {
    use atomic_core::types::Base32;
    if let Some(hash) = atomic_core::types::Merkle::from_base32(reference.as_bytes()) {
        return Ok(hash);
    }
    if reference.len() >= 2 {
        match repo.find_change_by_prefix(reference) {
            Ok(Some(hash)) => return Ok(hash),
            Ok(None) => {}
            Err(atomic_repository::RepositoryError::AmbiguousHash { .. }) => {
                return Err(domain_status(
                    ErrorCode::NotFound,
                    format!("Ambiguous change hash '{reference}' - matches multiple changes"),
                ));
            }
            Err(error) => return Err(repository_error(error)),
        }
    }
    Err(domain_status(
        ErrorCode::NotFound,
        format!("Change not found: {reference}"),
    ))
}

/// The single-insert domain calls the local single-insert body makes:
/// the dependency closure by default, `--deps=false` skips it (the
/// plain insert path).
fn single_insert(
    repo: &Repository,
    hash: &atomic_core::types::Merkle,
    target: &str,
    apply_dependencies: bool,
    allow_conflicts: bool,
) -> Result<atomic_repository::InsertOutcome, atomic_repository::RepositoryError> {
    let options = InsertOptions::default()
        .apply_deps(apply_dependencies)
        .allow_conflict(allow_conflicts)
        .view(target.to_string());
    if apply_dependencies {
        repo.insert_change_rec(hash, options)
    } else {
        repo.insert_change(hash, options)
    }
}

pub(crate) fn default_ref() -> RepositoryRef {
    RepositoryRef {
        authority: String::new(),
        repository_id: Vec::new(),
        workspace_id: None,
    }
}

// ---------------------------------------------------------------------------
// RepositoryMutationService
// ---------------------------------------------------------------------------

pub struct MutationImpl {
    pub state: Arc<DaemonState>,
}

fn resolve_author() -> Result<Option<Author>, String> {
    let store = IdentityStore::open_default().map_err(|e| e.to_string())?;
    let Some(identity) = store.get_default().map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    Ok(Some(Author::with_identity(
        identity.name.clone(),
        identity.email.clone(),
        identity.public_key_base32(),
    )))
}

/// Authorship for a record: an explicit wire author wins; else a named
/// identity from the serving host's store; else the default identity.
fn resolve_author_for(
    author: Option<&crate::atomic::Author>,
    identity_name: Option<&str>,
) -> Result<Option<Author>, String> {
    if let Some(author) = author {
        return Ok(Some(Author {
            name: author.name.clone(),
            email: author.email.clone(),
            identity: None,
        }));
    }
    if let Some(name) = identity_name {
        let store = IdentityStore::open_default().map_err(|e| e.to_string())?;
        let identity = store.load_by_name(name).map_err(|e| e.to_string())?;
        return Ok(Some(Author::with_identity(
            identity.name.clone(),
            identity.email.clone(),
            identity.public_key_base32(),
        )));
    }
    resolve_author()
}

#[tonic::async_trait]
impl repository_mutation_service_server::RepositoryMutationService for MutationImpl {
    async fn add_files(
        &self,
        request: Request<AddFilesRequest>,
    ) -> Result<Response<AddFilesResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("AddFiles", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let paths: Vec<String> = request.paths;
        let meta = request.meta.unwrap_or_default();
        let options = TrackingOptions {
            recursive: !request.no_recursive,
            force: request.force,
            dry_run: request.dry_run,
            ..TrackingOptions::default()
        };
        let directory = request.directory;
        let dry_run = request.dry_run;
        let result = tokio::task::spawn_blocking(move || {
            let (repo, workspace) = handle.workspace_repository(if dry_run {
                atomic_repository::WorkspaceTxnMode::Observe
            } else {
                atomic_repository::WorkspaceTxnMode::Reconcile
            })?;
            let working_copy = workspace.working_copy();
            let mut tracked = 0usize;
            // The tolerant per-path report (dry-run or not): skips never
            // fail the batch; failures are collected per path.
            let mut would_add: Vec<String> = Vec::new();
            let mut skipped_tracked: Vec<String> = Vec::new();
            let mut skipped_ignored: Vec<String> = Vec::new();
            let mut failed_paths: Vec<String> = Vec::new();
            for path in &paths {
                let result = if directory {
                    repo.add_directory(working_copy, path, options.clone())
                } else {
                    repo.add(working_copy, path, options.clone())
                };
                match result {
                    Ok(stats) => {
                        if dry_run {
                            if stats.total_added() > 0 {
                                would_add.push(path.clone());
                            }
                            for (skipped, reason) in &stats.skipped_paths {
                                if reason.contains("tracked") {
                                    skipped_tracked.push(skipped.to_string_lossy().into_owned());
                                } else if reason.contains("ignored") {
                                    skipped_ignored.push(skipped.to_string_lossy().into_owned());
                                }
                            }
                        } else {
                            tracked += stats.files_added;
                        }
                    }
                    Err(atomic_repository::RepositoryError::FileAlreadyTracked { path }) => {
                        skipped_tracked.push(path.to_string_lossy().into_owned());
                    }
                    Err(atomic_repository::RepositoryError::PathIgnored { path }) => {
                        if !options.force {
                            skipped_ignored.push(path.to_string_lossy().into_owned());
                        }
                    }
                    Err(_) if dry_run => {
                        failed_paths.push(path.clone());
                    }
                    Err(error) => {
                        return Err(repository_error(error));
                    }
                }
            }
            Ok::<_, Status>(AddFilesResponse {
                tracked: tracked as u32,
                would_add,
                skipped_tracked,
                skipped_ignored,
                failed_paths,
                dry_run,
                meta: Some(ResponseMeta {
                    request_id: meta.request_id,
                    replayed: false,
                    snapshot: None,
                }),
            })
        })
        .await
        .map_err(|error| Status::internal(format!("add task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn record(
        &self,
        request: Request<RecordRequest>,
    ) -> Result<Response<RecordResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("Record", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let message = request.message;
        let paths = request.paths;
        let meta = request.meta.unwrap_or_default();
        let author = request.author;
        let identity_name = request.identity_name;
        let result = tokio::task::spawn_blocking(move || {
            if message.trim().is_empty() {
                return Err(domain_status(
                    ErrorCode::InvalidArgument,
                    "record message is empty",
                ));
            }
            let author =
                resolve_author_for(author.as_ref(), identity_name.as_deref()).map_err(|error| {
                    domain_status(
                        ErrorCode::Internal,
                        format!("identity resolution failed: {error}"),
                    )
                })?;
            let mut header = ChangeHeader::builder().message(message.clone());
            if let Some(author) = author {
                header = header.author(author);
            }
            let header = header.build();
            let mut options = RecordOptions::new().message(message.clone());
            if !paths.is_empty() {
                options = options.paths(paths.clone());
            }
            let (repo, workspace) =
                handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Reconcile)?;
            let working_copy = workspace.working_copy();
            let view = workspace.view().name.clone();
            let outcome = repo
                .record(working_copy, header, options)
                .map_err(|error| domain_status(ErrorCode::ChangeRejected, error.to_string()))?;
            let change = outcome.change();
            Ok::<_, Status>(RecordResponse {
                change: Some(ChangeInfo {
                    hash: Some(hash_proto(outcome.hash())),
                    message: Some(change.hashed.header.message.clone()),
                    description: change.hashed.header.description.clone(),
                    authors: change
                        .hashed
                        .header
                        .authors
                        .iter()
                        .map(author_proto)
                        .collect(),
                    recorded_at: Some(timestamp_proto(change.hashed.header.timestamp)),
                    dependencies: Vec::new(),
                    graph_section_count: 0,
                    semantic_section_count: 0,
                    content_chunk_count: 0,
                    has_provenance: change.has_provenance(),
                    has_unhashed: change.unhashed.is_some(),
                    has_signature: change.signature.is_some(),
                }),
                view_merkle: outcome.new_state().map(|state| hash_proto(&state)),
                view: Some(view),
                meta: Some(ResponseMeta {
                    request_id: meta.request_id,
                    replayed: false,
                    snapshot: None,
                }),
            })
        })
        .await
        .map_err(|error| Status::internal(format!("record task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn remove_files(
        &self,
        request: Request<RemoveFilesRequest>,
    ) -> Result<Response<RemoveFilesResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("RemoveFiles", Some(&handle));
        let keep = request.keep_working_copy;
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || -> Result<RemoveFilesResponse, Status> {
            let (repo, workspace) =
                handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Reconcile)?;
            let working_copy = workspace.working_copy();
            let options = TrackingOptions {
                force: request.force,
                dry_run: request.dry_run,
                ..TrackingOptions::default()
            };
            let mut removed: u32 = 0;
            let mut to_delete: Vec<std::path::PathBuf> = Vec::new();
            for path in &request.paths {
                // Normalize exactly like the CLI: absolute paths must be
                // inside the repository root and are made relative.
                let normalized = if std::path::Path::new(path).is_absolute() {
                    match std::path::Path::new(path).strip_prefix(&handle.root) {
                        Ok(rel) => rel.display().to_string(),
                        Err(_) => {
                            return Err(domain_status(
                                ErrorCode::InvalidArgument,
                                format!("path outside repository: {path}"),
                            ))
                        }
                    }
                } else {
                    path.clone()
                };
                let stats = repo
                    .remove(working_copy, &normalized, options.clone())
                    .map_err(repository_error)?;
                removed += stats.files_removed as u32;
                if !keep {
                    to_delete.push(handle.root.join(&normalized));
                }
            }
            // The CLI deletes from disk only after every tracking removal
            // succeeded; the daemon mirrors that ordering. A dry run
            // reports the untrack counts without touching disk.
            if !request.dry_run {
                for path in to_delete {
                    if path.is_file() {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            Ok(RemoveFilesResponse {
                removed,
                meta: None,
            })
        })
        .await
        .map_err(|error| Status::internal(format!("remove task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn move_file(
        &self,
        request: Request<MoveFileRequest>,
    ) -> Result<Response<MoveFileResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("MoveFile", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || -> Result<MoveFileResponse, Status> {
            let (repo, workspace) = handle.workspace_repository(if request.dry_run {
                atomic_repository::WorkspaceTxnMode::Observe
            } else {
                atomic_repository::WorkspaceTxnMode::Reconcile
            })?;
            let working_copy = workspace.working_copy();
            // The CLI's exact validation chain, with the domain's own
            // refusal vocabulary riding the stable error codes.
            if !repo.is_tracked(&request.source).map_err(repository_error)? {
                return Err(domain_status(
                    ErrorCode::NotFound,
                    format!("file not tracked: {}", request.source),
                ));
            }
            let source_path = handle.root.join(&request.source);
            if !source_path.exists() {
                return Err(domain_status(
                    ErrorCode::NotFound,
                    format!("file not found: {}", source_path.display()),
                ));
            }
            if repo
                .is_tracked(&request.destination)
                .map_err(repository_error)?
            {
                return Err(domain_status(
                    ErrorCode::InvalidArgument,
                    format!("destination already tracked: {}", request.destination),
                ));
            }
            if source_path.is_dir() {
                return Err(domain_status(
                    ErrorCode::InvalidArgument,
                    "directory moves are not yet supported; move tracked files individually",
                ));
            }
            let destination_path = handle.root.join(&request.destination);
            let destination_metadata = std::fs::symlink_metadata(&destination_path).ok();
            if destination_metadata.is_some() && !request.force {
                return Err(domain_status(
                    ErrorCode::InvalidArgument,
                    "destination exists; use --force only for untracked content",
                ));
            }
            if destination_metadata
                .as_ref()
                .is_some_and(|m| !m.file_type().is_file())
            {
                return Err(domain_status(
                    ErrorCode::InvalidArgument,
                    "destination is not a regular file and cannot be replaced safely",
                ));
            }
            if request.dry_run {
                return Ok(MoveFileResponse { meta: None });
            }
            if let Some(parent) = destination_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| domain_status(ErrorCode::Repository, e.to_string()))?;
            }
            // Keep forced bytes until both the disk move and stable-inode
            // staging succeed. Never discard them on a failed rollback.
            let backup = if destination_metadata.is_some() {
                let path = destination_path
                    .with_file_name(format!(".atomic-mv-backup-{}", uuid::Uuid::new_v4()));
                std::fs::rename(&destination_path, &path)
                    .map_err(|e| domain_status(ErrorCode::Repository, e.to_string()))?;
                Some(path)
            } else {
                None
            };
            if let Err(error) = std::fs::rename(&source_path, &destination_path) {
                if let Some(path) = &backup {
                    std::fs::rename(path, &destination_path).map_err(|rollback| {
                        domain_status(
                            ErrorCode::Repository,
                            format!(
                                "move failed: {error}; restoring {} failed: {rollback}",
                                path.display()
                            ),
                        )
                    })?;
                }
                return Err(domain_status(ErrorCode::Repository, error.to_string()));
            }
            if let Err(error) = repo.move_file(working_copy, &request.source, &request.destination)
            {
                std::fs::rename(&destination_path, &source_path).map_err(|rollback| {
                    domain_status(
                        ErrorCode::Repository,
                        format!(
                            "move staging failed: {error}; restoring source failed: {rollback}"
                        ),
                    )
                })?;
                if let Some(path) = &backup {
                    std::fs::rename(path, &destination_path).map_err(|rollback| {
                        domain_status(
                            ErrorCode::Repository,
                            format!(
                                "move staging failed: {error}; restoring {} failed: {rollback}",
                                path.display()
                            ),
                        )
                    })?;
                }
                return Err(repository_error(error));
            }
            if let Some(path) = backup {
                std::fs::remove_file(&path).map_err(|error| {
                    domain_status(
                        ErrorCode::Repository,
                        format!(
                            "move succeeded; backup {} could not be removed: {error}",
                            path.display()
                        ),
                    )
                })?;
            }
            Ok(MoveFileResponse { meta: None })
        })
        .await
        .map_err(|error| Status::internal(format!("move task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn restore(
        &self,
        request: Request<RestoreRequest>,
    ) -> Result<Response<RestoreResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("Restore", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || -> Result<RestoreResponse, Status> {
            let (repo, workspace) =
                handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Reconcile)?;
            let working_copy = workspace.working_copy();
            // Restore only ever touches TRACKED dirty files — the
            // untracked scan is skipped (the CLI's status_options).
            // One status pass classifies every path; only tracked
            // dirty states (Modified/Deleted/Added) are considered.
            let status = repo
                .status(
                    working_copy,
                    StatusOptions {
                        include_untracked: false,
                        ..StatusOptions::default()
                    },
                )
                .map_err(repository_error)?;
            let dirty: Vec<(String, atomic_repository::status::FileStatus)> = status
                .entries()
                .iter()
                .map(|e| (e.path().to_string_lossy().to_string(), e.status()))
                .filter(|(path, file_status)| {
                    matches!(
                        file_status,
                        atomic_repository::status::FileStatus::Modified
                            | atomic_repository::status::FileStatus::Deleted
                            | atomic_repository::status::FileStatus::Added
                    ) && (request.paths.is_empty()
                        || request.paths.iter().any(|p| path.starts_with(p.as_str())))
                })
                .collect();
            // The CLI's safety gate: only a WHOLE-copy restore (no
            // paths) requires force; naming paths is explicit consent.
            if request.paths.is_empty() && !dirty.is_empty() && !request.force {
                return Err(domain_status(
                    ErrorCode::PreconditionFailed,
                    "restore: working copy has unrecorded changes — \
                         name paths explicitly or pass force",
                ));
            }
            let mut restored: Vec<String> = Vec::new();
            for (path, file_status) in dirty {
                match file_status {
                    // Added: undo the add — untrack, keep on disk.
                    atomic_repository::status::FileStatus::Added => {
                        repo.remove(
                            working_copy,
                            &path,
                            TrackingOptions::default().with_recursive(false),
                        )
                        .map_err(repository_error)?;
                        restored.push(path);
                    }
                    // Modified/Deleted: pristine content back to disk.
                    _ => {
                        let content = repo
                            .get_file_content(std::path::Path::new(&path))
                            .map_err(repository_error)?;
                        if let Some(bytes) = content {
                            let full = handle.root.join(&path);
                            if let Some(parent) = full.parent() {
                                std::fs::create_dir_all(parent).map_err(|error| {
                                    domain_status(
                                        ErrorCode::Repository,
                                        format!("failed to create directory: {error}"),
                                    )
                                })?;
                            }
                            std::fs::write(&full, bytes).map_err(|error| {
                                domain_status(
                                    ErrorCode::Repository,
                                    format!("failed to write {}: {error}", full.display()),
                                )
                            })?;
                            restored.push(path);
                        }
                        // No pristine content: skip (nothing safe to do).
                    }
                }
            }
            Ok(RestoreResponse {
                restored,
                meta: None,
            })
        })
        .await
        .map_err(|error| Status::internal(format!("restore task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn revise(
        &self,
        request: Request<ReviseRequest>,
    ) -> Result<Response<ReviseResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("Revise", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || -> Result<ReviseResponse, Status> {
            let (mut repo, _workspace) =
                handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Reconcile)?;
            let target = match request
                .target
                .as_ref()
                .and_then(|target| target.kind.clone())
            {
                Some(change_ref::Kind::Hash(hash)) => {
                    let mut bytes = [0u8; 32];
                    if hash.value.len() != 32 {
                        return Err(domain_status(
                            ErrorCode::InvalidArgument,
                            "hash must be 32 bytes",
                        ));
                    }
                    bytes.copy_from_slice(&hash.value);
                    atomic_core::types::Merkle(bytes)
                }
                Some(change_ref::Kind::Sequence(_)) | None => {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        "revise target must be a change hash (the client resolves @/@~N)",
                    ))
                }
                // Case-insensitive unique-prefix resolution over the whole
                // change store; the revise stack surgery speaks.
                Some(change_ref::Kind::Prefix(prefix)) => {
                    let prefix = prefix.trim().to_ascii_uppercase();
                    match repo.find_change_by_prefix(&prefix) {
                        Ok(Some(hash)) => hash,
                        Ok(None) => {
                            return Err(domain_status(
                                ErrorCode::NotFound,
                                format!("no change found matching '{prefix}'"),
                            ))
                        }
                        Err(atomic_repository::RepositoryError::AmbiguousHash {
                            prefix,
                            matches,
                        }) => {
                            return Err(domain_status(
                                ErrorCode::NotFound,
                                format!(
                                    "ambiguous change prefix '{prefix}' (matches: {})",
                                    matches.join(", ")
                                ),
                            ))
                        }
                        Err(error) => return Err(repository_error(error)),
                    }
                }
            };
            // The content-modification mode: the message (and optional
            // author override, and the positional file filter) compose
            // CLIENT-side — the editor flow is interactive by design —
            // and the stack surgery (unrecord → record from the working
            // copy → re-apply) is the domain's, applied atomically.
            let wire_author = request.author.map(|author| Author {
                name: author.name,
                email: author.email,
                identity: None,
            });
            let (message, author, paths, content_mode) = match (
                request.reword_message,
                request.content,
            ) {
                (Some(message), _) => (message, wire_author, Vec::new(), false),
                (None, Some(content)) => (
                    content.message,
                    content.author.map(|author| Author {
                        name: author.name,
                        email: author.email,
                        identity: None,
                    }),
                    content.paths,
                    true,
                ),
                (None, None) => return Err(domain_status(
                    ErrorCode::InvalidArgument,
                    "reword_message or content is required (the interactive editor is client-side)",
                )),
            };
            let new_hash = if content_mode {
                repo.revise_content(&target, &message, author, paths)
                    .map(|outcome| outcome.new_hash)
            } else {
                repo.reword_change(&target, &message, author)
                    .map(|outcome| outcome.new_hash)
            }
            .map_err(|error| match error {
                atomic_repository::RepositoryError::ChangeNotFound { hash } => domain_status(
                    ErrorCode::NotFound,
                    format!("change {hash} not found on the current view"),
                ),
                other => domain_status(ErrorCode::ChangeRejected, other.to_string()),
            })?;
            let change = repo
                .load_change(&new_hash)
                .map_err(|error| domain_status(ErrorCode::Repository, error.to_string()))?;
            Ok(ReviseResponse {
                change: Some(change_info(&change, &new_hash)),
                meta: None,
            })
        })
        .await
        .map_err(|error| Status::internal(format!("revise task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn insert_changes(
        &self,
        request: Request<InsertChangesRequest>,
    ) -> Result<Response<InsertChangesResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("InsertChanges", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result =
            tokio::task::spawn_blocking(move || -> Result<InsertChangesResponse, Status> {
                let (repo, workspace) =
                    handle.workspace_repository(if request.dry_run.unwrap_or(false) {
                        atomic_repository::WorkspaceTxnMode::Observe
                    } else {
                        atomic_repository::WorkspaceTxnMode::Reconcile
                    })?;
                let working_copy = workspace.working_copy();
                // The target defaults to the repository's current view (the
                // wire's target_view is a display hint that must agree).
                let target = request
                    .target_view
                    .clone()
                    .unwrap_or_else(|| workspace.view().name.clone());
                let dry_run = request.dry_run.unwrap_or(false);
                let apply_dependencies = request.apply_dependencies.unwrap_or(true);
                // How the working copy refreshes after the insert — the
                // per-arm policy the local bodies apply (the bare
                // promotion never rematerializes: its target view is not
                // checked out).
                #[derive(PartialEq, Clone, Copy)]
                enum Refresh {
                    Skip,
                    Surgical,
                    Full,
                }
                // One report per arm: everything the CLI renders with the
                // local bodies' print functions.
                struct ArmReport {
                    applied: Vec<atomic_core::types::Merkle>,
                    affected_paths: std::collections::HashSet<String>,
                    new_state: atomic_core::types::Merkle,
                    skipped: usize,
                    has_conflicts: bool,
                    resolved_source_view: Option<String>,
                    resolved_target_view: String,
                    both_views_shared: Option<bool>,
                    resolved_change: Option<atomic_core::types::Merkle>,
                    refresh: Refresh,
                }
                let cross_view_error = |error: atomic_repository::RepositoryError| -> Status {
                    domain_status(ErrorCode::ChangeRejected, error.to_string())
                };
                let report = match request.source {
                    // A resolved single change: the direct domain calls the
                    // local single-insert body makes (the --deps=false
                    // escape hatch skips the closure).
                    Some(insert_changes_request::Source::SingleChange(hash)) => {
                        let hash = wire_hash(&hash)?;
                        let outcome = single_insert(
                            &repo,
                            &hash,
                            &target,
                            apply_dependencies,
                            request.allow_conflicts,
                        )
                        .map_err(cross_view_error)?;
                        ArmReport {
                            applied: outcome.stats.applied_hashes.clone(),
                            affected_paths: outcome.stats.affected_paths.clone(),
                            new_state: outcome.new_state,
                            skipped: 0,
                            has_conflicts: outcome.has_conflicts,
                            resolved_source_view: None,
                            resolved_target_view: target.clone(),
                            both_views_shared: None,
                            resolved_change: Some(hash),
                            refresh: Refresh::Full,
                        }
                    }
                    // A raw single reference (`insert <ref>`): resolved
                    // with the local resolver's semantics, then the same
                    // single-insert path.
                    Some(insert_changes_request::Source::SingleRef(reference)) => {
                        let hash = resolve_change_reference(&repo, &reference)?;
                        let outcome = single_insert(
                            &repo,
                            &hash,
                            &target,
                            apply_dependencies,
                            request.allow_conflicts,
                        )
                        .map_err(cross_view_error)?;
                        ArmReport {
                            applied: outcome.stats.applied_hashes.clone(),
                            affected_paths: outcome.stats.affected_paths.clone(),
                            new_state: outcome.new_state,
                            skipped: 0,
                            has_conflicts: outcome.has_conflicts,
                            resolved_source_view: None,
                            resolved_target_view: target.clone(),
                            both_views_shared: None,
                            resolved_change: Some(hash),
                            refresh: Refresh::Full,
                        }
                    }
                    Some(insert_changes_request::Source::FromView(source)) => {
                        let options = CrossViewInsertOptions::new(&source, &target)
                            .with_dependencies(apply_dependencies)
                            .allow_conflicts(request.allow_conflicts)
                            .dry_run(dry_run);
                        let outcome = repo.insert_from_view(options).map_err(cross_view_error)?;
                        ArmReport {
                            applied: outcome.applied_hashes.clone(),
                            affected_paths: outcome.affected_paths.clone(),
                            new_state: outcome.new_state,
                            skipped: outcome.skipped_hashes.len(),
                            has_conflicts: outcome.has_conflicts,
                            resolved_source_view: Some(source),
                            resolved_target_view: target.clone(),
                            both_views_shared: None,
                            resolved_change: None,
                            refresh: Refresh::Surgical,
                        }
                    }
                    Some(insert_changes_request::Source::UpToTag(tag)) => {
                        // The source view: the --from-view override when
                        // the wire carries one, else the repository's
                        // current view — exactly the CLI default.
                        let from_view = request
                            .tag_from_view
                            .clone()
                            .unwrap_or_else(|| workspace.view().name.clone());
                        let options = CrossViewInsertOptions::new(&from_view, &target)
                            .up_to_tag(&tag)
                            .with_dependencies(apply_dependencies)
                            .allow_conflicts(request.allow_conflicts)
                            .dry_run(dry_run);
                        let outcome = repo.insert_from_view(options).map_err(cross_view_error)?;
                        ArmReport {
                            applied: outcome.applied_hashes.clone(),
                            affected_paths: outcome.affected_paths.clone(),
                            new_state: outcome.new_state,
                            skipped: outcome.skipped_hashes.len(),
                            has_conflicts: outcome.has_conflicts,
                            resolved_source_view: Some(from_view),
                            resolved_target_view: target.clone(),
                            both_views_shared: None,
                            resolved_change: None,
                            refresh: Refresh::Surgical,
                        }
                    }
                    // The bare CLI promotion: the current view into its
                    // parent (or the explicit target override). Everything
                    // resolves HERE — the CLI holds no repository handle.
                    Some(insert_changes_request::Source::PromoteCurrentView(_)) => {
                        let source = workspace.view().name.clone();
                        let source_info = repo
                            .get_view_info(&source)
                            .map_err(|error| domain_status(ErrorCode::View, error.to_string()))?;
                        let target = match request.target_view.clone() {
                            Some(override_target) => override_target,
                            None => source_info.parent_name.clone().ok_or_else(|| {
                                domain_status(
                                    ErrorCode::View,
                                    format!(
                                        "'{source}' is a root view — there is no parent to \
                                         insert into.\n  Use 'atomic insert from-view <source>' \
                                         or pass --to <view> to choose a target."
                                    ),
                                )
                            })?,
                        };
                        if target == source {
                            return Err(domain_status(
                                ErrorCode::InvalidArgument,
                                format!(
                                    "Source and target are the same view ('{source}'). Pass \
                                     --to <view> to insert somewhere else."
                                ),
                            ));
                        }
                        // The pre-flight listing: the same domain read the
                        // local pre-flight makes (the dry-run form's report
                        // AND the real form's count/confirm inputs).
                        let missing = repo
                            .get_missing_changes_between(&source, Some(&target))
                            .map_err(cross_view_error)?;
                        let mut both_views_shared = None;
                        if !dry_run && !missing.is_empty() {
                            // The shared→shared confirmation gate the CLI
                            // prompts for client-side.
                            let target_info = repo.get_view_info(&target).map_err(|error| {
                                domain_status(ErrorCode::View, error.to_string())
                            })?;
                            both_views_shared = Some(
                                source_info.scope.is_shared() && target_info.scope.is_shared(),
                            );
                        }
                        if dry_run {
                            // The preview: list what would move; insert
                            // nothing. (The local dry-run branch lists the
                            // missing set from this same read.)
                            ArmReport {
                                applied: missing,
                                affected_paths: std::collections::HashSet::new(),
                                new_state: atomic_core::types::Merkle::ZERO,
                                skipped: 0,
                                has_conflicts: false,
                                resolved_source_view: Some(source),
                                resolved_target_view: target,
                                both_views_shared: None,
                                resolved_change: None,
                                refresh: Refresh::Skip,
                            }
                        } else {
                            // The insert itself: the same domain call the
                            // local body makes after its confirmation.
                            let options = CrossViewInsertOptions::new(&source, &target)
                                .with_dependencies(apply_dependencies)
                                .allow_conflicts(request.allow_conflicts);
                            let outcome =
                                repo.insert_from_view(options).map_err(cross_view_error)?;
                            ArmReport {
                                applied: outcome.applied_hashes.clone(),
                                affected_paths: outcome.affected_paths.clone(),
                                new_state: outcome.new_state,
                                skipped: outcome.skipped_hashes.len(),
                                has_conflicts: outcome.has_conflicts,
                                resolved_source_view: Some(source),
                                resolved_target_view: target,
                                both_views_shared,
                                resolved_change: None,
                                // The promotion's target is never the
                                // checked-out view: no rematerialization.
                                refresh: Refresh::Skip,
                            }
                        }
                    }
                    // Multi-pick: resolve each reference with the local
                    // resolver's semantics, then the cherry-pick call the
                    // local multi-insert body makes.
                    Some(insert_changes_request::Source::ChangeSet(set)) => {
                        let mut hashes = Vec::with_capacity(set.changes.len());
                        for reference in &set.changes {
                            hashes.push(resolve_change_reference(&repo, reference)?);
                        }
                        let outcome = if dry_run {
                            let options = CrossViewInsertOptions::new("", &target)
                                .only_changes(hashes.clone())
                                .with_dependencies(true)
                                .dry_run(true);
                            repo.insert_from_view(options).map_err(cross_view_error)?
                        } else {
                            repo.cherry_pick(&hashes, "", Some(&target))
                                .map_err(cross_view_error)?
                        };
                        ArmReport {
                            applied: outcome.applied_hashes.clone(),
                            affected_paths: outcome.affected_paths.clone(),
                            new_state: outcome.new_state,
                            skipped: outcome.skipped_hashes.len(),
                            has_conflicts: outcome.has_conflicts,
                            resolved_source_view: None,
                            resolved_target_view: target.clone(),
                            both_views_shared: None,
                            resolved_change: None,
                            refresh: Refresh::Full,
                        }
                    }
                    None => {
                        return Err(domain_status(
                            ErrorCode::InvalidArgument,
                            "insert source required (change hash, from-view, or up-to-tag)",
                        ))
                    }
                };
                // Render the inserted changes (tolerant of an unloadable
                // change — the local listings print the hash alone) and
                // collect the touched paths for the surgical refresh.
                let mut inserted = Vec::with_capacity(report.applied.len());
                let affected_paths = report.affected_paths;
                for hash in &report.applied {
                    match repo.load_change(hash) {
                        Ok(change) => {
                            inserted.push(change_info(&change, hash));
                        }
                        Err(_) => inserted.push(bare_change_info(hash)),
                    }
                }
                // Refresh the working copy when the insert landed on the
                // repository's CURRENT view: the graph insert alone never
                // touches the working tree, and a CLI standing on the
                // target view expects the inserted files on disk (the
                // local bodies' materialization, owned here so both
                // transports behave identically). Per arm: view/tag inserts
                // refresh the touched paths (full as the fallback when the
                // changes carry no path info); single- and multi-pick
                // inserts rematerialize the whole view; the bare promotion
                // never does (its target is not checked out).
                let mut files_updated = None;
                let mut directories_created = None;
                if !dry_run
                    && !report.applied.is_empty()
                    && report.refresh != Refresh::Skip
                    && target == workspace.view().name.clone()
                {
                    let materialized = if report.refresh == Refresh::Full {
                        repo.materialize(working_copy).map_err(|error| {
                            domain_status(ErrorCode::Materialize, error.to_string())
                        })?
                    } else {
                        repo.materialize_paths(working_copy, affected_paths)
                            .map_err(|error| {
                                domain_status(ErrorCode::Materialize, error.to_string())
                            })?
                    };
                    files_updated = Some(materialized.files_written as u64);
                    directories_created = Some(materialized.directories_created as u64);
                }
                // The still-on-disk conflict listing (per record, file
                // order preserved): what the CLI's conflict summary prints.
                // A read failure silences the listing — the local summary
                // returns silently too.
                let conflicts = repo
                    .list_conflicts(working_copy)
                    .unwrap_or_default()
                    .into_iter()
                    .flat_map(|(path, records)| {
                        records.into_iter().map(move |record| ConflictInfo {
                            path: path.clone(),
                            view: None,
                            base: None,
                            kind: Some(format!("{:?}", record.kind).to_lowercase()),
                            line: record.line.map(|line| line as u64),
                            sides: record.sides,
                        })
                    })
                    .collect();
                Ok(InsertChangesResponse {
                    inserted,
                    conflicts,
                    resolved_source_view: report.resolved_source_view,
                    resolved_target_view: Some(report.resolved_target_view),
                    both_views_shared: report.both_views_shared,
                    resolved_change: report.resolved_change.as_ref().map(hash_proto),
                    new_state: Some(hash_proto(&report.new_state)),
                    skipped_count: report.skipped as u32,
                    files_updated,
                    directories_created,
                    has_conflicts: report.has_conflicts,
                    meta: None,
                })
            })
            .await
            .map_err(|error| Status::internal(format!("insert task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn unrecord(
        &self,
        request: Request<UnrecordRequest>,
    ) -> Result<Response<UnrecordResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("Unrecord", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || -> Result<UnrecordResponse, Status> {
            let (repo, _workspace) =
                handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Reconcile)?;
            let options = atomic_repository::UnrecordOptions::new();
            let outcome = match request
                .target
                .as_ref()
                .and_then(|target| target.kind.clone())
            {
                Some(change_ref::Kind::Hash(hash)) => {
                    let mut bytes = [0u8; 32];
                    if hash.value.len() != 32 {
                        return Err(domain_status(
                            ErrorCode::InvalidArgument,
                            "hash must be 32 bytes",
                        ));
                    }
                    bytes.copy_from_slice(&hash.value);
                    repo.unrecord(&atomic_core::types::Merkle(bytes), options)
                        .map_err(repository_error)?
                }
                Some(change_ref::Kind::Sequence(_)) => {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        "unrecord takes a hash target (or none for the last change)",
                    ))
                }
                // Case-insensitive unique-prefix resolution over the whole
                // change store; the unrecord membership guard speaks.
                Some(change_ref::Kind::Prefix(prefix)) => {
                    let prefix = prefix.trim().to_ascii_uppercase();
                    match repo.find_change_by_prefix(&prefix) {
                        Ok(Some(hash)) => {
                            repo.unrecord(&hash, options).map_err(repository_error)?
                        }
                        Ok(None) => {
                            return Err(domain_status(
                                ErrorCode::NotFound,
                                format!("no change found matching '{prefix}'"),
                            ))
                        }
                        Err(atomic_repository::RepositoryError::AmbiguousHash {
                            prefix,
                            matches,
                        }) => {
                            return Err(domain_status(
                                ErrorCode::NotFound,
                                format!(
                                    "ambiguous change prefix '{prefix}' (matches: {})",
                                    matches.join(", ")
                                ),
                            ))
                        }
                        Err(error) => return Err(repository_error(error)),
                    }
                }
                // No target: the most recent change on the view. An empty
                // view refuses with the domain's own message.
                None => repo.unrecord_last(options).map_err(|error| {
                    if matches!(error, atomic_repository::RepositoryError::Unrecord(_)) {
                        domain_status(ErrorCode::PreconditionFailed, error.to_string())
                    } else {
                        repository_error(error)
                    }
                })?,
            };
            // The primary unrecorded change rides the response; cascades
            // (when UnrecordOptions ever allows them) list through Log.
            let removed_hash =
                outcome.unrecorded.first().copied().ok_or_else(|| {
                    domain_status(ErrorCode::Internal, "unrecord returned no change")
                })?;
            let change = repo
                .load_change(&removed_hash)
                .map_err(|error| domain_status(ErrorCode::NotFound, error.to_string()))?;
            Ok(UnrecordResponse {
                removed: Some(ChangeInfo {
                    hash: Some(hash_proto(&removed_hash)),
                    message: Some(change.hashed.header.message.clone()),
                    description: change.hashed.header.description.clone(),
                    authors: change
                        .hashed
                        .header
                        .authors
                        .iter()
                        .map(author_proto)
                        .collect(),
                    recorded_at: Some(timestamp_proto(change.hashed.header.timestamp)),
                    dependencies: Vec::new(),
                    graph_section_count: 0,
                    semantic_section_count: 0,
                    content_chunk_count: 0,
                    has_provenance: change.has_provenance(),
                    has_unhashed: change.unhashed.is_some(),
                    has_signature: change.signature.is_some(),
                }),
                meta: None,
            })
        })
        .await
        .map_err(|error| Status::internal(format!("unrecord task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn create_stash(
        &self,
        request: Request<CreateStashRequest>,
    ) -> Result<Response<CreateStashResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("CreateStash", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || -> Result<CreateStashResponse, Status> {
            let (mut repo, _workspace) =
                handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Reconcile)?;
            match repo
                .stash_push(atomic_repository::StashPushOptions {
                    message: request.message,
                    include_untracked: request.include_untracked,
                    keep: request.keep,
                })
                .map_err(|message| domain_status(ErrorCode::Repository, message))?
            {
                // A clean working copy is Ok(None), never an error —
                // the empty stash_id says "nothing to save".
                None => Ok(CreateStashResponse {
                    stash_id: String::new(),
                    meta: None,
                }),
                Some(entry) => Ok(CreateStashResponse {
                    stash_id: entry.view_name,
                    meta: None,
                }),
            }
        })
        .await
        .map_err(|error| Status::internal(format!("stash task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn apply_stash(
        &self,
        request: Request<ApplyStashRequest>,
    ) -> Result<Response<ApplyStashResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("ApplyStash", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || -> Result<ApplyStashResponse, Status> {
            let (repo, _workspace) =
                handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Reconcile)?;
            let (_stash, applied) = repo
                .stash_apply(if request.stash_id.is_empty() {
                    None
                } else {
                    Some(request.stash_id.as_str())
                })
                .map_err(|message| domain_status(ErrorCode::Repository, message))?;
            Ok(ApplyStashResponse {
                applied,
                meta: None,
            })
        })
        .await
        .map_err(|error| Status::internal(format!("stash task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn list_stashes(
        &self,
        request: Request<ListStashesRequest>,
    ) -> Result<Response<ListStashesResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("ListStashes", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || -> Result<ListStashesResponse, Status> {
            let repo = handle.repository()?;
            let stashes = repo
                .stash_list()
                .map_err(|message| domain_status(ErrorCode::Repository, message))?;
            Ok(ListStashesResponse {
                stashes: stashes
                    .into_iter()
                    .map(|stash| StashInfo {
                        id: stash.view_name,
                        message: Some(stash.message),
                        created_at: Some(prost_types::Timestamp {
                            seconds: stash.created_at.timestamp(),
                            nanos: stash.created_at.timestamp_subsec_nanos() as i32,
                        }),
                        tree: None,
                    })
                    .collect(),
            })
        })
        .await
        .map_err(|error| Status::internal(format!("stash task failed: {error}")))??;
        Ok(Response::new(result))
    }

    async fn drop_stash(
        &self,
        request: Request<DropStashRequest>,
    ) -> Result<Response<DropStashResponse>, Status> {
        let request = request.into_inner();
        let handle = self
            .state
            .resolve(request.repository.as_ref().unwrap_or(&default_ref()))?;
        self.state.log_rpc("DropStash", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        tokio::task::spawn_blocking(move || -> Result<(), Status> {
            let mut repo = handle.repository()?;
            if request.all {
                repo.stash_clear()
                    .map_err(|message| domain_status(ErrorCode::Repository, message))?;
                return Ok(());
            }
            let stash_id = request.stash_id.unwrap_or_default();
            repo.stash_drop(if stash_id.is_empty() {
                None
            } else {
                Some(stash_id.as_str())
            })
            .map_err(|message| domain_status(ErrorCode::Repository, message))?;
            Ok(())
        })
        .await
        .map_err(|error| Status::internal(format!("stash task failed: {error}")))??;
        Ok(Response::new(DropStashResponse { meta: None }))
    }
}

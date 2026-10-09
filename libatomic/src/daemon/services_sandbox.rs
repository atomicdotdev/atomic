//! SandboxService handlers.
//!
//! - The local working-tree surface (`atomic sandbox create/stage/seal`): the
//!   copy-on-write sandbox provisioning, the OCI layer staging, and the
//!   sealed image, each running the exact domain calls the local CLI bodies
//!   make.
//! - Grant admin (`OpenSandbox`/`RenewSandbox`/`CloseSandbox`), local callers
//!   only, through the state's [`super::sandbox_grants::SandboxGrants`].
//! - The remote-sandbox data ops (`Materialize`, `GetFileStates`,
//!   `GetChanges`, `SubmitChange`, `PublishProvenance`) on the repository's
//!   remote-cache kernel (`atomic_repository::remote_cache`). A sandbox
//!   caller reaches exactly its grant's view of its grant's repository; a
//!   local caller names the view.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::atomic::ErrorCode;
use crate::atomic::*;
use atomic_core::pristine::ViewTxnT;
use atomic_repository::{Repository, RepositoryError, SealOptions, StageOptions, SubmitRejection};
use tonic::{Request, Response, Status};

use super::convert::{hash_proto, timestamp_proto};
use super::sandbox_grants::{
    grant_capabilities, view_ref_id, GrantRequest, SandboxCaller, DEFAULT_TTL_SECS,
};
use super::sandbox_wire as wire;
use super::services_agent::response_meta;
use super::state::{domain_status, repository_error, DaemonState, RepoHandle};

pub struct SandboxImpl {
    pub state: Arc<DaemonState>,
}

/// The local body's default destination: `<repo-parent>/<repo-name>-
/// sandboxes/<name>`.
fn default_sandbox_dest(repo_root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let repo_name = repo_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    let parent = repo_root.parent().unwrap_or(repo_root);
    parent.join(format!("{repo_name}-sandboxes")).join(name)
}

fn repository_ref(repository: &Option<RepositoryRef>) -> Result<RepositoryRef, Status> {
    repository
        .clone()
        .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "repository is required"))
}

fn joined(error: tokio::task::JoinError) -> Status {
    Status::internal(error.to_string())
}

fn invalid(message: impl Into<String>) -> Status {
    domain_status(ErrorCode::InvalidArgument, message)
}

/// A domain error, with a missing view said as such.
fn view_error(error: RepositoryError) -> Status {
    match error {
        RepositoryError::ViewNotFound { name } => {
            domain_status(ErrorCode::NotFound, format!("no view '{name}'"))
        }
        other => repository_error(other),
    }
}

/// The view a request names: `target`'s name, or `legacy` (the admin
/// requests' display-hint string). A `target.view_id` must agree with it.
fn named_view(target: Option<&ViewRef>, legacy: &str) -> Result<String, Status> {
    let name = target
        .and_then(|t| t.name.clone())
        .filter(|n| !n.is_empty())
        .or_else(|| (!legacy.is_empty()).then(|| legacy.to_string()))
        .ok_or_else(|| invalid("name the target view"))?;
    if let Some(target) = target {
        if !target.view_id.is_empty() && target.view_id != view_ref_id(&name) {
            return Err(invalid("target view_id and name disagree"));
        }
    }
    Ok(name)
}

/// The view a data op acts on: a sandbox's grant view (an explicit target
/// must be it, and the grant must carry `capability`), or the view a local
/// caller names.
fn data_view(
    caller: Option<&SandboxCaller>,
    handle: &RepoHandle,
    repository: &RepositoryRef,
    target: Option<&ViewRef>,
    capability: &str,
) -> Result<String, Status> {
    match caller {
        Some(caller) => {
            caller.require(capability)?;
            caller.require_target(handle, repository, target)?;
            Ok(caller.view().to_string())
        }
        None => named_view(target, ""),
    }
}

/// The view exists — and, for a grant that recorded the view's id, it is
/// still the view the grant was issued for (deleting a view and recreating
/// its name does not revive a grant).
fn check_view(
    repo: &Repository,
    caller: Option<&SandboxCaller>,
    view: &str,
) -> Result<u64, Status> {
    let txn = repo
        .pristine()
        .read_txn()
        .map_err(|e| domain_status(ErrorCode::Repository, e.to_string()))?;
    let state = txn
        .get_view(view)
        .map_err(|e| domain_status(ErrorCode::Repository, e.to_string()))?
        .ok_or_else(|| domain_status(ErrorCode::NotFound, format!("no view '{view}'")))?;
    if let Some(expected) = caller.and_then(|c| c.grant.view_id) {
        if expected != state.id {
            return Err(domain_status(
                ErrorCode::Forbidden,
                format!("view '{view}' is not the view this sandbox's grant was issued for"),
            ));
        }
    }
    Ok(state.id)
}

/// `view`'s snapshot as the repository has it now.
fn current_snapshot(repo: &Repository, view: &str) -> Result<ViewSnapshot, Status> {
    let effective = repo.sandbox_effective_state(view).map_err(view_error)?;
    let own = repo
        .export_sandbox_skeleton(view, &BTreeMap::new())
        .map_err(view_error)?
        .view
        .state;
    Ok(wire::view_snapshot(view, &effective, Some(&own)))
}

/// Refuse unless `expected` is the view's snapshot now.
fn fence(repo: &Repository, view: &str, expected: &ViewSnapshot) -> Result<ViewSnapshot, Status> {
    let fence = wire::snapshot_fence(expected).map_err(invalid)?;
    let current = current_snapshot(repo, view)?;
    if wire::snapshot_fence(&current).map_err(invalid)? != fence {
        return Err(domain_status(
            ErrorCode::ViewStale,
            format!("view '{view}' has moved on; materialize it again"),
        ));
    }
    Ok(current)
}

fn wire_error(message: String) -> Status {
    domain_status(ErrorCode::Internal, message)
}

/// Whether a change touches the vault — which a sandbox may submit only with
/// `vault.write` as well.
fn touches_vault(bytes: &[u8]) -> bool {
    let Ok((change, _)) = atomic_core::change::Change::deserialize(&mut &bytes[..]) else {
        // Not a change: the kernel refuses it as malformed.
        return false;
    };
    let vault = |p: &str| p == ".vault" || p.starts_with(".vault/");
    change.hunks().iter().filter_map(|h| h.path()).any(vault)
        || change.file_ops().iter().map(|ops| ops.path()).any(vault)
}

#[tonic::async_trait]
impl sandbox_service_server::SandboxService for SandboxImpl {
    async fn open_sandbox(
        &self,
        request: Request<OpenSandboxRequest>,
    ) -> Result<Response<OpenSandboxResponse>, Status> {
        self.state.local_only(&request, "OpenSandbox")?;
        let request = request.into_inner();
        let repository = repository_ref(&request.repository)?;
        let handle = self.state.resolve(&repository)?;
        self.state.log_rpc("OpenSandbox", Some(&handle));
        let view = named_view(request.target.as_ref(), &request.view)?;
        let capabilities = grant_capabilities(&request.capabilities).map_err(|e| e.status())?;
        let ttl_secs = request.ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
        let meta = request.meta.clone();
        let acting_as = request.acting_as_did.clone().filter(|d| !d.is_empty());

        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let lookup = {
            let handle = handle.clone();
            let view = view.clone();
            tokio::task::spawn_blocking(move || {
                let repo = handle.repository_readonly()?;
                let id = check_view(&repo, None, &view)?;
                Ok::<_, Status>((id, current_snapshot(&repo, &view)?))
            })
        };
        let (view_id, snapshot) = lookup.await.map_err(joined)??;
        let (token, grant) = self
            .state
            .sandbox_grants()
            .open(GrantRequest {
                repository_id: handle.repository_id(),
                view: view.clone(),
                view_id: Some(view_id),
                acting_as,
                capabilities,
                ttl_secs,
            })
            .map_err(|e| e.status())?;
        Ok(Response::new(OpenSandboxResponse {
            opened: Some(SandboxOpened {
                // libatomic has no transport: the host that serves these
                // handlers fills in how a sandbox reaches it.
                endpoint_addr: String::new(),
                view: view.clone(),
                token,
                expires_at: Some(timestamp_proto(grant.expires)),
                repository: Some(RepositoryRef {
                    authority: repository.authority.clone(),
                    repository_id: grant.repository_id.clone(),
                    workspace_id: None,
                }),
                target: Some(wire::view_ref(&view)),
                capabilities: grant.capabilities.iter().cloned().collect(),
                snapshot: Some(snapshot),
            }),
            meta: response_meta(&meta),
        }))
    }

    async fn renew_sandbox(
        &self,
        request: Request<RenewSandboxRequest>,
    ) -> Result<Response<RenewSandboxResponse>, Status> {
        self.state.local_only(&request, "RenewSandbox")?;
        let request = request.into_inner();
        let repository = repository_ref(&request.repository)?;
        let handle = self.state.resolve(&repository)?;
        self.state.log_rpc("RenewSandbox", Some(&handle));
        let view = named_view(request.target.as_ref(), &request.view)?;
        let grant = self
            .state
            .sandbox_grants()
            .renew(&handle.repository_id(), &view, request.ttl_secs)
            .map_err(|e| e.status())?;
        Ok(Response::new(RenewSandboxResponse {
            expires_at: Some(timestamp_proto(grant.expires)),
            meta: response_meta(&request.meta),
        }))
    }

    async fn close_sandbox(
        &self,
        request: Request<CloseSandboxRequest>,
    ) -> Result<Response<CloseSandboxResponse>, Status> {
        self.state.local_only(&request, "CloseSandbox")?;
        let request = request.into_inner();
        let repository = repository_ref(&request.repository)?;
        let handle = self.state.resolve(&repository)?;
        self.state.log_rpc("CloseSandbox", Some(&handle));
        let view = named_view(request.target.as_ref(), &request.view)?;
        let revoked = self
            .state
            .sandbox_grants()
            .close(&handle.repository_id(), &view)
            .map_err(|e| e.status())?;
        Ok(Response::new(CloseSandboxResponse {
            revoked,
            meta: response_meta(&request.meta),
        }))
    }

    async fn create_sandbox_tree(
        &self,
        request: Request<CreateSandboxTreeRequest>,
    ) -> Result<Response<CreateSandboxTreeResponse>, Status> {
        self.state.local_only(&request, "CreateSandboxTree")?;
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("CreateSandboxTree", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let name = request.name.clone();
        let from_view = request.from_view.clone();
        let view = request.view.clone();
        let dest = request.dest.clone();
        let root = handle.root.clone();
        let (resolved_dest, view, created_draft, files_cloned, repo_root) =
            tokio::task::spawn_blocking(move || {
                let mut repo = handle.repository()?;
                let dest = dest
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| default_sandbox_dest(&root, &name));
                if dest.exists() {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        format!("destination already exists: {}", dest.display()),
                    ));
                }
                // --from <v> creates a new draft named after the sandbox
                // forked from <v>; --view <v> uses the existing view;
                // neither → the current view.
                let (view, created_draft) = if let Some(from) = &from_view {
                    repo.create_view_from(&name, from)
                        .map_err(repository_error)?;
                    (name.clone(), true)
                } else {
                    let view = view.unwrap_or_else(|| repo.current_view().to_string());
                    (view, false)
                };
                let count = repo
                    .provision_sandbox(&dest, &view)
                    .map_err(repository_error)?;
                Ok::<_, Status>((
                    dest.display().to_string(),
                    view,
                    created_draft,
                    count as u32,
                    root.display().to_string(),
                ))
            })
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(CreateSandboxTreeResponse {
            sandbox_path: resolved_dest.clone(),
            resolved_dest,
            view,
            created_draft,
            files_cloned,
            repo_root,
            meta: response_meta(&meta),
        }))
    }

    async fn stage_sandbox_image(
        &self,
        request: Request<StageSandboxImageRequest>,
    ) -> Result<Response<StageSandboxImageResponse>, Status> {
        self.state.local_only(&request, "StageSandboxImage")?;
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("StageSandboxImage", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let view = request.name.clone();
        let base_view = request.base_view.clone();
        let out = request.out_dir.clone();
        let (image_dir, manifest_digest, base_diff_id, delta_files) =
            tokio::task::spawn_blocking(move || {
                let repo = handle.repository()?;
                let result = repo
                    .stage(StageOptions {
                        view,
                        base_view,
                        out: std::path::PathBuf::from(out),
                    })
                    .map_err(repository_error)?;
                Ok::<_, Status>((
                    result.out.display().to_string(),
                    result.manifest_digest,
                    result.base_diff_id,
                    result.delta_files as u64,
                ))
            })
            .await
            .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(StageSandboxImageResponse {
            staged_ref: manifest_digest.clone(),
            image_dir,
            manifest_digest,
            base_diff_id,
            delta_files,
            meta: response_meta(&meta),
        }))
    }

    async fn seal_sandbox_image(
        &self,
        request: Request<SealSandboxImageRequest>,
    ) -> Result<Response<SealSandboxImageResponse>, Status> {
        self.state.local_only(&request, "SealSandboxImage")?;
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("SealSandboxImage", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let view = request.name.clone();
        let out = request.out_dir.clone();
        let entrypoint = request.entrypoint.clone();
        let env = request.env.clone();
        let (image_dir, manifest_digest, files) = tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            let result = repo
                .seal(SealOptions {
                    view,
                    out: std::path::PathBuf::from(out),
                    entrypoint,
                    env,
                })
                .map_err(repository_error)?;
            Ok::<_, Status>((
                result.out.display().to_string(),
                result.manifest_digest,
                result.files as u64,
            ))
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(SealSandboxImageResponse {
            sealed_ref: manifest_digest.clone(),
            image_dir,
            manifest_digest,
            files,
            meta: response_meta(&meta),
        }))
    }

    type MaterializeStream = std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<MaterializeFrame, Status>> + Send>,
    >;

    async fn materialize(
        &self,
        request: Request<MaterializeRequest>,
    ) -> Result<Response<Self::MaterializeStream>, Status> {
        let caller = self.state.sandbox_caller(&request)?;
        let request = request.into_inner();
        let repository = repository_ref(&request.repository)?;
        let handle = self.state.resolve(&repository)?;
        self.state.log_rpc("Materialize", Some(&handle));
        let view = data_view(
            caller.as_ref(),
            &handle,
            &repository,
            request.target.as_ref(),
            "sandbox.read",
        )?;
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        if let Some(caller) = &caller {
            caller.revalidate(&self.state)?;
        }
        // Rendered whole while the repository is held, then streamed with it
        // released: entries, snapshot and skeleton come from one state, and a
        // slow reader never keeps the repository open.
        let frames = tokio::task::spawn_blocking(move || {
            // Writable: rendering a view other than the checked-out one
            // projects its tree in a transaction that is thrown away.
            let repo = handle.repository()?;
            check_view(&repo, caller.as_ref(), &view)?;
            let mut frames = Vec::new();
            let mut live = BTreeMap::new();
            repo.materialize_view_entries::<()>(&view, |entry| {
                live.insert(entry.inode, entry.path.clone());
                frames.push(MaterializeFrame {
                    frame: Some(materialize_frame::Frame::Entry(wire::tree_entry_proto(
                        entry,
                    ))),
                });
                Ok(())
            })
            .map_err(view_error)?
            .map_err(|()| Status::internal("materialize stopped"))?;
            let skeleton = repo
                .export_sandbox_skeleton(&view, &live)
                .map_err(view_error)?;
            let effective = repo.sandbox_effective_state(&view).map_err(view_error)?;
            let snapshot = wire::view_snapshot(&view, &effective, Some(&skeleton.view.state));
            let entries = frames.len() as u64;
            frames.push(MaterializeFrame {
                frame: Some(materialize_frame::Frame::Done(MaterializeSummary {
                    snapshot: Some(hash_proto(&effective)),
                    entries,
                    skeleton: Some(
                        wire::skeleton_proto(&skeleton, snapshot, Some(&live))
                            .map_err(wire_error)?,
                    ),
                })),
            });
            Ok::<_, Status>(frames)
        })
        .await
        .map_err(joined)??;
        Ok(Response::new(Box::pin(tokio_stream::iter(
            frames.into_iter().map(Ok),
        ))))
    }

    async fn get_file_states(
        &self,
        request: Request<GetFileStatesRequest>,
    ) -> Result<Response<GetFileStatesResponse>, Status> {
        let caller = self.state.sandbox_caller(&request)?;
        let request = request.into_inner();
        let repository = repository_ref(&request.repository)?;
        let handle = self.state.resolve(&repository)?;
        self.state.log_rpc("GetFileStates", Some(&handle));
        let view = data_view(
            caller.as_ref(),
            &handle,
            &repository,
            request.target.as_ref(),
            "sandbox.read",
        )?;
        let inodes = request.inodes;
        let expected = request.expected_snapshot;
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        if let Some(caller) = &caller {
            caller.revalidate(&self.state)?;
        }
        let slice = tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            check_view(&repo, caller.as_ref(), &view)?;
            let snapshot = match &expected {
                Some(expected) => fence(&repo, &view, expected)?,
                None => current_snapshot(&repo, &view)?,
            };
            // Inodes are handles, not permissions: the kernel serves only
            // the ones the view has, filtered by its effective perspective.
            let slice = repo
                .export_sandbox_slice(&view, &inodes)
                .map_err(view_error)?;
            wire::slice_proto(&slice, snapshot).map_err(wire_error)
        })
        .await
        .map_err(joined)??;
        Ok(Response::new(GetFileStatesResponse { slice: Some(slice) }))
    }

    async fn get_changes(
        &self,
        request: Request<GetChangesRequest>,
    ) -> Result<Response<GetChangesResponse>, Status> {
        /// GetCapabilities' `max_changes_per_request`.
        const MAX_CHANGES: usize = 64;
        let caller = self.state.sandbox_caller(&request)?;
        let request = request.into_inner();
        let repository = repository_ref(&request.repository)?;
        let handle = self.state.resolve(&repository)?;
        self.state.log_rpc("GetChanges", Some(&handle));
        let view = data_view(
            caller.as_ref(),
            &handle,
            &repository,
            request.target.as_ref(),
            "sandbox.read",
        )?;
        if request.hashes.len() > MAX_CHANGES {
            return Err(domain_status(
                ErrorCode::ResourceExhausted,
                format!("at most {MAX_CHANGES} changes per request"),
            ));
        }
        let hashes = request
            .hashes
            .iter()
            .map(|h| wire::hash_from_proto(Some(h)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(invalid)?;
        let expected = request.expected_snapshot;
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        if let Some(caller) = &caller {
            caller.revalidate(&self.state)?;
        }
        let outcome = tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            check_view(&repo, caller.as_ref(), &view)?;
            if let Some(expected) = &expected {
                fence(&repo, &view, expected)?;
            }
            Ok::<_, Status>(
                match repo
                    .export_sandbox_changes(&view, &hashes)
                    .map_err(view_error)?
                {
                    Ok(changes) => get_changes_response::Outcome::Changes(ChangesPayload {
                        changes: changes
                            .into_iter()
                            .map(|(hash, bytes)| wire::change_bundle(&hash, bytes))
                            .collect(),
                    }),
                    Err(rejection) => get_changes_response::Outcome::Refused(
                        wire::rejection_error_info(&rejection),
                    ),
                },
            )
        })
        .await
        .map_err(joined)??;
        Ok(Response::new(GetChangesResponse {
            outcome: Some(outcome),
        }))
    }

    async fn publish_provenance(
        &self,
        request: Request<PublishProvenanceRequest>,
    ) -> Result<Response<PublishProvenanceResponse>, Status> {
        let caller = self.state.sandbox_caller(&request)?;
        let request = request.into_inner();
        let repository = repository_ref(&request.repository)?;
        let handle = self.state.resolve(&repository)?;
        self.state.log_rpc("PublishProvenance", Some(&handle));
        let view = data_view(
            caller.as_ref(),
            &handle,
            &repository,
            request.target.as_ref(),
            "provenance.write",
        )?;
        let graph = wire::provenance_graph_from(request.graph.as_ref()).map_err(invalid)?;
        let turn = wire::session_turn_from(request.session_turn.as_ref()).map_err(invalid)?;
        let named = request
            .turn
            .as_ref()
            .ok_or_else(|| invalid("turn is required"))?;
        if named.session_id != turn.session_id || named.turn_number != turn.turn_number {
            return Err(invalid("turn and session_turn name different turns"));
        }
        if let Some(caller) = &caller {
            let owns = self
                .state
                .sandbox_grants()
                .owns_session(&caller.grant, &turn.session_id)
                .map_err(|e| e.status())?;
            if !owns {
                return Err(domain_status(
                    ErrorCode::Forbidden,
                    format!(
                        "session '{}' is not this sandbox's (view '{}')",
                        turn.session_id,
                        caller.view()
                    ),
                ));
            }
        }
        let meta = request.meta.clone();
        let expected_generation = request.expected_generation;
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        if let Some(caller) = &caller {
            caller.revalidate(&self.state)?;
        }
        let outcome = tokio::task::spawn_blocking(move || {
            if expected_generation != 0 {
                // The journal and the repository share one database file:
                // the store is opened, read and dropped before the repository.
                let store = handle.change_store()?;
                let stored = store
                    .get_provenance_turn_for(&turn.session_id, turn.turn_number)
                    .map_err(|e| domain_status(ErrorCode::ProvenanceStore, e.to_string()))?;
                if stored.map(|t| t.generation) != Some(expected_generation) {
                    return Err(domain_status(
                        ErrorCode::GenerationConflict,
                        format!(
                            "turn {} of session '{}' is not at generation {expected_generation}",
                            turn.turn_number, turn.session_id
                        ),
                    ));
                }
            }
            let repo = handle.repository()?;
            check_view(&repo, caller.as_ref(), &view)?;
            Ok::<_, Status>(
                match repo
                    .publish_sandbox_provenance(&view, &graph, turn)
                    .map_err(|e| domain_status(ErrorCode::Provenance, e.to_string()))?
                {
                    Ok(publication) => publish_provenance_response::Outcome::Publication(
                        wire::publication_proto(&publication).map_err(wire_error)?,
                    ),
                    Err(rejection) => publish_provenance_response::Outcome::Refused(
                        wire::rejection_error_info(&rejection),
                    ),
                },
            )
        })
        .await
        .map_err(joined)??;
        Ok(Response::new(PublishProvenanceResponse {
            outcome: Some(outcome),
            meta: response_meta(&meta),
        }))
    }

    async fn submit_change(
        &self,
        request: Request<SubmitChangeRequest>,
    ) -> Result<Response<SubmitChangeResponse>, Status> {
        let caller = self.state.sandbox_caller(&request)?;
        let request = request.into_inner();
        let repository = repository_ref(&request.repository)?;
        let handle = self.state.resolve(&repository)?;
        self.state.log_rpc("SubmitChange", Some(&handle));
        let view = data_view(
            caller.as_ref(),
            &handle,
            &repository,
            request.target.as_ref(),
            "sandbox.submit",
        )?;
        let (hash, bytes) = wire::change_from_bundle(
            request
                .change
                .as_ref()
                .ok_or_else(|| invalid("change is required"))?,
        )
        .map_err(invalid)?;
        let base_state = wire::snapshot_fence(
            request
                .expected_snapshot
                .as_ref()
                .ok_or_else(|| invalid("expected_snapshot is required"))?,
        )
        .map_err(invalid)?;
        if let Some(caller) = &caller {
            if touches_vault(&bytes) {
                caller.require("vault.write")?;
            }
        }
        let meta = request.meta.clone();
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        // Checked again after the wait: a grant that expired or was revoked
        // while queued publishes nothing.
        if let Some(caller) = &caller {
            caller.revalidate(&self.state)?;
        }
        let (outcome, snapshot) = tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            check_view(&repo, caller.as_ref(), &view)?;
            let submitted = repo
                .insert_submitted_change(&view, &base_state, &hash, &bytes)
                .map_err(view_error)?;
            let live_skeleton = |repo: &Repository| -> Result<SandboxSkeleton, Status> {
                let skeleton = repo.current_sandbox_skeleton(&view).map_err(view_error)?;
                let snapshot = current_snapshot(repo, &view)?;
                wire::skeleton_proto(&skeleton, snapshot, None).map_err(wire_error)
            };
            Ok::<_, Status>(match submitted {
                Ok(submitted) => {
                    let skeleton = repo
                        .after_sandbox_submit(&view, &submitted.hash)
                        .map_err(view_error)?;
                    let snapshot = current_snapshot(&repo, &view)?;
                    (
                        submit_change_response::Outcome::Submitted(ChangeSubmitted {
                            submitted: true,
                            skeleton: Some(
                                wire::skeleton_proto(&skeleton, snapshot.clone(), None)
                                    .map_err(wire_error)?,
                            ),
                        }),
                        Some(snapshot),
                    )
                }
                // A stale base is recoverable, and the sandbox cannot get out
                // of it alone — its view row is the stale one. The view as it
                // is now rides the refusal.
                Err(rejection @ SubmitRejection::StaleView { .. }) => (
                    submit_change_response::Outcome::Refused(ChangeRefused {
                        rejection: Some(wire::rejection_error_info(&rejection)),
                        skeleton: Some(live_skeleton(&repo)?),
                    }),
                    None,
                ),
                Err(rejection) => (
                    submit_change_response::Outcome::Refused(ChangeRefused {
                        rejection: Some(wire::rejection_error_info(&rejection)),
                        skeleton: None,
                    }),
                    None,
                ),
            })
        })
        .await
        .map_err(joined)??;
        let mut meta = response_meta(&meta).unwrap_or_default();
        meta.snapshot = snapshot;
        Ok(Response::new(SubmitChangeResponse {
            outcome: Some(outcome),
            meta: Some(meta),
        }))
    }
}

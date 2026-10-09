//! SandboxService handlers — the local working-tree surface
//! (`atomic sandbox create/stage/seal`): the copy-on-write sandbox
//! provisioning, the OCI layer staging, and the sealed image, each running
//! the exact domain calls the local CLI bodies make. The grant-admin and
//! remote data-op RPCs are separate slices (HANDOFF §5) and stay refused.

use std::sync::Arc;

use crate::atomic::ErrorCode;
use crate::atomic::*;
use atomic_repository::SealOptions;
use atomic_repository::StageOptions;
use tonic::{Request, Response, Status};

use super::services_agent::response_meta;
use super::state::{domain_status, repository_error, DaemonState};

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

#[tonic::async_trait]
impl sandbox_service_server::SandboxService for SandboxImpl {
    async fn open_sandbox(
        &self,
        _request: Request<OpenSandboxRequest>,
    ) -> Result<Response<OpenSandboxResponse>, Status> {
        Err(Status::unimplemented("OpenSandbox lands with its slice"))
    }

    async fn renew_sandbox(
        &self,
        _request: Request<RenewSandboxRequest>,
    ) -> Result<Response<RenewSandboxResponse>, Status> {
        Err(Status::unimplemented("RenewSandbox lands with its slice"))
    }

    async fn close_sandbox(
        &self,
        _request: Request<CloseSandboxRequest>,
    ) -> Result<Response<CloseSandboxResponse>, Status> {
        Err(Status::unimplemented("CloseSandbox lands with its slice"))
    }

    async fn create_sandbox_tree(
        &self,
        request: Request<CreateSandboxTreeRequest>,
    ) -> Result<Response<CreateSandboxTreeResponse>, Status> {
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
                let (mut repo, workspace) =
                    handle.workspace_repository(atomic_repository::WorkspaceTxnMode::Reconcile)?;
                let working_copy = workspace.working_copy();
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
                    .provision_sandbox(working_copy, &dest, &view)
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
        _request: Request<MaterializeRequest>,
    ) -> Result<Response<Self::MaterializeStream>, Status> {
        Err(Status::unimplemented("Materialize lands with its slice"))
    }

    async fn get_file_states(
        &self,
        _request: Request<GetFileStatesRequest>,
    ) -> Result<Response<GetFileStatesResponse>, Status> {
        Err(Status::unimplemented("GetFileStates lands with its slice"))
    }

    async fn get_changes(
        &self,
        _request: Request<GetChangesRequest>,
    ) -> Result<Response<GetChangesResponse>, Status> {
        Err(Status::unimplemented("GetChanges lands with its slice"))
    }

    async fn publish_provenance(
        &self,
        _request: Request<PublishProvenanceRequest>,
    ) -> Result<Response<PublishProvenanceResponse>, Status> {
        Err(Status::unimplemented(
            "PublishProvenance lands with its slice",
        ))
    }

    async fn submit_change(
        &self,
        _request: Request<SubmitChangeRequest>,
    ) -> Result<Response<SubmitChangeResponse>, Status> {
        Err(Status::unimplemented("SubmitChange lands with its slice"))
    }
}

//! TriageService handlers — the promotion-review surface.
//!
//! `ListTriageCandidates` runs the repository's candidate-set domain call
//! (the view diff, the dependency-closure additions, and the baggage
//! classification) plus the view-pair resolution the local bodies
//! perform, and carries the domain JSON so the CLI renders the local
//! reports. `GenerateTriageReview` builds the canonical report
//! (`atomic_repository::triage::build_report` — the projection moved out
//! of the CLI) and carries it the same way: the dashboard, walkthrough,
//! HTML, attest export and outpost's readers all render client-side over
//! the wire-carried domain report.

use std::sync::Arc;

use crate::atomic::ErrorCode;
use crate::atomic::*;
use atomic_repository::Repository;
use tonic::{Request, Response, Status};

use super::state::{domain_status, repository_error, DaemonState};

pub struct TriageImpl {
    pub state: Arc<DaemonState>,
}

/// The local `resolve_views` semantics — the (feature, target) pair with
/// the same defaults (current view; its parent) and the same errors
/// (unknown view, root view without --into).
fn resolve_triage_views(
    repo: &Repository,
    feature: Option<&str>,
    into: Option<&str>,
) -> Result<(String, String), Status> {
    let source = match feature {
        Some(name) => {
            require_triage_view(repo, name, "<VIEW>")?;
            name.to_string()
        }
        None => repo.current_view().to_string(),
    };
    let target = match into {
        Some(name) => {
            require_triage_view(repo, name, "--into")?;
            name.to_string()
        }
        None => match repo.parent_change_count(&source) {
            Ok(Some((parent, _))) => parent,
            Ok(None) => {
                return Err(domain_status(
                    ErrorCode::InvalidArgument,
                    format!(
                        "'{source}' is a root view — it has no parent to promote into.\n  \
                         Pass --into <view> to choose a target (see 'atomic view list')."
                    ),
                ));
            }
            Err(atomic_repository::RepositoryError::ViewNotFound { name }) => {
                return Err(unknown_triage_view(repo, &name, "<VIEW>"));
            }
            Err(error) => return Err(repository_error(error)),
        },
    };
    Ok((source, target))
}

fn require_triage_view(repo: &Repository, name: &str, arg: &str) -> Result<(), Status> {
    if repo.view_exists(name).map_err(repository_error)? {
        Ok(())
    } else {
        Err(unknown_triage_view(repo, name, arg))
    }
}

fn unknown_triage_view(repo: &Repository, name: &str, arg: &str) -> Status {
    domain_status(
        ErrorCode::InvalidArgument,
        format!(
            "No view named '{name}' — {arg} takes a real view name, not a placeholder.\n  \
             Current view is '{}'; omit {arg} to triage that instead, or run \
             'atomic view list' to see all views.",
            repo.current_view()
        ),
    )
}

#[tonic::async_trait]
impl triage_service_server::TriageService for TriageImpl {
    async fn list_triage_candidates(
        &self,
        request: Request<ListTriageCandidatesRequest>,
    ) -> Result<Response<ListTriageCandidatesResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ListTriageCandidates", Some(&handle));
        let feature_arg = request.from_view.clone();
        let into_arg = request.to_view.clone();
        let (feature, target, payload) = tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            let (feature, target) =
                resolve_triage_views(&repo, feature_arg.as_deref(), into_arg.as_deref())?;
            let set = repo
                .triage_candidate_set(&feature, &target)
                .map_err(repository_error)?;
            let payload = serde_json::to_vec(&set)
                .map_err(|error| Status::internal(format!("candidate set encode: {error}")))?;
            Ok::<_, Status>((feature, target, payload))
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(ListTriageCandidatesResponse {
            candidates: Vec::new(),
            candidate_set: Some(VersionedBytes {
                schema: "atomic.triage.candidates.v1".to_string(),
                payload,
            }),
            feature,
            target,
        }))
    }

    async fn generate_triage_review(
        &self,
        request: Request<GenerateTriageReviewRequest>,
    ) -> Result<Response<GenerateTriageReviewResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("GenerateTriageReview", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let feature_arg = request.from_view.clone();
        let into_arg = request.to_view.clone();
        let report = tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            // Empty strings read as "not given" — the same defaults the
            // local `resolve_views` applies (current view; its parent).
            let feature = (!feature_arg.is_empty()).then_some(feature_arg.as_str());
            let into = (!into_arg.is_empty()).then_some(into_arg.as_str());
            let (feature, target) = resolve_triage_views(&repo, feature, into)?;
            let report = atomic_repository::triage::build_report(&repo, &feature, &target)
                .map_err(repository_error)?;
            let payload = serde_json::to_vec(&report)
                .map_err(|error| Status::internal(format!("triage report encode: {error}")))?;
            Ok::<_, Status>(payload)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        // The report is the whole answer (the walkthrough layers and risks
        // above are a parser-free projection a caller may want later; the
        // CLI's renders and outpost's readers all consume the report).
        Ok(Response::new(GenerateTriageReviewResponse {
            layers: Vec::new(),
            risks: Vec::new(),
            report: Some(VersionedBytes {
                schema: "atomic.triage.report.v1".to_string(),
                payload: report,
            }),
        }))
    }
}

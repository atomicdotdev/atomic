//! TagService handlers: `atomic tag create|delete|list|show` — the domain
//! half (the exact repository calls the local CLI bodies make), with the
//! wire carrying the full tag record so the CLI renders the local reports.
//!
//! ONE implementation: the atomicd transport serves these handlers over the
//! socket and the CLI's local service mode calls them in-process. The
//! create flow reproduces the local body exactly — the existing-tag
//! pre-check, the `--force` delete, and the Release kind.

use std::sync::Arc;

use crate::atomic::ErrorCode;
use crate::atomic::*;
use atomic_core::types::Base32;
use atomic_core::types::Hash;
use atomic_repository::TagKind;
use atomic_repository::TagRecord;
use tonic::{Request, Response, Status};

use super::convert::{hash_proto, timestamp_proto};
use super::services_agent::response_meta;
use super::state::{domain_status, repository_error, DaemonState};

pub struct TagImpl {
    pub state: Arc<DaemonState>,
}

/// The domain tag record → the wire's full TagInfo (add-only fields ride
/// the wire so the CLI renders the local listing/show lines).
fn tag_info(tag: &TagRecord) -> TagInfo {
    TagInfo {
        name: tag.name.clone(),
        state: Some(hash_proto(&tag.state)),
        message: tag.message.clone(),
        created_at: Some(timestamp_proto(tag.timestamp)),
        view: tag.view.clone(),
        sequence: tag.sequence,
        kind: tag.kind.to_string(),
        author: tag.author.as_ref().map(|author| Author {
            name: author.name.clone(),
            email: author.email.clone(),
        }),
        metadata: tag.metadata.as_ref().map(|value| value.to_string()),
    }
}

fn tag_already_exists(name: &str) -> Status {
    domain_status(
        ErrorCode::InvalidArgument,
        format!("Tag '{name}' already exists. Use --force to overwrite."),
    )
}

/// The create-tag domain error mapping — the local body's exact cases.
fn create_tag_error(error: atomic_repository::RepositoryError, name: &str) -> Status {
    match error {
        atomic_repository::RepositoryError::TagAlreadyExists { .. } => tag_already_exists(name),
        atomic_repository::RepositoryError::InvalidTagName { name, reason } => domain_status(
            ErrorCode::InvalidArgument,
            format!("Invalid tag name '{name}': {reason}"),
        ),
        atomic_repository::RepositoryError::ViewNotFound { name } => {
            domain_status(ErrorCode::View, format!("view '{name}' does not exist"))
        }
        other => repository_error(other),
    }
}

#[tonic::async_trait]
impl tag_service_server::TagService for TagImpl {
    async fn create_tag(
        &self,
        request: Request<CreateTagRequest>,
    ) -> Result<Response<CreateTagResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("CreateTag", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let name = request.name.clone();
        let message = request.message.clone();
        let force = request.force;
        let tag = tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            // The local body's pre-check: refuse an existing tag unless
            // --force deleted it first.
            if let Ok(Some(_)) = repo.get_tag(&name) {
                if force {
                    let _ = repo.delete_tag(&name);
                } else {
                    return Err(tag_already_exists(&name));
                }
            }
            let tag = repo
                .create_tag(&name, message.as_deref(), TagKind::Release)
                .map_err(|error| create_tag_error(error, &name))?;
            Ok::<_, Status>(tag)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(CreateTagResponse {
            tag: Some(tag_info(&tag)),
            meta: response_meta(&meta),
        }))
    }

    async fn delete_tag(
        &self,
        request: Request<DeleteTagRequest>,
    ) -> Result<Response<DeleteTagResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("DeleteTag", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let meta = request.meta.clone();
        let name = request.name.clone();
        let deleted = tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            repo.delete_tag(&name).map_err(repository_error)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(DeleteTagResponse {
            deleted,
            meta: response_meta(&meta),
        }))
    }

    async fn list_tags(
        &self,
        request: Request<ListTagsRequest>,
    ) -> Result<Response<ListTagsResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ListTags", Some(&handle));
        let view = request.view.clone();
        let pattern = request.pattern.clone();
        let annotated_only = request.annotated_only;
        let tags = tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            // The local listing's domain call: a view filter switches to
            // that view's tag list, otherwise every tag across views.
            let mut tags = match &view {
                Some(view) => repo.list_tags_for_view(view).map_err(repository_error)?,
                None => repo.list_all_tags().map_err(repository_error)?,
            };
            // The local body's filters, same semantics: the star-stripped
            // substring pattern match, and the annotated-only keep.
            if let Some(pattern) = &pattern {
                let pat = pattern.replace('*', "");
                tags.retain(|tag| tag.name.contains(&pat));
            }
            if annotated_only {
                tags.retain(|tag| tag.is_annotated());
            }
            Ok::<_, Status>(tags)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(ListTagsResponse {
            tags: tags.iter().map(tag_info).collect(),
        }))
    }

    async fn get_tag(
        &self,
        request: Request<GetTagRequest>,
    ) -> Result<Response<GetTagResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("GetTag", Some(&handle));
        let name = request.name.clone();
        let (tag, records) = tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            let Some(tag) = repo.get_tag(&name).map_err(repository_error)? else {
                return Ok::<_, Status>((None, Vec::new()));
            };
            // The ReviewGate metadata render reads each record hash's
            // presence and view membership — the same domain reads the
            // local show body performs, computed here so the CLI renders.
            let mut records = Vec::new();
            if tag.kind == TagKind::ReviewGate {
                if let Some(metadata) = &tag.metadata {
                    if let Some(original) = metadata
                        .get("changes")
                        .and_then(|changes| changes.get("original_hashes"))
                        .and_then(|value| value.as_array())
                    {
                        for entry in original {
                            let Some(text) = entry.as_str() else { continue };
                            match Hash::from_base32(text.as_bytes()) {
                                Some(hash) => {
                                    let present = repo.has_change(&hash);
                                    let views =
                                        repo.views_containing_change(&hash).unwrap_or_default();
                                    records.push(TagRecordPresence {
                                        hash: text.to_string(),
                                        present,
                                        views,
                                        parseable: true,
                                    });
                                }
                                None => records.push(TagRecordPresence {
                                    hash: text.to_string(),
                                    present: false,
                                    views: Vec::new(),
                                    parseable: false,
                                }),
                            }
                        }
                    }
                }
            }
            Ok((Some(tag_info(&tag)), records))
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(GetTagResponse { tag, records }))
    }
}

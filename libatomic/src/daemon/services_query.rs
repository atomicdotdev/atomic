//! Additional query-surface handlers landed after the first full-turn slice:
//! working-copy diff streaming, change lookup, session ledger, view listing,
//! goal listing, and KG neighbors — each surfaced by the live harness audit.

use std::sync::Arc;

use crate::atomic::ErrorCode;
use crate::atomic::*;
use atomic_core::pristine::ViewTxnT;
use atomic_core::types::Base32;
use atomic_repository::status::{FileStatus as DomainFileStatus, StatusOptions};
use atomic_repository::Repository;
use tonic::{Request, Response, Status};

use super::convert::hash_proto;
use super::state::{domain_status, repository_error, DaemonState};

// ---------------------------------------------------------------------------
// DiffWorkingCopy (streaming) — `atomic diff`
// ---------------------------------------------------------------------------

/// The handler body, shared with the trait impl in `services.rs`.
pub async fn diff_working_copy_impl(
    state: &Arc<DaemonState>,
    request: Request<DiffWorkingCopyRequest>,
) -> Result<Response<<super::services::QueryImpl as repository_query_service_server::RepositoryQueryService>::DiffWorkingCopyStream>, Status>{
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("DiffWorkingCopy", Some(&handle));
    let scope = request.working_copy.unwrap_or_default();
    let stat_only = request.stat_only;
    let paths = scope.paths;

    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<DiffChunk, Status>>(16);
    let handle_for_task = handle;
    tokio::spawn(async move {
        let gate_handle = handle_for_task.clone();
        let _gate = gate_handle.exclusive().await;
        let result = tokio::task::spawn_blocking(move || {
            compute_working_copy_diff(&handle_for_task, &paths, scope.include_untracked, stat_only)
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
    Ok(Response::new(Box::pin(
        tokio_stream::wrappers::ReceiverStream::new(receiver),
    )))
}

/// Compute the working-copy diff chunks: recorded graph content (via
/// `get_file_content_on_view`) against the on-disk bytes, unified-diffed
/// with `similar` — the same comparison the CLI's local path performs.
fn compute_working_copy_diff(
    handle: &super::state::RepoHandle,
    paths: &[String],
    include_untracked: bool,
    stat_only: bool,
) -> Result<Vec<DiffChunk>, Status> {
    // A query: read-only open with the read-concurrency wait.
    let repo = handle.repository_readonly()?;
    let view = repo.current_view().to_string();
    let status = repo
        .status(StatusOptions::default())
        .map_err(repository_error)?;

    let mut targets: Vec<(std::path::PathBuf, DomainFileStatus)> = Vec::new();
    for entry in status.entries() {
        let entry_status = entry.status();
        let wanted = match entry_status {
            DomainFileStatus::Modified | DomainFileStatus::Added | DomainFileStatus::Deleted => {
                true
            }
            DomainFileStatus::Untracked => include_untracked,
            _ => false,
        };
        if !wanted {
            continue;
        }
        if !paths.is_empty() && !paths.iter().any(|p| entry.path().starts_with(p)) {
            continue;
        }
        targets.push((entry.path().to_path_buf(), entry_status));
    }

    let mut chunks = Vec::new();
    for (path, file_status) in targets {
        let path_str = path.display().to_string();
        let old_content: Vec<u8> = match file_status {
            DomainFileStatus::Added | DomainFileStatus::Untracked => Vec::new(),
            _ => repo
                .get_file_content_on_view(&path, &view)
                .map_err(repository_error)?
                .unwrap_or_default(),
        };
        let new_content: Option<Vec<u8>> = match file_status {
            DomainFileStatus::Deleted => None,
            _ => std::fs::read(handle.root.join(&path)).ok(),
        };

        let old_text = String::from_utf8_lossy(&old_content).into_owned();
        let new_text = match &new_content {
            Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            None => String::new(),
        };
        let old_binary = old_content.contains(&0);
        let new_binary = new_content
            .as_ref()
            .map(|c| c.contains(&0))
            .unwrap_or(false);
        if old_binary || new_binary {
            chunks.push(DiffChunk {
                path: path_str,
                patch: None,
                additions: 0,
                deletions: 0,
                binary: true,
                status: Some(diff_status_string(file_status)),
                old_content: None,
                new_content: None,
            });
            continue;
        }

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
            (Some(old_content.clone()), new_content.clone())
        };
        chunks.push(DiffChunk {
            path: path_str,
            patch,
            additions,
            deletions,
            binary: false,
            status: Some(diff_status_string(file_status)),
            old_content: old_payload,
            new_content: new_payload,
        });
    }
    Ok(chunks)
}

/// The wire's downcased file status for a diff chunk — the CLI renders
/// name-status and /dev/null headers from this.
fn diff_status_string(status: DomainFileStatus) -> String {
    match status {
        DomainFileStatus::Added => "added",
        DomainFileStatus::Deleted => "deleted",
        DomainFileStatus::Modified => "modified",
        DomainFileStatus::Untracked => "untracked",
        other => return format!("{other:?}").to_lowercase(),
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// GetChange — `atomic change <HASH|#N>`
// ---------------------------------------------------------------------------

pub async fn get_change_impl(
    state: &Arc<DaemonState>,
    request: Request<GetChangeRequest>,
) -> Result<Response<GetChangeResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("GetChange", Some(&handle));
    let reference = request
        .r#ref
        .and_then(|r| r.kind)
        .ok_or_else(|| domain_status(ErrorCode::InvalidArgument, "change ref required"))?;
    let view = request
        .view
        .as_ref()
        .map(|v| v.name.clone().unwrap_or_default());

    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let handle_for_task = handle.clone();
    let wants_bundle = request.includes.as_ref().is_some_and(|i| i.metadata);
    let wants_file_contents = request.include_file_contents;
    let change = tokio::task::spawn_blocking(move || {
        // A query: read-only open with the read-concurrency wait.
        let repo = handle_for_task.repository_readonly()?;
        let (hash, sequence) = match reference {
            change_ref::Kind::Hash(hash) => {
                let mut bytes = [0u8; 32];
                if hash.value.len() != 32 {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        "hash must be 32 bytes",
                    ));
                }
                bytes.copy_from_slice(&hash.value);
                (atomic_core::types::Merkle(bytes), None)
            }
            change_ref::Kind::Sequence(sequence) => {
                let view_name = view
                    .clone()
                    .unwrap_or_else(|| repo.current_view().to_string());
                let txn = repo
                    .pristine()
                    .read_txn()
                    .map_err(|error| domain_status(ErrorCode::Repository, error.to_string()))?;
                let view_state = txn
                    .get_view(&view_name)
                    .map_err(|error| domain_status(ErrorCode::View, error.to_string()))?
                    .ok_or_else(|| {
                        domain_status(ErrorCode::View, format!("view '{view_name}' not found"))
                    })?;
                let entry =
                    atomic_repository::history::get_change_at_sequence(&txn, &view_state, sequence)
                        .map_err(|error| domain_status(ErrorCode::NotFound, error.to_string()))?;
                (entry.hash, Some(sequence))
            }
            change_ref::Kind::Prefix(prefix) => {
                // Case-insensitive unique-prefix resolution over the whole
                // change store — the same resolver `insert` uses; the
                // caller's membership/guard errors come from the target op.
                let prefix = prefix.trim().to_ascii_uppercase();
                if prefix.is_empty() {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        "change prefix required",
                    ));
                }
                match repo.find_change_by_prefix(&prefix) {
                    Ok(Some(hash)) => (hash, None),
                    Ok(None) => {
                        return Err(domain_status(
                            ErrorCode::NotFound,
                            format!("no change found matching '{prefix}'"),
                        ))
                    }
                    Err(atomic_repository::RepositoryError::AmbiguousHash { prefix, matches }) => {
                        return Err(domain_status(
                            ErrorCode::NotFound,
                            format!(
                                "ambiguous change prefix '{prefix}' (matches: {})",
                                matches.join(", ")
                            ),
                        ))
                    }
                    Err(error) => {
                        return Err(domain_status(ErrorCode::Repository, error.to_string()))
                    }
                }
            }
        };
        let change = repo
            .load_change(&hash)
            .map_err(|error| domain_status(ErrorCode::NotFound, error.to_string()))?;
        // The complete change, V3-serialized, plus the causal decision
        // graph(s) explaining it — the client's full-detail renders
        // (change ledger, hunks, JSON) deserialize these with atomic-core.
        let bundle = if wants_bundle {
            let mut buffer = Vec::new();
            change
                .serialize(&mut buffer)
                .map_err(|error| domain_status(ErrorCode::Repository, error.to_string()))?;
            Some(VersionedBytes {
                schema: "atomic.change.v3".to_string(),
                payload: buffer,
            })
        } else {
            None
        };
        let ledger = repo
            .find_provenance_for_change(&hash)
            .map_err(|error| domain_status(ErrorCode::Repository, error.to_string()))?
            .into_iter()
            .map(|(_graph_hash, graph)| {
                let payload = serde_json::to_vec(&graph)
                    .map_err(|error| domain_status(ErrorCode::Internal, error.to_string()))?;
                Ok::<VersionedBytes, Status>(VersionedBytes {
                    schema: "atomic.prov.graph.v1".to_string(),
                    payload,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Per-file content reconstruction (`diff -c`): each touched
        // file's pre/post-change bytes as the graph holds them — the
        // legacy no-file_ops fallback AND the FileOps hunks' context
        // padding read server-side; the client renders.
        let file_contents = if wants_file_contents {
            let mut paths: Vec<String> = atomic_repository::get_files_in_change(&change);
            for ops in change.file_ops() {
                paths.push(ops.path().to_string());
            }
            paths.sort();
            paths.dedup();
            paths
                .into_iter()
                .map(|path| {
                    let before = repo
                        .get_file_content_before_change(&path, &hash)
                        .ok()
                        .flatten();
                    let after = repo
                        .get_file_content_after_change(&path, &hash)
                        .ok()
                        .flatten();
                    ChangeFileContent {
                        path,
                        before,
                        after,
                    }
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        Ok::<_, Status>((change, hash, sequence, bundle, ledger, file_contents))
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;

    let (change, hash, sequence, change_bundle, provenance_ledger, file_contents) = change;
    let header = &change.hashed.header;
    Ok(Response::new(GetChangeResponse {
        change: Some(ChangeInfo {
            hash: Some(hash_proto(&hash)),
            message: Some(header.message.clone()),
            description: header.description.clone(),
            authors: header
                .authors
                .iter()
                .map(|author| Author {
                    name: author.name.clone(),
                    email: author.email.clone(),
                })
                .collect(),
            recorded_at: Some(prost_types::Timestamp {
                seconds: header.timestamp.timestamp(),
                nanos: header.timestamp.timestamp_subsec_nanos() as i32,
            }),
            dependencies: change.hashed.dependencies.iter().map(hash_proto).collect(),
            graph_section_count: 0,
            semantic_section_count: 0,
            content_chunk_count: 0,
            has_provenance: change.has_provenance(),
            has_unhashed: change.unhashed.is_some(),
            has_signature: change.signature.is_some(),
        }),
        graph_section: None,
        semantic_section: None,
        signatures: Vec::new(),
        content_chunks: None,
        change_bundle,
        provenance_ledger,
        sequence,
        file_contents,
    }))
}

// ---------------------------------------------------------------------------
// GetSession — `atomic session show`
// ---------------------------------------------------------------------------

pub async fn get_session_impl(
    state: &Arc<DaemonState>,
    request: Request<GetSessionRequest>,
) -> Result<Response<GetSessionResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("GetSession", Some(&handle));
    let session_id = request.session_id.clone();

    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let handle_for_task = handle.clone();
    let ledger = tokio::task::spawn_blocking(move || {
        let repo = handle_for_task.repository()?;
        let ledger = repo
            .get_session_ledger(&session_id)
            .map_err(repository_error)?;
        // Turn → intent resolution: explicit plan ids ride the turns; vault
        // session links come from the manifest (the same map the client
        // built locally — one implementation, served here).
        let turn_intents = match &ledger {
            Some((_record, turns)) => turns
                .iter()
                .filter_map(|turn| {
                    let intent = turn
                        .plan_id
                        .clone()
                        .or_else(|| vault_turn_intent(&repo, &session_id, turn.turn_number))?;
                    Some(SessionTurnIntent {
                        turn: turn.turn_number,
                        intent,
                    })
                })
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        let head_hash = repo.get_session_head(&session_id).ok().flatten();
        let manifest_head = head_hash.as_ref().map(hash_proto);
        let (parent_manifest, fork_turn) = head_hash
            .as_ref()
            .and_then(|head| repo.get_session_manifest(head).ok().flatten())
            .map(|manifest| {
                (
                    manifest.parent_session.as_ref().map(hash_proto),
                    manifest.fork_turn,
                )
            })
            .unwrap_or((None, None));
        Ok::<_, Status>((
            ledger,
            turn_intents,
            manifest_head,
            parent_manifest,
            fork_turn,
        ))
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))??;

    let (ledger, turn_intents, manifest_head, parent_manifest, fork_turn) = ledger;
    let Some((record, turns)) = ledger else {
        return Err(domain_status(ErrorCode::NotFound, "session not found"));
    };
    // The complete ledger as versioned-opaque bytes: JSON of the
    // {record, turns} pair — the client's listings/renders deserialize the
    // domain shape it knows (atomic.session.ledger.v1).
    let ledger_bundle = serde_json::to_vec(&(record, &turns))
        .ok()
        .map(|payload| VersionedBytes {
            schema: "atomic.session.ledger.v1".to_string(),
            payload,
        });
    let turns = turns
        .into_iter()
        .filter(|turn| {
            let from = request.from_turn.unwrap_or(0);
            let to = request.to_turn.unwrap_or(u32::MAX);
            turn.turn_number >= from && turn.turn_number <= to
        })
        .map(stored_turn_proto)
        .collect();
    Ok(Response::new(GetSessionResponse {
        turns,
        ledger_bundle,
        turn_intents,
        manifest_head,
        parent_manifest,
        fork_turn,
    }))
}

/// The vault-manifest turn→intent map for one session (the stored `turn` is
/// `turn_count + 1` at creation, so subtract one for the ledger's 0-indexed
/// turn number — mirrors the vault's own resolution).
fn vault_turn_intent(repo: &Repository, session_id: &str, turn_number: u32) -> Option<String> {
    let manifest = repo.vault_manifest().ok()?;
    for (key, summary) in &manifest.intents {
        if summary.session.as_deref() != Some(session_id) {
            continue;
        }
        if summary.turn.and_then(|t| t.checked_sub(1)) == Some(turn_number) {
            return Some(if summary.human_key.is_empty() {
                key.clone()
            } else {
                summary.human_key.clone()
            });
        }
    }
    None
}

/// Map a stored session turn to the wire shape (the fields the ledger owns:
/// turn number, provenance hash, and the turn timestamp).
fn stored_turn_proto(turn: atomic_core::change::session::SessionTurn) -> StoredProvenanceTurn {
    StoredProvenanceTurn {
        schema_version: 1,
        provenance_id: 0,
        session_id: turn.session_id.clone(),
        turn_number: turn.turn_number,
        state: ProvenanceTurnState::Unspecified as i32,
        generation: 0,
        next_event_seq: 0,
        created_at: Some(prost_types::Timestamp {
            seconds: turn.timestamp,
            nanos: 0,
        }),
        updated_at: None,
        completed_at: None,
        final_hash: Some(hash_proto(&turn.provenance_hash)),
        checkpoint_attempt: None,
        stop_state: None,
    }
}

// ---------------------------------------------------------------------------
// ListViews — `atomic view list`
// ---------------------------------------------------------------------------

pub struct ViewImpl {
    pub state: Arc<DaemonState>,
}

#[tonic::async_trait]
impl view_service_server::ViewService for ViewImpl {
    async fn list_views(
        &self,
        request: Request<ListViewsRequest>,
    ) -> Result<Response<ListViewsResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("ListViews", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let handle_for_task = handle.clone();
        let views = tokio::task::spawn_blocking(move || {
            // A query: read-only open with the read-concurrency wait.
            let repo = handle_for_task.repository_readonly()?;
            let names = repo.list_views().map_err(repository_error)?;
            let mut infos = Vec::with_capacity(names.len());
            for name in names {
                infos.push(repo.get_view_info(&name).map_err(repository_error)?);
            }
            Ok::<_, Status>(infos)
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;

        let current = handle.current_view();
        let views = views
            .into_iter()
            .map(|info| {
                let name = info.name.clone();
                ViewInfo {
                    name: name.clone(),
                    scope: match info.scope {
                        atomic_core::pristine::ViewScope::Draft => ViewScope::Draft,
                        atomic_core::pristine::ViewScope::Shared => ViewScope::Shared,
                    } as i32,
                    parent: info.parent_name,
                    head: Some(hash_proto(&info.state)),
                    change_count: info.change_count,
                    current: name == current,
                    r#ref: Some(ViewRef {
                        view_id: blake3::hash(name.as_bytes()).as_bytes().to_vec(),
                        name: Some(name),
                    }),
                    snapshot: None,
                    // The own/inherited split `view list --json` carries:
                    // change_count reads as their sum (the effective
                    // total) — the same classification the local body
                    // computes per view.
                    own_change_count: Some(info.own_change_count),
                    inherited_change_count: Some(info.inherited_change_count),
                }
            })
            .collect();
        Ok(Response::new(ListViewsResponse { views }))
    }

    async fn create_view(
        &self,
        request: Request<CreateViewRequest>,
    ) -> Result<Response<CreateViewResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("CreateView", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let handle_for_task = handle.clone();
        let view =
            tokio::task::spawn_blocking(move || -> Result<(String, usize, String), Status> {
                let mut repo = handle_for_task.repository()?;
                let name = request.name.trim().to_string();
                validate_view_name(&name)?;
                if repo.view_exists(&name).map_err(repository_error)? {
                    return Err(domain_status(
                        ErrorCode::View,
                        format!("view '{name}' already exists"),
                    ));
                }
                // The overlay-chain mapping (views.rs): from_view anchors
                // AND seeds on the source; empty/default anchors on the
                // nearest SHARED ancestor with no seed — sibling drafts
                // must not see each other through the overlay.
                let (anchor, seed) = match request.base {
                    Some(create_view_request::Base::FromView(source)) => {
                        if !repo.view_exists(&source).map_err(repository_error)? {
                            return Err(domain_status(
                                ErrorCode::View,
                                format!("view '{source}' not found"),
                            ));
                        }
                        (Some(source.clone()), Some(source))
                    }
                    // `--parent`: anchor on the parent WITHOUT seeding —
                    // the draft workspace form.
                    Some(create_view_request::Base::Parent(parent)) => {
                        if !repo.view_exists(&parent).map_err(repository_error)? {
                            return Err(domain_status(
                                ErrorCode::View,
                                format!("view '{parent}' not found"),
                            ));
                        }
                        (Some(parent), None)
                    }
                    Some(create_view_request::Base::Empty(_)) | None => (None, None),
                };
                repo.create_overlay_view(&name, anchor.as_deref(), seed.as_deref())
                    .map_err(repository_error)?;
                let info = repo.get_view_info(&name).map_err(repository_error)?;
                Ok((
                    name,
                    info.change_count as usize,
                    info.parent_name.unwrap_or_default(),
                ))
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))??;
        let (name, change_count, parent) = view;
        let current = handle.current_view();
        Ok(Response::new(CreateViewResponse {
            view: Some(ViewInfo {
                name: name.clone(),
                scope: ViewScope::Unspecified as i32,
                parent: if parent.is_empty() {
                    None
                } else {
                    Some(parent)
                },
                head: None,
                change_count: change_count as u64,
                current: name == current,
                own_change_count: Some(change_count as u64),
                inherited_change_count: Some(0),
                r#ref: Some(ViewRef {
                    view_id: blake3::hash(name.as_bytes()).as_bytes().to_vec(),
                    name: Some(name),
                }),
                snapshot: None,
            }),
            meta: None,
        }))
    }

    async fn switch_view(
        &self,
        request: Request<SwitchViewRequest>,
    ) -> Result<Response<SwitchViewResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("SwitchView", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let handle_for_task = handle.clone();
        let files_written = tokio::task::spawn_blocking(move || -> Result<u64, Status> {
            let mut repo = handle_for_task.repository()?;
            let name = request.view.trim().to_string();
            if !repo.view_exists(&name).map_err(repository_error)? {
                return Err(domain_status(
                    ErrorCode::View,
                    format!("view '{name}' not found"),
                ));
            }
            // The CLI's safety gate, verbatim: refuse to switch away
            // from a dirty working copy — unless the caller explicitly
            // bypassed it (--force/--stash, an informed client decision).
            if !request.bypass_dirty_check.unwrap_or(false)
                && handle_for_task.current_view() != name
            {
                let status = repo
                    .status(StatusOptions::default())
                    .map_err(repository_error)?;
                if !status.is_clean() {
                    return Err(domain_status(
                        ErrorCode::PreconditionFailed,
                        "working copy has unrecorded changes — record them or \
                             switch with --force/--stash from the client",
                    ));
                }
            }
            let result = repo.switch_view(&name).map_err(repository_error)?;
            Ok(result.files_written as u64)
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;
        Ok(Response::new(SwitchViewResponse {
            files_written,
            meta: None,
        }))
    }

    async fn delete_view(
        &self,
        request: Request<DeleteViewRequest>,
    ) -> Result<Response<DeleteViewResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("DeleteView", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let handle_for_task = handle.clone();
        tokio::task::spawn_blocking(move || -> Result<(), Status> {
            let mut repo = handle_for_task.repository()?;
            let name = request.view.trim().to_string();
            // Never switch implicitly: refuse the current view.
            if handle_for_task.current_view() == name {
                return Err(domain_status(
                    ErrorCode::View,
                    format!("cannot delete the current view '{name}' — switch first"),
                ));
            }
            if !repo.view_exists(&name).map_err(repository_error)? {
                return Err(domain_status(
                    ErrorCode::View,
                    format!("view '{name}' not found"),
                ));
            }
            repo.delete_view(&name).map_err(repository_error)?;
            Ok(())
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;
        Ok(Response::new(DeleteViewResponse { meta: None }))
    }

    async fn set_view_scope(
        &self,
        request: Request<SetViewScopeRequest>,
    ) -> Result<Response<SetViewScopeResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("SetViewScope", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let handle_for_task = handle.clone();
        tokio::task::spawn_blocking(move || -> Result<(), Status> {
            let repo = handle_for_task.repository()?;
            let name = request.view.trim().to_string();
            if !repo.view_exists(&name).map_err(repository_error)? {
                return Err(domain_status(
                    ErrorCode::View,
                    format!("view '{name}' not found"),
                ));
            }
            let scope = match request.scope {
                x if x == ViewScope::Draft as i32 => atomic_core::pristine::ViewScope::Draft,
                x if x == ViewScope::Shared as i32 => atomic_core::pristine::ViewScope::Shared,
                _ => {
                    return Err(domain_status(
                        ErrorCode::InvalidArgument,
                        "view scope is required (DRAFT or SHARED)",
                    ))
                }
            };
            repo.set_view_scope(&name, scope)
                .map_err(repository_error)?;
            Ok(())
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;
        Ok(Response::new(SetViewScopeResponse { meta: None }))
    }

    async fn split_view(
        &self,
        request: Request<SplitViewRequest>,
    ) -> Result<Response<SplitViewResponse>, Status> {
        let request = request.into_inner();
        let handle = self.state.resolve(request.repository.as_ref().unwrap())?;
        self.state.log_rpc("SplitView", Some(&handle));
        let gate_handle = handle.clone();
        let _gate = gate_handle.exclusive().await;
        let handle_for_task = handle.clone();
        let result = tokio::task::spawn_blocking(
            move || -> Result<SplitViewResponse, Status> {
                let mut repo = handle_for_task.repository()?;
                let mut changes = Vec::with_capacity(request.changes.len());
                for hash in &request.changes {
                    let mut bytes = [0u8; 32];
                    if hash.value.len() != 32 {
                        return Err(domain_status(
                            ErrorCode::InvalidArgument,
                            "hash must be 32 bytes",
                        ));
                    }
                    bytes.copy_from_slice(&hash.value);
                    changes.push(atomic_core::types::Merkle(bytes));
                }
                let outcome = repo
                    .split_view(atomic_repository::SplitOptions {
                        target_view: request.name.clone(),
                        from_view: if request.from_view.is_empty() {
                            None
                        } else {
                            Some(request.from_view.clone())
                        },
                        changes,
                        cascade: request.cascade,
                        dry_run: request.dry_run,
                        materialize: request.materialize,
                    })
                    .map_err(repository_error)?;
                // A blocked split is a domain refusal carrying the
                // dependent hashes — never a partial mutation.
                if outcome.blocked {
                    let dependents = outcome
                        .dependents
                        .iter()
                        .map(|c| c.hash.to_base32())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(domain_status(
                        ErrorCode::PreconditionFailed,
                        format!(
                            "split blocked: {} change(s) remaining in '{}' depend on the split-out set: {dependents}",
                            outcome.dependents.len(),
                            outcome.from_view,
                        ),
                    ));
                }
                let map = |changes: &[atomic_repository::SplitChange]| {
                    changes
                        .iter()
                        .map(|c| SplitChangeInfo {
                            hash: Some(hash_proto(&c.hash)),
                            sequence: c.sequence,
                        })
                        .collect::<Vec<_>>()
                };
                // A dry run creates nothing: the report below is a
                // preview, so there is no target view to describe.
                let view_info = if request.dry_run {
                    None
                } else {
                    Some(
                        repo.get_view_info(&outcome.target_view)
                            .map_err(repository_error)?,
                    )
                };
                Ok(SplitViewResponse {
                    dry_run: request.dry_run,
                    requested: map(&outcome.requested),
                    dependents: map(&outcome.dependents),
                    moved: map(&outcome.moved),
                    source_change_count: outcome.source_change_count,
                    target_change_count: outcome.target_change_count,
                    working_copy_updated: outcome.working_copy_updated,
                    files_written: outcome.files_written as u64,
                    files_removed: outcome.files_removed as u64,
                    view: view_info.map(|info| ViewInfo {
                        name: outcome.target_view.clone(),
                        scope: ViewScope::Unspecified as i32,
                        parent: info.parent_name.clone(),
                        head: None,
                        change_count: outcome.target_change_count,
                        current: handle_for_task.current_view() == outcome.target_view,
                        own_change_count: Some(outcome.target_change_count),
                        inherited_change_count: Some(
                            info.inherited_change_count.saturating_add(
                                (info.change_count).saturating_sub(info.own_change_count),
                            ),
                        ),
                        r#ref: Some(ViewRef {
                            view_id: blake3::hash(outcome.target_view.as_bytes()).as_bytes().to_vec(),
                            name: Some(outcome.target_view.clone()),
                        }),
                        snapshot: None,
                    }),
                    meta: None,
                })
            },
        )
        .await
        .map_err(|error| Status::internal(error.to_string()))??;
        Ok(Response::new(result))
    }
}

/// The CLI's view-name rules (ported so the daemon refuses the same
/// names the local path would).
fn validate_view_name(name: &str) -> Result<(), Status> {
    if name.is_empty() {
        return Err(domain_status(
            ErrorCode::InvalidArgument,
            "view name cannot be empty",
        ));
    }
    if name.len() > 100 {
        return Err(domain_status(
            ErrorCode::InvalidArgument,
            "view name cannot exceed 100 characters",
        ));
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(domain_status(
            ErrorCode::InvalidArgument,
            "view name cannot contain '..' or path separators",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Goals — `atomic vault goal list` (ListVaultEntries kind=GOAL)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Neighbors — `atomic vault query neighbors`
// ---------------------------------------------------------------------------

pub fn neighbors_impl(
    repo: &Repository,
    node_id: &str,
    depth: u8,
) -> Result<(Vec<KgNode>, Vec<KgEdge>), Status> {
    let subgraph = repo
        .vault_kg_neighbors(node_id, depth)
        .map_err(repository_error)?;
    let nodes = subgraph.nodes.into_iter().map(kg_node_proto).collect();
    let edges = subgraph.edges.into_iter().map(kg_edge_proto).collect();
    Ok((nodes, edges))
}

/// Domain KG node → wire, with the FULL field set (add-only source and
/// metadata) so the client serializes the same JSON shape the local body
/// emits.
/// System prompt for the agentic `query ask` loop — the CLI's ASK_SYSTEM_PROMPT,
/// ported verbatim (one implementation: the handler).
pub(crate) const ASK_SYSTEM_PROMPT: &str = "\
You are a code assistant for a repository tracked by Atomic VCS (not git).

You have tools to explore the repository:
- kg_search: search the knowledge graph (files, functions, changes, views, goals, intents)
- kg_neighbors: explore connections around a node
- read_file: read source files (with optional line ranges)
- code_search: search source code content for a pattern (regex, path/type filters)
- list_entities: list functions/classes/types in a file via tree-sitter
- vault_read: read vault entries (goals, intents, memories)

Strategy:
1. Search first: use kg_search or code_search to find relevant files and line numbers.
2. Use list_entities to see what functions/classes a file contains and their line ranges.
3. Use kg_neighbors to follow relationships (which changes modified a file, what it depends on).
4. NEVER call read_file without start_line and end_line. Always use list_entities or \
   code_search first to find the exact line range you need, then read just that range.
5. Answer concisely using what you found. Cite file paths and line numbers.

The knowledge graph contains:
- file nodes (file:path) — tracked files
- entity nodes (entity:file:name:line) — functions, structs, classes from tree-sitter
- change nodes (change:hash) — commit history with dates and messages
- view nodes (view:name) — like branches
- goal/intent/memory nodes — development sessions and work items
- edges: MODIFIES, DEFINES, AUTHORED_BY, DEPENDS_ON, REFERENCES, ON_VIEW, etc.";

pub(crate) fn kg_node_proto(node: atomic_core::pristine::vault::KgNode) -> KgNode {
    KgNode {
        id: node.id,
        node_type: node.kind,
        name: Some(node.label),
        data: node.summary.clone().map(String::into_bytes),
        score: None,
        source: Some(node.source),
        metadata: node
            .metadata
            .and_then(|metadata| serde_json::to_vec(&metadata).ok()),
    }
}

/// Domain KG edge → wire, with the edge metadata (add-only).
pub(crate) fn kg_edge_proto(edge: atomic_core::pristine::vault::KgEdge) -> KgEdge {
    KgEdge {
        source: edge.from_id,
        target: edge.to_id,
        relation: edge.kind,
        metadata: edge
            .metadata
            .and_then(|metadata| serde_json::to_vec(&metadata).ok()),
    }
}

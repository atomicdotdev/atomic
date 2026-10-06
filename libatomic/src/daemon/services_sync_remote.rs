//! The remote-sync half of SyncService: `atomic push` / `atomic pull`
//! (the network sync IS domain — the handler performs it end to end and
//! returns the structured report the CLI renders with its local print
//! functions) and `BindRemoteProject` (`atomic project init`'s server-side
//! project creation + the remote bind).
//!
//! The ports are faithful: every domain step of the local bodies (chain
//! construction, manifest planning, the set-union membership model,
//! content-addressed stores, CAS ref moves, sidecar ingestion, set-id
//! verification) runs here against the repository handle; the CLI ships
//! the auth headers (resolved client-side from the identity store — a
//! config read, never redb) and renders the report.
//!
//! Network-phase failures carry the local bodies' exact composed texts
//! over a structured wire error (see the sync_error encoder) so the client raises
//! the identical CLI error after printing the same spinner-stage line.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use atomic_core::types::{Base32, Hash, Merkle, SetId};
use atomic_objects::{
    ObjectFamily, ObjectRecord, RefRecord, SyncPack, SyncWants, ViewScopeLabel, ViewSnapshot,
};
use atomic_remote::{HttpRemote, HttpRemoteConfig, RemoteError};
use atomic_repository::{Repository, ViewManifest};
use bytes::Bytes;
use serde_json::json;

use crate::atomic::ErrorCode;
use crate::atomic::{
    BindRemoteProjectRequest, BindRemoteProjectResponse, ManifestState, PullChangesRequest,
    PullChangesResponse, PushChange, PushChangesRequest, PushChangesResponse, PushConflict,
    PushReport, PushSidecar, PushTag, PushViewReport,
};
use tonic::{Request, Response, Status};

use super::convert::hash_proto;
use super::state::{domain_status, repository_error, DaemonState};

/// Default remote name / timeout when the request leaves them unset (the
/// local bodies' constants).
const DEFAULT_REMOTE: &str = "origin";
const DEFAULT_TIMEOUT_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// the structured wire error (stage + variant + the local's message text)
// ---------------------------------------------------------------------------

/// The wire-encoded sync failure: which spinner stage failed, which CLI
/// error shape the local body raised, the remote URL, and the exact
/// message text the local body composed. Encoded as JSON in the status
/// message so the client hook raises the identical CLI error.
fn sync_error(stage: &str, variant: &str, url: &str, message: String) -> Status {
    domain_status(
        ErrorCode::Remote,
        json!({
            "stage": stage,
            "variant": variant,
            "url": url,
            "message": message,
        })
        .to_string(),
    )
}

/// The local `convert_remote_error` mapping (push flavor), ported: it
/// produces the (variant, message, url) the CLI raised.
fn convert_remote_error_push(error: RemoteError, url: &str) -> (&'static str, String) {
    match error {
        RemoteError::ConnectionFailed { .. } => {
            ("remote_error", format!("Failed to connect: {error}"))
        }
        RemoteError::AuthenticationFailed { .. } => ("authentication_failed", url.to_string()),
        RemoteError::RepositoryNotFound { .. } => {
            ("remote_error", not_found_push_message("the project"))
        }
        RemoteError::ViewNotFound { view } => (
            "remote_error",
            not_found_push_message(&format!("project/view '{view}'")),
        ),
        RemoteError::ChangeNotFound { hash } => ("change_not_found", hash),
        RemoteError::MissingDependencies {
            count,
            missing_hashes,
        } => (
            "missing_dependency",
            if missing_hashes.is_empty() {
                format!("{count} dependencies")
            } else {
                missing_hashes.join(", ")
            },
        ),
        RemoteError::StateMismatch {
            remote_state,
            requested_state,
        } => (
            "conflict",
            format!("State mismatch: remote is at {remote_state}, requested {requested_state}"),
        ),
        RemoteError::Timeout { seconds } => (
            "remote_error",
            format!("Request timed out after {seconds} seconds"),
        ),
        RemoteError::HttpError { status: 413, .. } => (
            "remote_error",
            "Change too large for server (413 Payload Too Large). \
             The server's body size limit is too small. \
             Ask the server admin to increase MAX_BODY_SIZE_MB."
                .to_string(),
        ),
        other => ("remote_error", other.to_string()),
    }
}

fn not_found_push_message(target: &str) -> String {
    format!(
        "Remote returned 'not found' for {target}. This means either it \
         doesn't exist, or you're not authenticated/authorized to push to it. \
         Make sure your identity is registered with the server \
         (`atomic identity register <server-url>`) and that you have push access."
    )
}

/// The pull body's `convert_remote_error` (pull/helpers.rs), ported.
fn convert_remote_error_pull(error: RemoteError, _url: &str) -> (&'static str, String) {
    match error {
        RemoteError::ConnectionFailed { .. } => {
            ("remote_error", format!("Failed to connect: {error}"))
        }
        RemoteError::AuthenticationFailed { .. } => ("authentication_failed", String::new()),
        other => ("remote_error", other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// shared domain ports (the local push/pull helpers' logic, verbatim)
// ---------------------------------------------------------------------------

/// The HTTP remote config from the request knobs + the client's auth
/// headers.
fn remote_config(
    timeout_secs: u64,
    insecure: bool,
    auth_headers: &HashMap<String, String>,
) -> HttpRemoteConfig {
    let mut config = HttpRemoteConfig::new()
        .with_timeout(Duration::from_secs(timeout_secs))
        .danger_accept_invalid_certs(insecure);
    for (name, value) in auth_headers {
        config = config.with_header(name.clone(), value.clone());
    }
    config
}

/// The local `build_view_chain` — the ancestor chain, root → leaf, with
/// the cycle guard.
fn build_view_chain(repo: &Repository, leaf: &str) -> Result<Vec<String>, Status> {
    let mut chain = vec![leaf.to_string()];
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(leaf.to_string());
    let mut current = leaf.to_string();
    loop {
        let parent = repo
            .get_view_info(&current)
            .map_err(|_| domain_status(ErrorCode::View, format!("view '{current}' not found")))?
            .parent_name;
        let Some(parent) = parent else { break };
        if !visited.insert(parent.clone()) {
            return Err(domain_status(
                ErrorCode::InvalidArgument,
                format!("view parent chain contains a cycle at '{parent}'"),
            ));
        }
        chain.push(parent.clone());
        current = parent;
    }
    chain.reverse();
    Ok(chain)
}

/// The remote ref advertisement — the local `parse_advertisement`.
#[derive(Clone)]
struct Advertisement {
    ref_targets: HashMap<String, String>,
    manifests: HashMap<String, ViewManifest>,
}

fn parse_advertisement(pack: &SyncPack, url: &str, stage: &str) -> Result<Advertisement, Status> {
    let mut snapshots: HashMap<String, ViewSnapshot> = HashMap::new();
    for object in &pack.objects {
        if object.family == ObjectFamily::View {
            if let Some(snapshot) = ViewSnapshot::from_bytes(&object.bytes) {
                snapshots.insert(object.key.clone(), snapshot);
            }
        }
    }
    let mut ref_targets = HashMap::new();
    let mut manifests = HashMap::new();
    for reference in &pack.refs {
        ref_targets.insert(reference.name.clone(), reference.new_target.clone());
        if let Some(snapshot) = snapshots.get(&reference.new_target) {
            let manifest = parse_remote_manifest(
                &reference.name,
                &snapshot.to_manifest_text(&reference.name),
                url,
                stage,
            )?;
            manifests.insert(reference.name.clone(), manifest);
        }
    }
    Ok(Advertisement {
        ref_targets,
        manifests,
    })
}

/// The local `parse_remote_manifest` — parse + verify, mapping failures to
/// the remote error text.
fn parse_remote_manifest(
    view: &str,
    text: &str,
    url: &str,
    stage: &str,
) -> Result<ViewManifest, Status> {
    let manifest = ViewManifest::parse(text).map_err(|error| {
        sync_error(
            stage,
            "remote_error",
            url,
            format!("Invalid manifest for view '{view}' from remote: {error}"),
        )
    })?;
    manifest.verify().map_err(|error| {
        sync_error(
            stage,
            "remote_error",
            url,
            format!("Corrupt manifest for view '{view}' from remote: {error}"),
        )
    })?;
    Ok(manifest)
}

/// The local `plan_view_sync` — the set-union membership model with the
/// identity checks and the declare decision.
struct ViewSyncPlan {
    suffix: Vec<Hash>,
    declare: bool,
    forced: bool,
    shrink: bool,
}

impl ViewSyncPlan {
    fn is_noop(&self) -> bool {
        self.suffix.is_empty() && !self.declare
    }
}

#[allow(dead_code)]
enum ViewSyncConflict {
    Diverged {
        first_mismatch: Option<usize>,
        local_len: usize,
        remote_len: usize,
    },
    IdentityMismatch {
        field: &'static str,
        local: String,
        remote: String,
    },
}

impl ViewSyncConflict {
    fn describe(&self) -> String {
        match self {
            ViewSyncConflict::Diverged {
                first_mismatch,
                local_len,
                remote_len,
            } => match first_mismatch {
                Some(index) => format!(
                    "remote log ({remote_len} changes) is not a prefix of local log \
                     ({local_len} changes): first mismatch at position {index}"
                ),
                None => format!(
                    "remote log ({remote_len} changes) is longer than local log \
                     ({local_len} changes)"
                ),
            },
            ViewSyncConflict::IdentityMismatch {
                field,
                local,
                remote,
            } => {
                format!("view {field} differs: local '{local}' vs remote '{remote}'")
            }
        }
    }
}

fn plan_view_sync(
    local: &ViewManifest,
    remote: Option<&ViewManifest>,
) -> Result<ViewSyncPlan, ViewSyncConflict> {
    let Some(remote) = remote else {
        return Ok(ViewSyncPlan {
            suffix: local.changes.clone(),
            declare: true,
            forced: false,
            shrink: false,
        });
    };
    if remote.scope != local.scope {
        return Err(ViewSyncConflict::IdentityMismatch {
            field: "scope",
            local: local.scope.to_string(),
            remote: remote.scope.to_string(),
        });
    }
    if remote.parent != local.parent {
        let show = |parent: &Option<String>| parent.clone().unwrap_or_else(|| "(none)".to_string());
        return Err(ViewSyncConflict::IdentityMismatch {
            field: "parent",
            local: show(&local.parent),
            remote: show(&remote.parent),
        });
    }
    let remote_set: HashSet<Hash> = remote.changes.iter().copied().collect();
    let local_set: HashSet<Hash> = local.changes.iter().copied().collect();
    let suffix = local
        .changes
        .iter()
        .filter(|hash| !remote_set.contains(hash))
        .copied()
        .collect();
    Ok(ViewSyncPlan {
        suffix,
        declare: !local_set.is_subset(&remote_set),
        forced: false,
        shrink: false,
    })
}

/// The local `mint_view_snapshot` — the content-addressed snapshot mint
/// (the own-set-id fold matches the server bridge).
fn mint_view_snapshot(manifest: &ViewManifest, prev: Option<String>) -> ViewSnapshot {
    let own_changes: Vec<String> = manifest.changes.iter().map(|h| h.to_base32()).collect();
    let mut acc = SetId::ZERO;
    for hash in &manifest.changes {
        acc = acc.add(hash);
    }
    let own_set_id = acc.to_base32();
    let merkle_state = if manifest.changes.is_empty() {
        None
    } else {
        Some(manifest.state.to_base32())
    };
    let scope = if manifest.scope.is_draft() {
        ViewScopeLabel::Draft
    } else {
        ViewScopeLabel::Shared
    };
    ViewSnapshot::new(
        scope,
        manifest.parent.clone(),
        prev.into_iter().collect(),
        own_changes,
        own_set_id,
        merkle_state,
    )
}

/// The local `load_change_data` — the raw on-disk change bytes (the exact
/// bytes that produced the content hash), with the load+serialize fallback.
fn load_change_data(repo: &Repository, hash: &Hash) -> Result<Bytes, Status> {
    let change_path = repo.change_store().change_path(hash);
    if change_path.exists() {
        let data = std::fs::read(&change_path).map_err(|error| {
            domain_status(
                ErrorCode::Repository,
                format!("Failed to read change file {:?}: {error}", change_path),
            )
        })?;
        return Ok(Bytes::from(data));
    }
    let change = repo.load_change(hash).map_err(|_| {
        domain_status(
            ErrorCode::Changes,
            format!("change not found: {}", hash.to_base32()),
        )
    })?;
    let mut buffer = Vec::new();
    change.serialize(&mut buffer).map_err(|error| {
        domain_status(ErrorCode::Repository, format!("serialize change: {error}"))
    })?;
    Ok(Bytes::from(buffer))
}

fn load_change_message(repo: &Repository, hash: &Hash) -> Option<String> {
    repo.load_change(hash)
        .ok()
        .map(|change| change.hashed.header.message.clone())
}

fn manifest_state(view: &str, manifest: &ViewManifest) -> ManifestState {
    ManifestState {
        view: view.to_string(),
        state: manifest.state.to_base32(),
        change_count: manifest.changes.len() as u64,
    }
}

/// One planned view: the wire report plus the (possibly leaf-renamed)
/// local manifest the sync phase declares.
struct PlannedView {
    report: PushViewReport,
    manifest: ViewManifest,
}

// ---------------------------------------------------------------------------
// PushChanges
// ---------------------------------------------------------------------------

pub async fn push_changes_impl(
    state: Arc<DaemonState>,
    request: Request<PushChangesRequest>,
) -> Result<Response<PushChangesResponse>, Status> {
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("PushChanges", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let meta = request.meta.clone();

    // Resolve the remote name + URL (the client may have resolved the URL
    // through ListRemotes; otherwise the same domain lookup runs here).
    let requested_remote = request.remote.clone();
    let client_url = request.remote_url.clone();
    let (remote_name, remote_url) = {
        let handle = handle.clone();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            let name = if requested_remote.is_empty() {
                repo.get_default_remote()
                    .map(|(name, _)| name)
                    .unwrap_or_else(|_| DEFAULT_REMOTE.to_string())
            } else {
                requested_remote
            };
            let url = if let Some(url) = client_url {
                url
            } else if name.contains("://") {
                name.clone()
            } else {
                repo.get_remote(&name)
                    .map(|entry| entry.url)
                    .map_err(|error| match error {
                        atomic_repository::RepositoryError::RemoteNotFound { .. } => {
                            domain_status(ErrorCode::NotFound, format!("remote '{name}' not found"))
                        }
                        other => repository_error(other),
                    })?
            };
            Ok::<_, Status>((name, url))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??
    };

    let to_view = request.to_view.clone();
    let from_view = request.from_view.clone();
    let dry_run = request.dry_run;
    let all = request.all;
    let insecure = request.insecure;
    let timeout_secs = if request.timeout_secs == 0 {
        DEFAULT_TIMEOUT_SECS
    } else {
        request.timeout_secs
    };
    let auth_headers: HashMap<String, String> = request.auth_headers.clone();

    // The local view (the current-view default reads the working-copy
    // marker — the same default every local command applies).
    let local_view = match &from_view {
        Some(view) => view.clone(),
        None => handle.current_view(),
    };
    let remote_view = to_view.clone().unwrap_or_else(|| local_view.clone());

    // The ancestor chain, root → leaf.
    let chain = {
        let handle = handle.clone();
        let leaf = local_view.clone();
        tokio::task::spawn_blocking(move || build_view_chain(&handle.repository_readonly()?, &leaf))
            .await
            .map_err(|error| Status::internal(error.to_string()))??
    };

    // Connect (the auth headers ride the client-built config).
    let config = remote_config(timeout_secs, insecure, &auth_headers);
    let remote = HttpRemote::with_config(&remote_url, config).map_err(|error| {
        let (variant, message) = convert_remote_error_push(error, &remote_url);
        sync_error("connect", variant, &remote_url, message)
    })?;

    // Fetch the remote advertisement.
    let remote_names: Vec<String> = chain
        .iter()
        .map(|name| {
            if name == &local_view {
                remote_view.clone()
            } else {
                name.clone()
            }
        })
        .collect();
    let adv_pack = remote
        .sync_pull(&SyncWants::advertise(remote_names.clone()))
        .await
        .map_err(|error| {
            let (variant, message) = convert_remote_error_push(error, &remote_url);
            sync_error("advertise", variant, &remote_url, message)
        })?;
    let advertisement = parse_advertisement(&adv_pack, &remote_url, "advertise")?;

    // Plan phase: per-view sync plans (no mutation happens here — a
    // conflict rides the response so the client renders the local block).
    let (planned, plan_conflict): (Vec<PlannedView>, Option<PushConflict>) = {
        let handle = handle.clone();
        let advertisement = advertisement.clone();
        let chain = chain.clone();
        let local_view = local_view.clone();
        let remote_view = remote_view.clone();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            let mut known_on_remote: HashSet<Hash> = HashSet::new();
            let mut planned = Vec::with_capacity(chain.len());
            let mut conflict: Option<PushConflict> = None;
            for name in &chain {
                let is_leaf = name == &local_view;
                let remote_view_name = if is_leaf {
                    remote_view.clone()
                } else {
                    name.clone()
                };
                let mut manifest = repo.view_manifest(name).map_err(repository_error)?;
                if manifest.name != remote_view_name {
                    manifest.name = remote_view_name.clone();
                }
                let remote_manifest = advertisement.manifests.get(&remote_view_name).cloned();
                if let Some(remote_manifest) = &remote_manifest {
                    known_on_remote.extend(remote_manifest.changes.iter().copied());
                }
                let (noop, shrink, forced) =
                    match plan_view_sync(&manifest, remote_manifest.as_ref()) {
                        Ok(plan) => (plan.is_noop(), plan.shrink, plan.forced),
                        Err(view_conflict) => {
                            conflict = Some(PushConflict {
                                view: name.clone(),
                                identity_mismatch: matches!(
                                    view_conflict,
                                    ViewSyncConflict::IdentityMismatch { .. }
                                ),
                                description: view_conflict.describe(),
                                local: Some(manifest_state(name, &manifest)),
                                remote: remote_manifest
                                    .as_ref()
                                    .map(|manifest| manifest_state(&manifest.name, manifest)),
                            });
                            break;
                        }
                    };
                // With --all, re-store the full log (content-addressed,
                // idempotent); otherwise the suffix minus known-present.
                let candidates: &[Hash] = if all {
                    &manifest.changes
                } else {
                    &manifest
                        .changes
                        .iter()
                        .filter(|hash| !known_on_remote.contains(hash))
                        .copied()
                        .collect::<Vec<_>>()
                };
                let to_store_details = candidates
                    .iter()
                    .map(|hash| PushChange {
                        hash: Some(hash_proto(hash)),
                        message: load_change_message(&repo, hash),
                    })
                    .collect::<Vec<_>>();
                planned.push(PlannedView {
                    report: PushViewReport {
                        remote_name: remote_view_name,
                        scope: manifest.scope.to_string(),
                        parent: manifest.parent.clone(),
                        exists_on_remote: remote_manifest.is_some(),
                        remote_change_count: remote_manifest
                            .as_ref()
                            .map(|manifest| manifest.changes.len() as u64)
                            .unwrap_or(0),
                        noop,
                        shrink,
                        forced,
                        to_store: to_store_details,
                        log_count: manifest.changes.len() as u64,
                        declared: false,
                    },
                    manifest,
                });
            }
            Ok::<_, Status>((planned, conflict))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??
    };
    if let Some(conflict) = plan_conflict {
        return Ok(Response::new(PushChangesResponse {
            pushed_changes: 0,
            manifests: Vec::new(),
            report: None,
            conflict: Some(conflict),
            meta: super::services_agent::response_meta(&meta),
        }));
    }

    // Dry run: the plan is the whole outcome.
    if dry_run {
        let report = PushReport {
            remote_name,
            remote_url,
            views: planned.into_iter().map(|view| view.report).collect(),
            total_stored: 0,
            views_declared: 0,
            attestations: Vec::new(),
            provenance: Vec::new(),
            tags: Vec::new(),
        };
        return Ok(Response::new(PushChangesResponse {
            pushed_changes: 0,
            manifests: Vec::new(),
            report: Some(report),
            conflict: None,
            meta: super::services_agent::response_meta(&meta),
        }));
    }

    // Sync phase: store the change files, mint the snapshots + ref CAS
    // moves, queue attestations/provenance/tags.
    let all_noop = planned.iter().all(|view| view.report.noop);
    let sync_phase = {
        let handle = handle.clone();
        let local_view = local_view.clone();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository_readonly()?;
            let mut total_stored = 0usize;
            let mut views_declared = 0u64;
            let mut stored_hashes: Vec<Hash> = Vec::new();
            let mut declared_manifests: Vec<ViewManifest> = Vec::new();
            let mut pack = SyncPack::empty();
            // The published closure: the union of every view's full log
            // (the local body's all_synced).
            let mut all_synced: HashSet<Hash> = HashSet::new();
            for view in &planned {
                all_synced.extend(view.manifest.changes.iter().copied());
            }
            let mut stored_reports = Vec::new();
            for view in &planned {
                if view.report.noop {
                    stored_reports.push(view.report.clone());
                    continue;
                }
                let to_store: Vec<Hash> = view
                    .report
                    .to_store
                    .iter()
                    .filter_map(|change| {
                        change
                            .hash
                            .as_ref()
                            .and_then(|hash| hash.value.clone().try_into().ok())
                            .map(Merkle)
                    })
                    .collect();
                if !to_store.is_empty() {
                    for hash in &to_store {
                        let data = load_change_data(&repo, hash)?;
                        pack.objects.push(ObjectRecord::new(
                            ObjectFamily::Change,
                            hash.to_base32(),
                            data.to_vec(),
                        ));
                    }
                    total_stored += to_store.len();
                    stored_hashes.extend(to_store.iter().copied());
                }
                // The mint + ref CAS move.
                let prev = advertisement
                    .ref_targets
                    .get(&view.report.remote_name)
                    .cloned();
                let snapshot = mint_view_snapshot(&view.manifest, prev.clone());
                let snap_key = snapshot.content_key();
                pack.objects.push(ObjectRecord::new(
                    ObjectFamily::View,
                    snap_key.clone(),
                    snapshot.to_canonical_bytes(),
                ));
                pack.refs.push(RefRecord {
                    name: view.report.remote_name.clone(),
                    expect_old: prev,
                    new_target: snap_key,
                });
                views_declared += 1;
                declared_manifests.push(view.manifest.clone());
                let mut report = view.report.clone();
                report.declared = true;
                stored_reports.push(report);
            }
            // Upload attestations that cover only published changes.
            let mut attest_reports = Vec::new();
            {
                let mut seen: HashSet<Hash> = HashSet::new();
                for pushed_hash in &stored_hashes {
                    for (attest_hash, attestation) in repo
                        .find_attestations_for_change(pushed_hash)
                        .unwrap_or_default()
                    {
                        if !seen.insert(attest_hash) {
                            continue;
                        }
                        let all_covered = attestation
                            .changes_covered
                            .iter()
                            .all(|hash| all_synced.contains(hash));
                        if !all_covered {
                            continue;
                        }
                        let Ok(data) = attestation.serialize() else {
                            continue;
                        };
                        pack.objects.push(ObjectRecord::new(
                            ObjectFamily::Attest,
                            attest_hash.to_base32(),
                            data,
                        ));
                        attest_reports.push(PushSidecar {
                            hash: Some(hash_proto(&attest_hash)),
                            cost: Some(attestation.cost_display()),
                            covered: attestation.change_count() as u64,
                            nodes: 0,
                            explained: 0,
                        });
                    }
                }
            }
            // Upload provenance graphs whose explained changes all travel.
            let mut provenance_reports = Vec::new();
            {
                let mut seen: HashSet<Hash> = HashSet::new();
                for pushed_hash in &stored_hashes {
                    for (prov_hash, graph) in repo
                        .find_provenance_for_change(pushed_hash)
                        .unwrap_or_default()
                    {
                        if !seen.insert(prov_hash) {
                            continue;
                        }
                        let all_explained = graph
                            .changes_explained
                            .iter()
                            .all(|hash| all_synced.contains(hash));
                        if !all_explained {
                            continue;
                        }
                        let Ok(data) = graph.serialize() else {
                            continue;
                        };
                        pack.objects.push(ObjectRecord::new(
                            ObjectFamily::Provenance,
                            prov_hash.to_base32(),
                            data,
                        ));
                        provenance_reports.push(PushSidecar {
                            hash: Some(hash_proto(&prov_hash)),
                            cost: None,
                            covered: 0,
                            nodes: graph.node_count() as u64,
                            explained: graph.change_count() as u64,
                        });
                    }
                }
            }
            // Upload the pushed leaf view's tags (declared under the
            // possibly-renamed remote view name).
            let mut tag_reports = Vec::new();
            for tag in repo.list_tags_for_view(&local_view).unwrap_or_default() {
                let tag_hash = tag.content_hash();
                let Ok(bytes) = atomic_repository::serialize_tag(&tag) else {
                    continue;
                };
                pack.objects.push(ObjectRecord::new(
                    ObjectFamily::Tag,
                    tag_hash.to_base32(),
                    bytes,
                ));
                tag_reports.push(PushTag {
                    hash: Some(hash_proto(&tag_hash)),
                    name: tag.name.clone(),
                    kind: tag.kind.to_string(),
                });
            }
            Ok::<_, Status>((
                pack,
                stored_reports,
                declared_manifests,
                total_stored as u64,
                views_declared,
                attest_reports,
                provenance_reports,
                tag_reports,
            ))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??
    };
    let (
        pack,
        view_reports,
        declared_manifests,
        total_stored,
        views_declared,
        attestations,
        provenance,
        tags,
    ) = sync_phase;

    // Send everything in one /code push, then verify the union: every
    // declared manifest's patch must be present remotely.
    if !pack.is_empty() {
        remote.sync_push(&pack).await.map_err(|error| {
            let (variant, message) = convert_remote_error_push(error, &remote_url);
            sync_error("send", variant, &remote_url, message)
        })?;
        let final_pack = remote
            .sync_pull(&SyncWants::advertise(remote_names.clone()))
            .await
            .map_err(|error| {
                let (variant, message) = convert_remote_error_push(error, &remote_url);
                sync_error("verify", variant, &remote_url, message)
            })?;
        let final_adv = parse_advertisement(&final_pack, &remote_url, "verify")?;
        for manifest in &declared_manifests {
            let Some(final_manifest) = final_adv.manifests.get(&manifest.name) else {
                return Err(sync_error(
                    "verify-union",
                    "remote_error",
                    &remote_url,
                    format!("Remote did not advertise pushed view '{}'", manifest.name),
                ));
            };
            let final_set: HashSet<Hash> = final_manifest.changes.iter().copied().collect();
            if let Some(missing) = manifest
                .changes
                .iter()
                .find(|hash| !final_set.contains(hash))
            {
                return Err(sync_error(
                    "verify-patch",
                    "conflict",
                    &remote_url,
                    format!(
                        "Remote view '{}' does not contain proposed patch {}",
                        manifest.name,
                        missing.to_base32()
                    ),
                ));
            }
        }
    }
    let _ = all_noop;

    let report = PushReport {
        remote_name,
        remote_url,
        views: view_reports,
        total_stored,
        views_declared,
        attestations,
        provenance,
        tags,
    };
    Ok(Response::new(PushChangesResponse {
        pushed_changes: total_stored,
        manifests: Vec::new(),
        report: Some(report),
        conflict: None,
        meta: super::services_agent::response_meta(&meta),
    }))
}

// ---------------------------------------------------------------------------
// PullChanges
// ---------------------------------------------------------------------------

/// The local `manifest_chain_from_pack` — the remote metadata chain in
/// root-to-leaf order, with the set-id cross-check warning.
fn manifest_chain_from_pack(
    pack: &SyncPack,
    leaf: &str,
    url: &str,
    warnings: &mut Vec<String>,
) -> Result<Vec<ViewManifest>, Status> {
    let mut by_name: HashMap<String, ViewManifest> = HashMap::new();
    for reference in &pack.refs {
        if let Some(manifest) = manifest_from_pack(pack, &reference.name, url, warnings)? {
            by_name.insert(reference.name.clone(), manifest);
        }
    }
    let mut chain = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor = Some(leaf.to_string());
    while let Some(name) = cursor {
        if !seen.insert(name.clone()) {
            return Err(sync_error(
                "fetch",
                "remote_error",
                url,
                format!("Remote view parent chain contains a cycle at '{name}'"),
            ));
        }
        let manifest = by_name.remove(&name).ok_or_else(|| {
            sync_error(
                "fetch",
                "remote_error",
                url,
                format!("Remote view metadata is missing '{name}'"),
            )
        })?;
        cursor = manifest.parent.clone();
        chain.push(manifest);
    }
    chain.reverse();
    Ok(chain)
}

/// The local `manifest_from_pack` — one view's manifest reconstructed from
/// the pack's ref + snapshot, with the O(1) set-id cross-check.
fn manifest_from_pack(
    pack: &SyncPack,
    name: &str,
    url: &str,
    warnings: &mut Vec<String>,
) -> Result<Option<ViewManifest>, Status> {
    let Some(target) = pack.refs.iter().find(|r| r.name == name) else {
        return Ok(None);
    };
    let target = target.new_target.clone();
    let snapshot = pack
        .objects
        .iter()
        .find(|object| object.family == ObjectFamily::View && object.key == target)
        .and_then(|object| ViewSnapshot::from_bytes(&object.bytes));
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let manifest = ViewManifest::parse(&snapshot.to_manifest_text(name)).map_err(|error| {
        sync_error(
            "fetch",
            "remote_error",
            url,
            format!("Corrupt manifest for view '{name}': {error}"),
        )
    })?;
    let mut fold = SetId::ZERO;
    for hash in &manifest.changes {
        fold = fold.add(hash);
    }
    if fold.to_base32() != snapshot.own_set_id {
        warnings.push(format!(
            "Remote view '{name}' snapshot set-id disagrees with its change list; \
             the remote object may be inconsistent."
        ));
    }
    Ok(Some(manifest))
}

/// The local `save_downloaded_change` — deserialize, verify the hash, save.
fn save_downloaded_change(repo: &Repository, hash: &Hash, data: Bytes) -> Result<(), String> {
    let mut cursor = std::io::Cursor::new(&data[..]);
    let (change, computed) = atomic_core::change::Change::deserialize(&mut cursor)
        .map_err(|error| format!("Failed to deserialize change: {error}"))?;
    if computed != *hash {
        return Err(format!(
            "Hash mismatch: expected {}, got {}",
            hash.to_base32(),
            computed.to_base32()
        ));
    }
    repo.save_change(&change)
        .map(|_hash| ())
        .map_err(|error| format!("Failed to save change: {error}"))
}

/// The local `import_sidecars` (sidecars.rs) — ingest the pack's
/// provenance graphs and attestations; warnings collect for the client.
#[derive(Default)]
struct SidecarStats {
    provenance: usize,
    attestations: usize,
}

fn import_sidecars(repo: &Repository, pack: &SyncPack, warnings: &mut Vec<String>) -> SidecarStats {
    let mut stats = SidecarStats::default();
    fn short(key: &str) -> &str {
        &key[..12.min(key.len())]
    }
    for object in &pack.objects {
        match object.family {
            ObjectFamily::Provenance => {
                let Some(hash) = Hash::from_base32(object.key.as_bytes()) else {
                    continue;
                };
                let already_present = repo.has_provenance_graph(&hash);
                match atomic_core::change::ProvenanceGraph::deserialize(&object.bytes) {
                    Ok((graph, computed)) => {
                        if computed != hash {
                            warnings.push(format!(
                                "Provenance {} failed hash verification — skipped",
                                short(&object.key)
                            ));
                            continue;
                        }
                        match repo.save_provenance_graph(&graph).map(|_hash| ()) {
                            Ok(()) if !already_present => stats.provenance += 1,
                            Ok(()) => {}
                            Err(error) => warnings.push(format!(
                                "Failed to register provenance {}: {error}",
                                short(&object.key)
                            )),
                        }
                    }
                    Err(error) => warnings.push(format!(
                        "Corrupt provenance {}: {error}",
                        short(&object.key)
                    )),
                }
            }
            ObjectFamily::Attest => {
                let Some(hash) = Hash::from_base32(object.key.as_bytes()) else {
                    continue;
                };
                let already_present = repo.has_attestation(&hash);
                match atomic_core::change::Attestation::deserialize(&object.bytes) {
                    Ok((attestation, computed)) => {
                        if computed != hash {
                            warnings.push(format!(
                                "Attestation {} failed hash verification — skipped",
                                short(&object.key)
                            ));
                            continue;
                        }
                        match repo.save_attestation(&attestation).map(|_hash| ()) {
                            Ok(()) if !already_present => stats.attestations += 1,
                            Ok(()) => {}
                            Err(error) => warnings.push(format!(
                                "Failed to register attestation {}: {error}",
                                short(&object.key)
                            )),
                        }
                    }
                    Err(error) => warnings.push(format!(
                        "Corrupt attestation {}: {error}",
                        short(&object.key)
                    )),
                }
            }
            _ => {}
        }
    }
    stats
}

pub async fn pull_changes_impl(
    state: Arc<DaemonState>,
    request: Request<PullChangesRequest>,
) -> Result<Response<PullChangesResponse>, Status> {
    use crate::atomic::{PullDownload, PullReport, PullSidecar, PullTag, PullWarning};
    let request = request.into_inner();
    let handle = state.resolve(request.repository.as_ref().unwrap())?;
    state.log_rpc("PullChanges", Some(&handle));
    let gate_handle = handle.clone();
    let _gate = gate_handle.exclusive().await;
    let meta = request.meta.clone();

    // Resolve the remote name + URL (same domain lookup as push; the
    // client may have pre-resolved the URL through ListRemotes).
    let requested_remote = request.remote.clone();
    let client_url = request.remote_url.clone();
    let (remote_name, remote_url) = {
        let handle = handle.clone();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            let name = if requested_remote.is_empty() {
                repo.get_default_remote()
                    .map(|(name, _)| name)
                    .unwrap_or_else(|_| DEFAULT_REMOTE.to_string())
            } else {
                requested_remote
            };
            let url = if let Some(url) = client_url {
                url
            } else if name.contains("://") {
                name.clone()
            } else {
                repo.get_remote(&name)
                    .map(|entry| entry.url)
                    .map_err(|error| match error {
                        atomic_repository::RepositoryError::RemoteNotFound { .. } => {
                            domain_status(ErrorCode::NotFound, format!("remote '{name}' not found"))
                        }
                        other => repository_error(other),
                    })?
            };
            Ok::<_, Status>((name, url))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??
    };

    let to_view = request.to_view.clone();
    let from_view = request.from_view.clone();
    let dry_run = request.dry_run;
    let insecure = request.insecure;
    let timeout_secs = if request.timeout_secs == 0 {
        DEFAULT_TIMEOUT_SECS
    } else {
        request.timeout_secs
    };
    let download_only = request.download_only;
    let auth_headers: HashMap<String, String> = request.auth_headers.clone();

    // Views: the remote view defaults to the current view; the local view
    // defaults to the remote view being pulled.
    let current_view = handle.current_view();
    let remote_view = from_view.clone().unwrap_or_else(|| current_view.clone());
    let local_view = to_view.clone().unwrap_or_else(|| remote_view.clone());

    // The local view's state (loaded for display only) + the graph-wide
    // haves.
    let (local_view_exists, local_entries_len, local_tip, haves) = {
        let handle = handle.clone();
        let local_view = local_view.clone();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            let exists = repo.view_exists(&local_view).map_err(repository_error)?;
            let entries = if exists {
                repo.log(atomic_repository::HistoryOptions::new().view(&local_view))
                    .map_err(repository_error)?
            } else {
                Vec::new()
            };
            let tip = entries.last().map(|entry| entry.hash.to_base32());
            let haves = repo
                .registered_change_hashes()
                .map_err(repository_error)?
                .into_iter()
                .map(|hash| hash.to_base32())
                .collect::<Vec<_>>();
            Ok::<_, Status>((exists, entries.len(), tip, haves))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??
    };

    // Connect.
    let config = remote_config(timeout_secs, insecure, &auth_headers);
    let remote = HttpRemote::with_config(&remote_url, config).map_err(|error| {
        let (variant, message) = convert_remote_error_pull(error, &remote_url);
        sync_error("connect", variant, &remote_url, message)
    })?;

    // One /code pull: the remote view's metadata chain + missing objects.
    let mut warnings: Vec<String> = Vec::new();
    let pull_pack = remote
        .sync_pull(&SyncWants {
            refs: vec![remote_view.clone()],
            haves,
            refs_only: dry_run,
        })
        .await
        .map_err(|error| {
            let (variant, message) = convert_remote_error_pull(error, &remote_url);
            sync_error("fetch", variant, &remote_url, message)
        })?;
    let mut remote_chain =
        match manifest_chain_from_pack(&pull_pack, &remote_view, &remote_url, &mut warnings) {
            Ok(chain) if !chain.is_empty() => chain,
            _ => {
                return Err(sync_error(
                    "fetch-missing",
                    "remote_error",
                    &remote_url,
                    format!("Remote view '{remote_view}' does not exist"),
                ));
            }
        };
    // --to-view renames only the requested leaf locally.
    if let Some(leaf) = remote_chain.last_mut() {
        leaf.name = local_view.clone();
    }
    let remote_manifest = remote_chain.last().expect("non-empty remote chain").clone();
    let remote_entries = remote_manifest.changes.len() as u64;
    let remote_state = if remote_manifest.changes.is_empty() {
        String::new()
    } else {
        remote_manifest.state.to_base32()
    };
    // The local body's delta: every change object in the pack is missing.
    let change_objects: HashMap<String, Vec<u8>> = pull_pack
        .objects
        .iter()
        .filter(|object| object.family == ObjectFamily::Change)
        .map(|object| (object.key.clone(), object.bytes.clone()))
        .collect();
    let to_download: Vec<Hash> = change_objects
        .keys()
        .filter_map(|key| Hash::from_base32(key.as_bytes()))
        .collect();

    // Local-only changes (the diverged-history warning).
    let local_only: Vec<String> = {
        let handle = handle.clone();
        let local_view = local_view.clone();
        let remote_manifest = remote_manifest.clone();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            if !local_view_exists {
                return Ok::<Vec<String>, Status>(Vec::new());
            }
            let local = repo
                .log(atomic_repository::HistoryOptions::new().view(&local_view))
                .map_err(repository_error)?;
            let remote_set: HashSet<String> = remote_manifest
                .changes
                .iter()
                .map(|hash| hash.to_base32())
                .collect();
            Ok(local
                .iter()
                .filter(|entry| !remote_set.contains(&entry.hash.to_base32()))
                .map(|entry| entry.hash.to_base32())
                .collect())
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??
    };

    // Dry run: the delta is the whole outcome.
    if dry_run {
        let downloads = to_download
            .iter()
            .map(|hash| PullDownload {
                hash: Some(hash_proto(hash)),
                message: None,
                ok: true,
                error: None,
                index: 0,
                total: to_download.len() as u64,
            })
            .collect();
        let report = PullReport {
            remote_name,
            remote_url,
            remote_view,
            local_view,
            local_view_changes: local_entries_len as u64,
            graph_objects: 0,
            remote_changes: remote_entries,
            remote_state: Some(remote_state),
            local_only,
            downloads,
            changes_downloaded: 0,
            bytes_transferred: 0,
            applied: false,
            applied_changes: 0,
            apply_errors: Vec::new(),
            sidecars: Vec::new(),
            set_id_verified: false,
            set_id_warning: None,
            materialized: false,
            files_written: 0,
            materialize_error: None,
            tags: Vec::new(),
            warnings: warnings
                .iter()
                .map(|text| PullWarning { text: text.clone() })
                .collect(),
            local_tip,
            vault_bootstrapped: false,
            reconciled_views: 0,
            download_only: false,
            changes_failed: 0,
        };
        return Ok(Response::new(PullChangesResponse {
            applied_changes: 0,
            updated_views: Vec::new(),
            report: Some(report),
            meta: super::services_agent::response_meta(&meta),
        }));
    }

    // Save the missing graph nodes.
    let (downloads, changes_downloaded, bytes_transferred, changes_failed) = {
        let handle = handle.clone();
        let change_objects = change_objects.clone();
        let to_download = to_download.clone();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            let mut changes_downloaded = 0u64;
            let mut bytes_transferred = 0u64;
            let mut changes_failed = 0u64;
            let mut downloads = Vec::with_capacity(to_download.len());
            for (index, hash) in to_download.iter().enumerate() {
                let hash_str = hash.to_base32();
                let message = repo
                    .load_change(hash)
                    .ok()
                    .map(|change| change.hashed.header.message.clone());
                // The local body prints the message from the downloaded
                // bytes; pull it from the deserialized change instead of a
                // second store read (identical text).
                let entry = match change_objects.get(&hash_str) {
                    Some(data) => {
                        let data_len = data.len() as u64;
                        match save_downloaded_change(&repo, hash, Bytes::from(data.clone())) {
                            Ok(()) => {
                                changes_downloaded += 1;
                                bytes_transferred += data_len;
                                let message = message.or_else(|| {
                                    let mut cursor = std::io::Cursor::new(&data[..]);
                                    atomic_core::change::Change::deserialize(&mut cursor)
                                        .ok()
                                        .map(|(change, _)| change.hashed.header.message)
                                });
                                PullDownload {
                                    hash: Some(hash_proto(hash)),
                                    message,
                                    ok: true,
                                    error: None,
                                    index: index as u64,
                                    total: to_download.len() as u64,
                                }
                            }
                            Err(error) => {
                                changes_failed += 1;
                                PullDownload {
                                    hash: Some(hash_proto(hash)),
                                    message,
                                    ok: false,
                                    error: Some(format!("save failed: {error}")),
                                    index: index as u64,
                                    total: to_download.len() as u64,
                                }
                            }
                        }
                    }
                    None => {
                        // The local body aborts the pull here.
                        return Err(sync_error(
                            "download",
                            "change_not_found",
                            &hash_str.clone(),
                            hash_str,
                        ));
                    }
                };
                downloads.push(entry);
            }
            Ok::<_, Status>((
                downloads,
                changes_downloaded,
                bytes_transferred,
                changes_failed,
            ))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??
    };

    // Download-only: sidecars still land, nothing is applied.
    if download_only {
        let (sidecar_reports, warnings) = {
            let handle = handle.clone();
            let pull_pack = pull_pack.clone();
            let warnings = warnings.clone();
            tokio::task::spawn_blocking(move || {
                let repo = handle.repository()?;
                let mut warnings = warnings;
                let mut sidecar_reports = Vec::new();
                let stats = import_sidecars(&repo, &pull_pack, &mut warnings);
                if stats.provenance > 0 {
                    sidecar_reports.push(PullSidecar {
                        kind: "provenance".to_string(),
                        count: stats.provenance as u64,
                        detail: None,
                    });
                }
                if stats.attestations > 0 {
                    sidecar_reports.push(PullSidecar {
                        kind: "attestation".to_string(),
                        count: stats.attestations as u64,
                        detail: None,
                    });
                }
                Ok::<_, Status>((sidecar_reports, warnings))
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))??
        };
        let report = PullReport {
            remote_name,
            remote_url,
            remote_view,
            local_view,
            local_view_changes: local_entries_len as u64,
            graph_objects: 0,
            remote_changes: remote_entries,
            remote_state: Some(remote_state),
            local_only,
            downloads,
            changes_downloaded,
            bytes_transferred,
            applied: false,
            applied_changes: 0,
            apply_errors: Vec::new(),
            sidecars: sidecar_reports,
            set_id_verified: false,
            set_id_warning: None,
            materialized: false,
            files_written: 0,
            materialize_error: None,
            tags: Vec::new(),
            warnings: warnings
                .iter()
                .map(|text| PullWarning { text: text.clone() })
                .collect(),
            local_tip,
            vault_bootstrapped: false,
            reconciled_views: 0,
            download_only: true,
            changes_failed,
        };
        return Ok(Response::new(PullChangesResponse {
            applied_changes: 0,
            updated_views: Vec::new(),
            report: Some(report),
            meta: super::services_agent::response_meta(&meta),
        }));
    }

    // Reconcile the view metadata root-to-leaf over the populated graph.
    let (reconciled_views, applied, applied_changes, apply_errors) = {
        let handle = handle.clone();
        let remote_chain = remote_chain.clone();
        tokio::task::spawn_blocking(move || {
            let mut repo = handle.repository()?;
            let mut applied_changes = 0u64;
            let mut applied = false;
            let mut apply_errors = Vec::new();
            for manifest in &remote_chain {
                match repo.reconcile_view_manifest(manifest) {
                    Ok(outcome) => {
                        applied_changes += outcome.replayed as u64;
                        if outcome.replayed > 0 {
                            applied = true;
                        }
                    }
                    Err(error) => apply_errors.push(format!(
                        "Failed to reconcile view '{}': {error}",
                        manifest.name
                    )),
                }
            }
            Ok::<_, Status>((
                remote_chain.len() as u64,
                applied,
                applied_changes,
                apply_errors,
            ))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??
    };

    // Sidecars ingest AFTER reconciliation (the DEPS edges need the
    // covered changes registered).
    let (sidecar_reports, warnings) = {
        let handle = handle.clone();
        let pull_pack = pull_pack.clone();
        let warnings = warnings.clone();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            let mut warnings = warnings;
            let mut sidecar_reports = Vec::new();
            let stats = import_sidecars(&repo, &pull_pack, &mut warnings);
            if stats.provenance > 0 {
                sidecar_reports.push(PullSidecar {
                    kind: "provenance".to_string(),
                    count: stats.provenance as u64,
                    detail: None,
                });
            }
            if stats.attestations > 0 {
                sidecar_reports.push(PullSidecar {
                    kind: "attestation".to_string(),
                    count: stats.attestations as u64,
                    detail: None,
                });
            }
            Ok::<_, Status>((sidecar_reports, warnings))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??
    };

    // Set-id verification (the server's effective set-id vs the local).
    let mut set_id_verified = false;
    let mut set_id_warning: Option<String> = None;
    if applied {
        let remote_view = remote_view.clone();
        let expected = remote.list_view_refs().await.ok().and_then(|inventory| {
            inventory
                .into_iter()
                .find(|view| view.name == remote_view)
                .and_then(|view| view.set_id)
        });
        let handle = handle.clone();
        let local_view = local_view.clone();
        let (verified, warning) = {
            tokio::task::spawn_blocking(move || {
                let repo = handle.repository()?;
                let mut set_id_verified = false;
                let mut set_id_warning: Option<String> = None;
                match (repo.view_set_id(&local_view), expected) {
                    (Ok(local), Some(expected)) => {
                        if local.to_base32() == expected {
                            set_id_verified = true;
                        } else {
                            set_id_warning = Some(format!(
                                "Set-id mismatch after pull: local {} != remote {}. \
                                 The view may be divergent or incomplete.",
                                local.to_base32(),
                                expected
                            ));
                        }
                    }
                    (Ok(_), None) => { /* remote reported no set-id; skip */ }
                    (Err(error), _) => {
                        set_id_warning = Some(format!("Could not compute local set-id: {error}"));
                    }
                }
                Ok::<_, Status>((set_id_verified, set_id_warning))
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))??
        };
        set_id_verified = verified;
        set_id_warning = warning;
    }

    // Materialize the working copy (only when pulling the current view).
    let (materialized, files_written, materialize_error, vault_bootstrapped, warnings) = if applied
    {
        let handle = handle.clone();
        let local_view = local_view.clone();
        let current_view = current_view.clone();
        let warnings_in = warnings.clone();
        let materialize_outcome = tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            let mut warnings = warnings_in;
            let mut materialized = false;
            let mut files_written = 0u64;
            let mut materialize_error: Option<String> = None;
            let mut vault_bootstrapped = false;
            if local_view == current_view {
                match repo.materialize() {
                    Ok(result) => {
                        materialized = true;
                        files_written = result.files_written as u64;
                    }
                    Err(error) => {
                        materialize_error = Some(error.to_string());
                    }
                }
            }
            // The pulled vault content deflates into redb (the log-only
            // notes ride nothing; only failures warn).
            if repo.vault_dir().exists() {
                if repo.has_vault().unwrap_or(false) {
                    if let Err(error) = repo.vault_record_working_copy() {
                        warnings.push(format!(
                            "Failed to synchronize pulled vault content: {error}"
                        ));
                    }
                } else {
                    match repo.bootstrap_vault_from_working_copy() {
                        Ok(()) => vault_bootstrapped = true,
                        Err(error) => {
                            warnings.push(format!("Failed to bootstrap vault: {error}"));
                        }
                    }
                }
            }
            Ok::<_, Status>((
                materialized,
                files_written,
                materialize_error,
                vault_bootstrapped,
                warnings,
            ))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;
        materialize_outcome
    } else {
        (false, 0u64, None, false, warnings)
    };

    // Download the remote view's tags.
    let (tags, warnings) = {
        let handle = handle.clone();
        let pull_pack = pull_pack.clone();
        let local_view = local_view.clone();
        let warnings_in = warnings.clone();
        tokio::task::spawn_blocking(move || {
            let repo = handle.repository()?;
            let mut warnings = warnings_in;
            let mut tags = Vec::new();
            for object in pull_pack
                .objects
                .iter()
                .filter(|o| o.family == ObjectFamily::Tag)
            {
                match atomic_repository::deserialize_tag(&object.bytes) {
                    Ok(tag) => {
                        if let Ok(Some(_)) = repo.get_tag_from_view(&tag.name, &local_view) {
                            continue;
                        }
                        match repo.save_synced_tag(&tag) {
                            Ok(()) => tags.push(PullTag {
                                name: tag.name.clone(),
                                kind: tag.kind.to_string(),
                            }),
                            Err(error) => {
                                warnings.push(format!("Failed to save tag '{}': {error}", tag.name))
                            }
                        }
                    }
                    Err(error) => {
                        warnings.push(format!("Failed to deserialize tag: {error}"));
                    }
                }
            }
            Ok::<_, Status>((tags, warnings))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??
    };

    let report = PullReport {
        remote_name,
        remote_url,
        remote_view,
        local_view,
        local_view_changes: local_entries_len as u64,
        graph_objects: 0,
        remote_changes: remote_entries,
        remote_state: Some(remote_state),
        local_only,
        downloads,
        changes_downloaded,
        bytes_transferred,
        applied,
        applied_changes,
        apply_errors,
        sidecars: sidecar_reports,
        set_id_verified,
        set_id_warning,
        materialized,
        files_written,
        materialize_error,
        tags,
        warnings: warnings
            .iter()
            .map(|text| PullWarning { text: text.clone() })
            .collect(),
        local_tip,
        vault_bootstrapped,
        reconciled_views,
        download_only: false,
        changes_failed,
    };
    Ok(Response::new(PullChangesResponse {
        applied_changes,
        updated_views: remote_chain
            .iter()
            .map(|manifest| manifest.name.clone())
            .collect(),
        report: Some(report),
        meta: super::services_agent::response_meta(&meta),
    }))
}

// ---------------------------------------------------------------------------
// BindRemoteProject
// ---------------------------------------------------------------------------

pub async fn bind_remote_project_impl(
    state: Arc<DaemonState>,
    request: Request<BindRemoteProjectRequest>,
) -> Result<Response<BindRemoteProjectResponse>, Status> {
    let _ = state;
    let _ = request;
    Err(Status::unimplemented(
        "BindRemoteProject lands with its slice",
    ))
}

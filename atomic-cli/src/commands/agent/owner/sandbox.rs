//! Remote sandboxes on the owner protocol: who may ask for what, the
//! requests that open and close a sandbox, and materializing its view.
//!
//! A local caller is trusted as it always was. A remote caller (iroh) gets
//! the provenance requests and `Materialize`, each checked against its
//! token: its own view, its own sessions and turns, and checkpoints only for
//! changes its view can see. Opening, renewing and closing sandboxes, and
//! shutting the owner down, are local only.

use std::path::Path;

use atomic_core::pristine::{GraphTxnT, ViewTxnT};
use atomic_core::types::{Base32, Hash};
use atomic_repository::redb_change_store::RedbChangeStore;
use atomic_repository::{Repository, ViewEntryKind};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::remote::{Grant, TokenRegistry};
use super::{
    read_frame, write_frame, OwnerRequest, OwnerResponse, OwnerState, RequestFrame, ResponseFrame,
    PROTOCOL_VERSION,
};

/// Who is on the other end of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Caller {
    /// The local socket or pipe.
    Local,
    /// iroh: trusted only as far as the frame's token.
    Remote,
}

fn error(code: &str, message: impl Into<String>) -> OwnerResponse {
    OwnerResponse::Error {
        code: code.to_string(),
        message: message.into(),
    }
}

fn refuse(code: &str, message: impl Into<String>) -> Box<OwnerResponse> {
    Box::new(error(code, message))
}

/// The grant `frame` acts under (`None` for a local caller), or the
/// refusal to send instead.
pub(crate) fn authorize(
    owner: &OwnerState,
    caller: Caller,
    frame: &RequestFrame,
) -> Result<Option<Grant>, Box<OwnerResponse>> {
    if caller == Caller::Local {
        return Ok(None);
    }
    let token = frame
        .token
        .as_deref()
        .ok_or_else(|| refuse("unauthorized", "a remote request needs a sandbox token"))?;
    let grant = owner
        .tokens
        .check(token)
        .map_err(|e| refuse("unauthorized", e.to_string()))?;
    let view = grant.view.as_str();
    let tokens = &owner.tokens;
    let forbidden = |what: &str| {
        refuse(
            "forbidden",
            format!("{what} is not this sandbox's (view '{view}')"),
        )
    };
    match &frame.request {
        OwnerRequest::Ping => {}
        OwnerRequest::Materialize { view: asked }
        | OwnerRequest::FileStates { view: asked, .. }
        | OwnerRequest::SubmitChange { view: asked, .. } => {
            if asked.as_deref().is_some_and(|v| v != view) {
                return Err(forbidden("that view"));
            }
        }
        OwnerRequest::ReserveProvenanceTurn {
            session_id,
            turn_number,
            ..
        } => {
            let fresh = session_is_fresh(&owner.store, session_id, *turn_number)
                .map_err(|e| refuse("provenance-store", e.to_string()))?;
            if !tokens.claim_session(view, session_id, fresh) {
                return Err(forbidden("that session"));
            }
        }
        OwnerRequest::AppendProvenanceEnvelopes { provenance_id, .. }
        | OwnerRequest::LoadFrozenEnvelopes { provenance_id }
        | OwnerRequest::LoadFrozenEnvelopesPage { provenance_id, .. }
        | OwnerRequest::AcknowledgeCheckpoint { provenance_id, .. } => {
            if !tokens.owns_provenance(view, *provenance_id) {
                return Err(forbidden("that provenance turn"));
            }
        }
        OwnerRequest::PrepareCheckpoint {
            provenance_id,
            source,
            ..
        } => {
            if !tokens.owns_provenance(view, *provenance_id) {
                return Err(forbidden("that provenance turn"));
            }
            require_on_view(&owner.root, view, &source.change_hashes)?;
        }
        OwnerRequest::BindCheckpointHash {
            provenance_id,
            hash,
            ..
        } => {
            if !tokens.owns_provenance(view, *provenance_id) {
                return Err(forbidden("that provenance turn"));
            }
            require_on_view(&owner.root, view, std::slice::from_ref(hash))?;
        }
        OwnerRequest::StopTurn { session_id, .. }
        | OwnerRequest::ResumeTurn { session_id, .. }
        | OwnerRequest::AbandonTurn { session_id, .. }
        | OwnerRequest::TurnStatus { session_id, .. } => {
            if !tokens.owns_session(view, session_id) {
                return Err(forbidden("that session"));
            }
        }
        OwnerRequest::OpenSandbox { .. }
        | OwnerRequest::RenewSandbox { .. }
        | OwnerRequest::CloseSandbox { .. }
        | OwnerRequest::Shutdown => {
            return Err(refuse("forbidden", "only a local caller may do that"));
        }
    }
    Ok(Some(grant))
}

/// Whether no one has recorded provenance for `session_id` up to this turn.
fn session_is_fresh(store: &RedbChangeStore, session_id: &str, turn: u32) -> anyhow::Result<bool> {
    for t in 1..=turn {
        if store.get_provenance_turn_for(session_id, t)?.is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Refuse unless every change in `hashes` is visible on `view`: a sandbox's
/// provenance can only describe its own view's work.
fn require_on_view(root: &Path, view: &str, hashes: &[Hash]) -> Result<(), Box<OwnerResponse>> {
    if hashes.is_empty() {
        return Ok(());
    }
    let check = || -> anyhow::Result<Option<Hash>> {
        let repo = Repository::open_readonly(root)?;
        let txn = repo.pristine().read_txn()?;
        let state = txn
            .get_view(view)?
            .ok_or_else(|| anyhow::anyhow!("view '{view}' no longer exists"))?;
        let visible = atomic_repository::collect_visible_change_ids(&txn, &state)?;
        for hash in hashes {
            match txn.get_internal(hash)? {
                Some(id) if visible.contains(&id) => {}
                _ => return Ok(Some(*hash)),
            }
        }
        Ok(None)
    };
    match check() {
        Ok(None) => Ok(()),
        Ok(Some(hash)) => Err(refuse(
            "forbidden",
            format!("change {} is not on view '{view}'", hash.to_base32()),
        )),
        Err(e) => Err(refuse("repository", e.to_string())),
    }
}

/// After a remote reservation succeeds, the turn is the sandbox's.
pub(crate) fn note_response(
    tokens: &TokenRegistry,
    grant: Option<&Grant>,
    response: &OwnerResponse,
) {
    if let (Some(grant), OwnerResponse::ProvenanceTurn { turn, .. }) = (grant, response) {
        tokens.bind_provenance(&grant.view, turn.provenance_id.get());
    }
}

/// `OpenSandbox`, `RenewSandbox`, `CloseSandbox` (local callers only).
pub(crate) async fn admin(
    owner: &std::sync::Arc<OwnerState>,
    request: OwnerRequest,
) -> OwnerResponse {
    match request {
        OwnerRequest::OpenSandbox {
            view,
            acting_as,
            ttl_secs,
        } => {
            match Repository::open_readonly(&owner.root)
                .map_err(anyhow::Error::from)
                .and_then(|repo| Ok(repo.pristine().read_txn()?.get_view(&view)?))
            {
                Ok(Some(_)) => {}
                Ok(None) => return error("view", format!("no view '{view}'")),
                Err(e) => return error("repository", e.to_string()),
            }
            let endpoint = match owner.remote_endpoint().await {
                Ok(endpoint) => endpoint,
                Err(e) => return error("remote", format!("{e:#}")),
            };
            let (token, grant) =
                owner
                    .tokens
                    .mint(&view, acting_as, chrono::Duration::seconds(ttl_secs));
            OwnerResponse::SandboxOpened {
                remote: super::remote::pointer_addr(endpoint, super::remote::offline()).await,
                view,
                token,
                expires: grant.expires.to_rfc3339(),
            }
        }
        OwnerRequest::RenewSandbox { view, ttl_secs } => {
            match owner
                .tokens
                .renew(&view, chrono::Duration::seconds(ttl_secs))
            {
                Ok(expires) => OwnerResponse::SandboxRenewed {
                    expires: expires.to_rfc3339(),
                },
                Err(e) => error("sandbox-token", e.to_string()),
            }
        }
        OwnerRequest::CloseSandbox { view } => OwnerResponse::SandboxClosed {
            revoked: owner.tokens.revoke(&view),
        },
        other => error("internal", format!("not a sandbox request: {other:?}")),
    }
}

/// `Materialize`: every entry of the view as its own frame, in path order,
/// then `Materialized`. The one request answered with more than one frame.
pub(crate) async fn materialize<S>(
    owner: &OwnerState,
    grant: Option<Grant>,
    frame: RequestFrame,
    stream: &mut S,
) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let request_id = frame.request_id;
    let respond = |response| ResponseFrame {
        version: PROTOCOL_VERSION,
        request_id: request_id.clone(),
        response,
    };
    let view = match (grant, frame.request) {
        (Some(grant), _) => grant.view,
        (None, OwnerRequest::Materialize { view: Some(view) }) => view,
        (None, _) => {
            return write_frame(
                stream,
                &respond(error("view", "name the view to materialize")),
            )
            .await;
        }
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<OwnerResponse>(64);
    let root = owner.root.clone();
    let render = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let repo = Repository::open_readonly(&root)?;
        let mut entries = 0u64;
        let mut live = std::collections::BTreeSet::new();
        let rendered = repo.materialize_view_entries(&view, |entry| {
            entries += 1;
            live.insert(entry.inode);
            tx.blocking_send(OwnerResponse::MaterializeEntry { entry })
                .map_err(|_| ())
        })?;
        match rendered {
            Ok(snapshot) => {
                let skeleton = Box::new(repo.export_sandbox_skeleton(&view, &live)?);
                let _ = tx.blocking_send(OwnerResponse::Materialized {
                    snapshot,
                    entries,
                    skeleton,
                });
                Ok(())
            }
            Err(()) => Err(anyhow::anyhow!("the client went away")),
        }
    });
    while let Some(response) = rx.recv().await {
        write_frame(stream, &respond(response)).await?;
    }
    match render.await? {
        Ok(()) => Ok(()),
        Err(e) => write_frame(stream, &respond(error("materialize", format!("{e:#}")))).await,
    }
}

/// `FileStates` and `SubmitChange`: the view is the token's (remote) or
/// the one named (local).
pub(crate) async fn record_request(
    owner: &OwnerState,
    grant: Option<Grant>,
    request: OwnerRequest,
) -> OwnerResponse {
    let named = match &request {
        OwnerRequest::FileStates { view, .. } | OwnerRequest::SubmitChange { view, .. } => {
            view.clone()
        }
        _ => None,
    };
    let Some(view) = grant.map(|g| g.view).or(named) else {
        return error("view", "name the view");
    };
    let root = owner.root.clone();
    match request {
        OwnerRequest::FileStates { inodes, .. } => {
            let read = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let repo = Repository::open_readonly(&root)?;
                Ok(repo.export_sandbox_slice(&view, &inodes)?)
            });
            match read.await {
                Ok(Ok(slice)) => OwnerResponse::FileStates {
                    slice: Box::new(slice),
                },
                Ok(Err(e)) => error("file-states", format!("{e:#}")),
                Err(e) => error("internal", e.to_string()),
            }
        }
        OwnerRequest::SubmitChange {
            base_state,
            hash,
            bytes,
            ..
        } => {
            let _one_at_a_time = owner.submissions.lock().await;
            let write = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let repo =
                    Repository::open_existing_wait(&root, std::time::Duration::from_secs(30))?;
                match repo.insert_submitted_change(&view, &base_state, &hash, &bytes)? {
                    Ok(submitted) => {
                        let skeleton =
                            repo.export_sandbox_skeleton(&view, &live_inodes(&repo, &view)?)?;
                        Ok(Ok((submitted, skeleton)))
                    }
                    Err(rejection) => Ok(Err(rejection)),
                }
            });
            match write.await {
                Ok(Ok(Ok((submitted, skeleton)))) => OwnerResponse::ChangeSubmitted {
                    submitted,
                    skeleton: Box::new(skeleton),
                },
                Ok(Ok(Err(rejection))) => OwnerResponse::ChangeRefused { rejection },
                Ok(Err(e)) => error("submit", format!("{e:#}")),
                Err(e) => error("internal", e.to_string()),
            }
        }
        other => error("internal", format!("not a record request: {other:?}")),
    }
}

/// The inodes `view`'s tree holds.
fn live_inodes(repo: &Repository, view: &str) -> anyhow::Result<std::collections::BTreeSet<u64>> {
    let mut live = std::collections::BTreeSet::new();
    repo.materialize_view_entries::<()>(view, |entry| {
        live.insert(entry.inode);
        Ok(())
    })?
    .map_err(|()| anyhow::anyhow!("unreachable"))?;
    Ok(live)
}

/// Client side of `Materialize`: write the view's tree into `dir` and return
/// how many entries it had. Files already at those paths are overwritten;
/// nothing else in `dir` is touched.
pub(crate) async fn receive_materialized<S>(
    stream: &mut S,
    request_id: &str,
    dir: &Path,
) -> anyhow::Result<(u64, atomic_repository::SandboxSkeleton)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut written = 0u64;
    loop {
        let frame: ResponseFrame = read_frame(stream).await?;
        if frame.request_id != request_id {
            anyhow::bail!("database owner response request id mismatch");
        }
        match frame.response {
            OwnerResponse::MaterializeEntry { entry } => {
                let path = dir.join(&entry.path);
                match entry.kind {
                    ViewEntryKind::Directory => std::fs::create_dir_all(&path)?,
                    #[cfg(unix)]
                    ViewEntryKind::Symlink => {
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        let target = String::from_utf8(entry.content.clone())?;
                        match std::fs::remove_file(&path) {
                            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                                return Err(e.into())
                            }
                            _ => {}
                        }
                        std::os::unix::fs::symlink(target, &path)?;
                    }
                    #[cfg(not(unix))]
                    ViewEntryKind::Symlink => {
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&path, &entry.content)?;
                    }
                    ViewEntryKind::File => {
                        if let Some(parent) = path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&path, &entry.content)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            std::fs::set_permissions(
                                &path,
                                std::fs::Permissions::from_mode(entry.mode as u32),
                            )?;
                        }
                    }
                }
                written += 1;
            }
            OwnerResponse::Materialized {
                entries, skeleton, ..
            } => {
                stream.shutdown().await.ok();
                if entries != written {
                    anyhow::bail!("owner sent {written} entries but said {entries}");
                }
                return Ok((written, *skeleton));
            }
            OwnerResponse::Error { code, message } => {
                anyhow::bail!("database owner materialize failed [{code}]: {message}")
            }
            other => anyhow::bail!("unexpected materialize response: {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use atomic_core::change::ChangeHeader;
    use atomic_repository::redb_change_store::ProvenanceCheckpointSource;
    use atomic_repository::{InsertOptions, RecordOptions, TrackingOptions};

    use super::*;

    /// A repository with one change on `dev`, and an owner for it.
    fn owner() -> (tempfile::TempDir, Arc<OwnerState>, Hash) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let repo = Repository::init(&root).unwrap();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        repo.add("a.txt", TrackingOptions::default()).unwrap();
        let options = RecordOptions::new()
            .with_all(true)
            .save_to_store(true)
            .apply_after_record(false);
        let outcome = repo.record(ChangeHeader::new("a"), options).unwrap();
        repo.write_recorded(&outcome, InsertOptions::default())
            .unwrap();
        let hash = *outcome.hash();
        drop(repo);
        let store = Arc::new(RedbChangeStore::open(dir.path().join("journal.redb")).unwrap());
        (dir, OwnerState::new(store, root.join(".atomic")), hash)
    }

    fn frame(token: Option<&str>, request: OwnerRequest) -> RequestFrame {
        RequestFrame {
            version: PROTOCOL_VERSION,
            request_id: "r".into(),
            token: token.map(str::to_string),
            request,
        }
    }

    fn prepare(provenance_id: u64, change_hashes: Vec<Hash>) -> OwnerRequest {
        OwnerRequest::PrepareCheckpoint {
            provenance_id,
            expected_generation: 1,
            source: ProvenanceCheckpointSource {
                agent_name: "a".into(),
                agent_display_name: "A".into(),
                agent_vendor: "v".into(),
                change_hashes,
                previous_provenance: None,
                plan_id: None,
                ledger_turn_number: 1,
            },
            reuse_frozen_changes: false,
            now: 0,
        }
    }

    fn refusal(result: Result<Option<Grant>, Box<OwnerResponse>>) -> String {
        match result.map_err(|e| *e) {
            Err(OwnerResponse::Error { code, .. }) => code,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_remote_checkpoint_covers_only_its_own_turn_and_its_view_s_changes() {
        let (_dir, owner, on_view) = owner();
        let (token, _) = owner.tokens.mint("dev", None, chrono::Duration::hours(1));
        owner.tokens.bind_provenance("dev", 7);
        let remote = |request| authorize(&owner, Caller::Remote, &frame(Some(&token), request));

        assert!(remote(prepare(7, vec![on_view])).unwrap().is_some());
        assert_eq!(
            refusal(remote(prepare(8, vec![on_view]))),
            "forbidden",
            "not its turn"
        );
        let elsewhere = Hash::of(b"a change on no view");
        assert_eq!(
            refusal(remote(prepare(7, vec![elsewhere]))),
            "forbidden",
            "not its view's change"
        );

        assert_eq!(
            refusal(authorize(
                &owner,
                Caller::Remote,
                &frame(None, OwnerRequest::Ping)
            )),
            "unauthorized"
        );
        assert!(
            authorize(
                &owner,
                Caller::Local,
                &frame(None, prepare(8, vec![elsewhere]))
            )
            .unwrap()
            .is_none(),
            "a local caller is trusted as before"
        );
    }
}

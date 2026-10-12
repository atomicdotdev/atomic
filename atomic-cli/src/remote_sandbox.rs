//! The CLI inside a remote sandbox.
//!
//! A remote sandbox is a directory holding a pointer
//! ([`atomic_repository::RemoteSandboxPointer`]: repository, view, token —
//! no address) and, once materialized, a cache of its view's rows under
//! `.atomic-sandbox.d/`. It reaches the repository that serves it through
//! the CLI's ordinary daemon transport: the atomic-client channel over
//! `ATOMIC_DAEMON_SOCKET`. Whatever listens on that socket — a local atomicd,
//! or a host forwarding the bytes to its own server — is not this module's
//! concern; it never starts a daemon and never resolves a repository path.
//!
//! Every request carries the pointer's token in `x-atomic-sandbox-token-bin`
//! and names the pointer's `RepositoryRef`.
//!
//! Commands run their local bodies over the cache (`status`, `record`,
//! `diff`, `log`, `restore`, `intent`, ...): `Repository` resolves the
//! pointer to the cache, and the cache calls back here through the
//! [`RemoteSandboxLink`] this module installs for slices, change files,
//! submissions and provenance. The agent hooks' provenance journal goes over
//! ProvenanceService with the same token (`agent::provenance_rpc`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use atomic_client::proto as pb;
use atomic_core::change::session::{SessionCheckpointPublication, SessionTurn};
use atomic_core::types::{Base32, Hash};
use atomic_repository::{
    ChangeFile, RemoteRepositoryRef, RemoteSandboxLink, RemoteSandboxPointer, Repository,
    SandboxSlice, SubmitRejection, Submitted, SubmittedOutcome,
};
use libatomic::daemon::sandbox_grants::SANDBOX_TOKEN_METADATA;
use libatomic::daemon::sandbox_wire as wire;
use tonic::metadata::{Binary, MetadataValue};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::{CliError, CliResult};

/// Attaches a sandbox token to every request (or nothing, without one).
#[derive(Clone, Default)]
pub struct TokenInterceptor(Option<MetadataValue<Binary>>);

impl TokenInterceptor {
    pub fn new(token: Option<&[u8]>) -> Self {
        Self(token.map(MetadataValue::from_bytes))
    }
}

impl tonic::service::Interceptor for TokenInterceptor {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(token) = &self.0 {
            request
                .metadata_mut()
                .insert_bin(SANDBOX_TOKEN_METADATA, token.clone());
        }
        Ok(request)
    }
}

/// A channel whose every request carries the sandbox token.
pub type SandboxChannel = InterceptedService<Channel, TokenInterceptor>;

/// The remote sandbox the current directory is in, if any: its root and
/// pointer.
pub fn current() -> Option<(PathBuf, RemoteSandboxPointer)> {
    let cwd = std::env::current_dir().ok()?;
    atomic_repository::find_remote_sandbox(&cwd)
}

/// A refusal naming the sandbox and why.
pub fn refusal(root: &Path, why: &str) -> CliError {
    CliError::ServiceRefusal {
        message: format!(
            "not available in a remote sandbox ({}): {why}",
            root.display()
        ),
    }
}

/// The pointer's repository as the contract names it.
pub fn reference(pointer: &RemoteSandboxPointer) -> Result<pb::RepositoryRef, String> {
    Ok(pb::RepositoryRef {
        authority: pointer.repository.authority.clone(),
        repository_id: data_encoding::HEXLOWER_PERMISSIVE
            .decode(pointer.repository.repository_id.as_bytes())
            .map_err(|e| format!("the sandbox pointer's repository_id is not hex: {e}"))?,
        workspace_id: None,
    })
}

/// The view the pointer names, sent as every request's target: a token for
/// another view is refused rather than quietly serving that view.
pub fn target(pointer: &RemoteSandboxPointer) -> pb::ViewRef {
    pb::ViewRef {
        view_id: pointer
            .view_id
            .as_deref()
            .and_then(|id| {
                data_encoding::HEXLOWER_PERMISSIVE
                    .decode(id.as_bytes())
                    .ok()
            })
            .unwrap_or_default(),
        name: Some(pointer.view.clone()),
    }
}

/// The pointer form of an OpenSandbox reply.
pub fn pointer_from(opened: &pb::SandboxOpened) -> CliResult<RemoteSandboxPointer> {
    let repository = opened
        .repository
        .as_ref()
        .ok_or_else(|| CliError::Internal(anyhow::anyhow!("OpenSandbox returned no repository")))?;
    let token = String::from_utf8(opened.token.clone()).map_err(|_| {
        CliError::Internal(anyhow::anyhow!(
            "the sandbox token is not text; a pointer cannot carry it"
        ))
    })?;
    Ok(RemoteSandboxPointer {
        repository: RemoteRepositoryRef {
            authority: repository.authority.clone(),
            repository_id: data_encoding::HEXLOWER.encode(&repository.repository_id),
        },
        view: opened.view.clone(),
        view_id: opened
            .target
            .as_ref()
            .filter(|t| !t.view_id.is_empty())
            .map(|t| data_encoding::HEXLOWER.encode(&t.view_id)),
        token,
    })
}

/// Connect to whatever serves the daemon socket — never starting one: a
/// remote sandbox has no daemon of its own to start.
pub async fn connect(token: Option<&[u8]>) -> Result<SandboxChannel, String> {
    let client = atomic_client::AtomicClient::connect_existing().await?;
    Ok(InterceptedService::new(
        client.channel,
        TokenInterceptor::new(token),
    ))
}

fn sandbox_client(
    channel: SandboxChannel,
) -> pb::sandbox_service_client::SandboxServiceClient<SandboxChannel> {
    pb::sandbox_service_client::SandboxServiceClient::new(channel)
        .max_decoding_message_size(64 * 1024 * 1024)
        .max_encoding_message_size(64 * 1024 * 1024)
}

/// Run `exchange` against the sandbox's server on a fresh thread and
/// runtime: the link is called synchronously from inside `Repository`, which
/// may itself be running inside an async context (the agent hooks).
fn exchange<T, F, Fut>(root: &Path, exchange: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(pb::RepositoryRef, pb::ViewRef, SandboxChannel) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let pointer = RemoteSandboxPointer::read(root)
        .ok_or_else(|| format!("{} holds no remote sandbox pointer", root.display()))?;
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async move {
            let reference = reference(&pointer)?;
            let channel = connect(Some(pointer.token.as_bytes())).await?;
            exchange(reference, target(&pointer), channel).await
        })
    })
    .join()
    .map_err(|_| "the sandbox link's thread panicked".to_string())?
}

fn refused(status: tonic::Status) -> String {
    status.message().to_string()
}

/// The CLI's way from a remote sandbox's cache to the serving repository.
pub struct CliLink;

impl RemoteSandboxLink for CliLink {
    fn file_states(&self, root: &Path, inodes: Vec<u64>) -> Result<SandboxSlice, String> {
        exchange(root, move |repository, target, channel| async move {
            let response = sandbox_client(channel)
                .get_file_states(pb::GetFileStatesRequest {
                    repository: Some(repository),
                    inodes,
                    target: Some(target),
                    expected_snapshot: None,
                })
                .await
                .map_err(refused)?
                .into_inner();
            wire::slice_from_proto(&response.slice.ok_or("no slice in GetFileStates")?)
        })
    }

    fn submit(
        &self,
        root: &Path,
        base_state: String,
        hash: Hash,
        bytes: Vec<u8>,
    ) -> Result<SubmittedOutcome, String> {
        exchange(root, move |repository, target, channel| async move {
            let fence = Hash::from_base32(base_state.as_bytes()).ok_or("bad base state")?;
            let response = sandbox_client(channel)
                .submit_change(pb::SubmitChangeRequest {
                    repository: Some(repository),
                    meta: Some(pb::RequestMeta {
                        request_id: uuid::Uuid::new_v4().to_string(),
                        observed_at: None,
                    }),
                    change: Some(wire::change_bundle(&hash, bytes)),
                    expected_snapshot: Some(wire::view_snapshot("", &fence, None)),
                    target: Some(target),
                })
                .await
                .map_err(refused)?
                .into_inner();
            match response.outcome.ok_or("no outcome in SubmitChange")? {
                pb::submit_change_response::Outcome::Submitted(submitted) => {
                    let skeleton = wire::skeleton_from_proto(
                        &submitted.skeleton.ok_or("no skeleton in SubmitChange")?,
                    )?;
                    let effective = response
                        .meta
                        .and_then(|m| m.snapshot)
                        .map(|s| wire::snapshot_fence(&s))
                        .transpose()?
                        .unwrap_or_default();
                    Ok(Ok((
                        Submitted {
                            hash,
                            state: skeleton.view.state.to_base32(),
                            effective,
                        },
                        skeleton,
                    )))
                }
                pb::submit_change_response::Outcome::Refused(refused) => {
                    let info = refused.rejection.ok_or("no rejection in a refusal")?;
                    let rejection = wire::rejection_from_error_info(&info)
                        .ok_or_else(|| info.message.clone())?;
                    let skeleton = refused
                        .skeleton
                        .map(|s| wire::skeleton_from_proto(&s))
                        .transpose()?;
                    Ok(Err((rejection, skeleton)))
                }
            }
        })
    }

    fn changes(&self, root: &Path, hashes: Vec<Hash>) -> Result<Vec<ChangeFile>, String> {
        exchange(root, move |repository, target, channel| async move {
            let response = sandbox_client(channel)
                .get_changes(pb::GetChangesRequest {
                    repository: Some(repository),
                    hashes: hashes
                        .iter()
                        .map(libatomic::daemon::convert::hash_proto)
                        .collect(),
                    target: Some(target),
                    expected_snapshot: None,
                })
                .await
                .map_err(refused)?
                .into_inner();
            match response.outcome.ok_or("no outcome in GetChanges")? {
                pb::get_changes_response::Outcome::Changes(payload) => payload
                    .changes
                    .iter()
                    .map(wire::change_from_bundle)
                    .collect(),
                pb::get_changes_response::Outcome::Refused(info) => Err(info.message),
            }
        })
    }

    fn publish_provenance(
        &self,
        root: &Path,
        graph: Vec<u8>,
        turn: SessionTurn,
    ) -> Result<Result<SessionCheckpointPublication, SubmitRejection>, String> {
        exchange(root, move |repository, target, channel| async move {
            let response = sandbox_client(channel)
                .publish_provenance(pb::PublishProvenanceRequest {
                    repository: Some(repository),
                    meta: Some(pb::RequestMeta {
                        request_id: uuid::Uuid::new_v4().to_string(),
                        observed_at: None,
                    }),
                    graph: Some(wire::provenance_graph_bytes(graph)),
                    turn: Some(pb::SessionTurn {
                        session_id: turn.session_id.clone(),
                        turn_number: turn.turn_number,
                    }),
                    expected_generation: 0,
                    target: Some(target),
                    session_turn: Some(wire::session_turn_bytes(&turn)?),
                })
                .await
                .map_err(refused)?
                .into_inner();
            match response.outcome.ok_or("no outcome in PublishProvenance")? {
                pb::publish_provenance_response::Outcome::Publication(published) => {
                    Ok(Ok(wire::publication_from(&published)?))
                }
                pb::publish_provenance_response::Outcome::Refused(info) => {
                    Ok(Err(wire::rejection_from_error_info(&info)
                        .ok_or_else(|| info.message.clone())?))
                }
            }
        })
    }
}

/// Install the link, once per process. Harmless outside a remote sandbox:
/// it is only ever called by a remote sandbox's cache.
pub fn install_link() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| atomic_repository::set_remote_sandbox_link(Box::new(CliLink)));
}

/// `atomic sandbox materialize` inside a remote sandbox: write the view's
/// tree into `root` and (re)make its cache. Returns the number of entries
/// and the view.
///
/// Every entry goes through the kernel's path-checked writer (no climbing
/// out, no writing the pointer or the cache). A fresh cache also clears a
/// sandbox that had fallen behind.
pub fn materialize(root: &Path) -> CliResult<(u64, String)> {
    use tokio_stream::StreamExt;

    let internal = |e: String| CliError::Internal(anyhow::anyhow!(e));
    let pointer = RemoteSandboxPointer::read(root).ok_or_else(|| {
        internal(format!(
            "{} holds no remote sandbox pointer",
            root.display()
        ))
    })?;
    let root_for_thread = root.to_path_buf();
    let (entries, skeleton) = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async move {
            let repository = reference(&pointer)?;
            let channel = connect(Some(pointer.token.as_bytes())).await?;
            let mut stream = sandbox_client(channel)
                .materialize(pb::MaterializeRequest {
                    repository: Some(repository),
                    target: Some(target(&pointer)),
                })
                .await
                .map_err(refused)?
                .into_inner();
            let mut written = 0u64;
            while let Some(frame) = stream.next().await {
                match frame.map_err(refused)?.frame {
                    Some(pb::materialize_frame::Frame::Entry(entry)) => {
                        atomic_repository::write_sandbox_entry(
                            &root_for_thread,
                            &entry.path,
                            wire::tree_entry_kind(&entry)?,
                            entry.mode.unwrap_or(0o644),
                            entry.inline.as_deref().unwrap_or_default(),
                        )
                        .map_err(|e| e.to_string())?;
                        written += 1;
                    }
                    Some(pb::materialize_frame::Frame::Done(done)) => {
                        if done.entries != written {
                            return Err(format!(
                                "the server sent {written} entries but said {}",
                                done.entries
                            ));
                        }
                        let skeleton = wire::skeleton_from_proto(
                            &done.skeleton.ok_or("no skeleton in the summary")?,
                        )?;
                        return Ok((written, skeleton));
                    }
                    None => return Err("an empty materialize frame".to_string()),
                }
            }
            Err("materialize ended without its summary".to_string())
        })
    })
    .join()
    .map_err(|_| internal("the materialize thread panicked".to_string()))?
    .map_err(|e| CliError::ServiceRefusal { message: e })?;
    let view = skeleton.view.name.clone();
    install_link();
    drop(Repository::create_remote_sandbox_cache(root, &skeleton).map_err(CliError::Repository)?);
    Ok((entries, view))
}

/// The commands a remote sandbox runs: those that work off its cache and
/// reach the serving repository only through its grant. Anything else
/// would act on a repository the sandbox does not have.
pub fn command_allowed(name: &str) -> bool {
    matches!(
        name,
        "agent"
            | "status"
            | "add"
            | "remove"
            | "move"
            | "restore"
            | "record"
            | "log"
            | "change"
            | "diff"
            | "conflicts"
            | "sandbox"
            | "intent"
            | "memory"
            | "vault"
            | "query"
            | "triage"
            | "session"
            | "provenance"
            | "identity"
            | "completions"
    )
}

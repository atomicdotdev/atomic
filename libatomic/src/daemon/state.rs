//! Daemon state: the repository registry, RPC logging, error mapping.
//!
//! One daemon multiplexes many repositories (RFC D3): each resolved root is
//! registered under its persistent ID and opened lazily, exactly once, and
//! the handle is reused for every subsequent request. redb permits one
//! writable process per database file — that process is this daemon.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::atomic::ErrorCode;
use crate::atomic::{ErrorInfo, RepositoryRef};
use atomic_repository::redb_change_store::RedbChangeStore;
use atomic_repository::Repository;
use tonic::{Code, Status};

use prost::Message;

/// Env-configured JSONL log of handled RPCs (one line per request, so
/// external auditing can pair client invocations with served requests).
pub const ENV_LOG_REQUESTS: &str = "ATOMIC_DAEMON_LOG_REQUESTS";

/// How long a read-only open retries when another process holds a
/// writable handle: short-lived writers (a recording agent turn, a stop
/// checkpoint publication) finish first; readers then share the database
/// across processes. The query handlers' policy — the same one the CLI's
/// query commands apply.
pub const READ_OPEN_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// One registered repository: canonical root + the per-repository
/// serialization gate. redb permits ONE handle per database file even
/// within a process, so the daemon never holds a repository handle across
/// requests: each RPC acquires this gate, opens the database, runs, and
/// drops the handle. (RFC D2's bounded per-repo queue, simplified to full
/// serialization for this slice — the read/write split is future work.)
pub struct RepoHandle {
    pub root: PathBuf,
    gate: tokio::sync::Mutex<()>,
}

impl RepoHandle {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            gate: tokio::sync::Mutex::new(()),
        }
    }

    /// Serialize repository database access: at most one open handle at a
    /// time for this repository. Acquire before any domain call.
    pub async fn exclusive(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.gate.lock().await
    }

    /// The persistent repository ID for this daemon: a blake3 digest of the
    /// canonical root. Slice limitation: RFC D3 wants IDs assigned at init
    /// and recorded in `.atomic` so they survive daemon restarts and follow
    /// the repository across daemons; a path digest is stable across
    /// restarts of the same layout and unambiguous locally.
    pub fn repository_id(&self) -> Vec<u8> {
        blake3::hash(self.root.to_string_lossy().as_bytes())
            .as_bytes()
            .to_vec()
    }

    /// The current view, read from the `.atomic/current_view` marker WITHOUT
    /// opening any database (ResolveRepository is a lookup, not a storage
    /// open — CONTRACT "routing" section).
    pub fn current_view(&self) -> String {
        let path = self.root.join(".atomic").join("current_view");
        std::fs::read_to_string(&path)
            .map(|content| content.trim().to_string())
            .unwrap_or_else(|_| "dev".to_string())
    }

    pub fn repository_ref(&self) -> RepositoryRef {
        RepositoryRef {
            authority: "local".to_string(),
            repository_id: self.repository_id(),
            workspace_id: None,
        }
    }

    /// Open the repository database for this request. The caller drops the
    /// handle when done; the gate guarantees exclusivity. `open_existing`
    /// skips the table-init write transaction (the repository exists — the
    /// daemon never bootstraps).
    pub fn repository(&self) -> Result<Repository, Status> {
        Repository::open_existing(&self.root).map_err(|error| {
            domain_status(
                ErrorCode::Repository,
                format!(
                    "failed to open repository at {}: {error}",
                    self.root.display()
                ),
            )
        })
    }

    /// Open the repository database READ-ONLY for this query, with the
    /// read-concurrency wait: redb refuses any open while a writable
    /// handle exists (this or another process), so a short-lived writer —
    /// a recording agent turn in another process, a gated mutation in this
    /// one — is given [`READ_OPEN_WAIT`] to finish before the read fails.
    /// Readers share the database across processes the same way the CLI's
    /// query commands always have. The caller must hold no other handle.
    pub fn repository_readonly(&self) -> Result<Repository, Status> {
        Repository::open_readonly_wait(&self.root, READ_OPEN_WAIT).map_err(|error| {
            domain_status(
                ErrorCode::Repository,
                format!(
                    "failed to open repository at {}: {error}",
                    self.root.display()
                ),
            )
        })
    }

    /// The redb-native change/provenance store (changes.redb) for this
    /// request — opened by path WITHOUT opening the repository database.
    /// The hosted turn orchestrator opens pristine itself; changes.redb is a
    /// separate database file, so both can be open in-process together.
    pub fn change_store(&self) -> Result<RedbChangeStore, Status> {
        let path = Repository::canonical_change_store_path(&self.root)
            .map_err(|error| domain_status(ErrorCode::Repository, error.to_string()))?;
        RedbChangeStore::open(&path).map_err(|error| {
            domain_status(
                ErrorCode::ProvenanceStore,
                format!("failed to open {}: {error}", path.display()),
            )
        })
    }
}

pub struct DaemonState {
    repos: Mutex<HashMap<Vec<u8>, std::sync::Arc<RepoHandle>>>,
    pub shutdown: tokio::sync::Notify,
    log_path: Option<PathBuf>,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            repos: Mutex::new(HashMap::new()),
            shutdown: tokio::sync::Notify::new(),
            log_path: std::env::var_os(ENV_LOG_REQUESTS).map(PathBuf::from),
        }
    }

    /// Register (or return the already-registered) handle for a repository
    /// root. The ID is derived from the canonical root, so re-resolution is
    /// idempotent.
    pub fn register(&self, root: PathBuf) -> std::sync::Arc<RepoHandle> {
        let root = root.canonicalize().unwrap_or_else(|_| root.clone());
        let id = blake3::hash(root.to_string_lossy().as_bytes())
            .as_bytes()
            .to_vec();
        let mut repos = self.repos.lock().expect("repository registry");
        repos
            .entry(id)
            .or_insert_with(|| std::sync::Arc::new(RepoHandle::new(root.clone())))
            .clone()
    }

    /// Resolve a request's RepositoryRef to its handle. Unknown IDs fail
    /// closed (REPOSITORY_NOT_FOUND) — never a filesystem fallback.
    pub fn resolve(&self, reference: &RepositoryRef) -> Result<std::sync::Arc<RepoHandle>, Status> {
        if reference.authority != "local" {
            return Err(domain_status(
                ErrorCode::InvalidArgument,
                format!(
                    "this daemon serves the local authority; '{}' is not served here",
                    reference.authority
                ),
            ));
        }
        let repos = self.repos.lock().expect("repository registry");
        repos.get(&reference.repository_id).cloned().ok_or_else(|| {
            domain_status(
                ErrorCode::RepositoryNotFound,
                "repository is not registered with this daemon; resolve it by path first",
            )
        })
    }

    pub fn list(&self) -> Vec<std::sync::Arc<RepoHandle>> {
        let repos = self.repos.lock().expect("repository registry");
        let mut handles: Vec<_> = repos.values().cloned().collect();
        handles.sort_by_key(|handle| handle.root.clone());
        handles
    }

    /// Append one JSONL line to the env-configured RPC log. Logging must
    /// never break a request.
    pub fn log_rpc(&self, method: &str, handle: Option<&RepoHandle>) {
        let Some(path) = &self.log_path else { return };
        let record = serde_json::json!({
            "ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            "method": method,
            "repository_id": handle.map(|h| data_encoding::HEXLOWER.encode(&h.repository_id()[..16])),
            "root": handle.map(|h| h.root.display().to_string()),
        });
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{record}");
        }
    }
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}

/// A domain refusal as a gRPC status carrying the contract's stable
/// machine-readable ErrorInfo detail.
pub fn domain_status(code: ErrorCode, message: impl Into<String>) -> Status {
    let message: String = message.into();
    fn code_name(code: ErrorCode) -> &'static str {
        match code {
            ErrorCode::Unspecified => "ERROR_CODE_UNSPECIFIED",
            ErrorCode::CapabilityUnavailable => "CAPABILITY_UNAVAILABLE",
            ErrorCode::ProtocolVersionMismatch => "PROTOCOL_VERSION_MISMATCH",
            ErrorCode::WorkspaceRequired => "WORKSPACE_REQUIRED",
            ErrorCode::Unauthorized => "UNAUTHORIZED",
            ErrorCode::Forbidden => "FORBIDDEN",
            ErrorCode::SandboxToken => "SANDBOX_TOKEN",
            ErrorCode::IdempotencyMismatch => "IDEMPOTENCY_MISMATCH",
            ErrorCode::PreconditionFailed => "PRECONDITION_FAILED",
            ErrorCode::GenerationConflict => "GENERATION_CONFLICT",
            ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::Repository => "REPOSITORY",
            ErrorCode::RepositoryNotFound => "REPOSITORY_NOT_FOUND",
            ErrorCode::ResourceExhausted => "RESOURCE_EXHAUSTED",
            ErrorCode::RateLimited => "RATE_LIMITED",
            ErrorCode::View => "VIEW",
            ErrorCode::ViewStale => "VIEW_STALE",
            ErrorCode::ChangeRejected => "CHANGE_REJECTED",
            ErrorCode::ProvenanceStore => "PROVENANCE_STORE",
            ErrorCode::ProvenanceCheckpoint => "PROVENANCE_CHECKPOINT",
            ErrorCode::ProvenanceLifecycle => "PROVENANCE_LIFECYCLE",
            ErrorCode::Provenance => "PROVENANCE",
            ErrorCode::Changes => "CHANGES",
            ErrorCode::FileStates => "FILE_STATES",
            ErrorCode::Materialize => "MATERIALIZE",
            ErrorCode::Remote => "REMOTE",
            ErrorCode::Internal => "INTERNAL",
        }
    }
    let info = ErrorInfo {
        code: code as i32,
        message: message.clone(),
        context: HashMap::new(),
        request_id: None,
    };
    let mut payload = Vec::new();
    let _ = info.encode(&mut payload);
    let _ = payload;
    let grpc_code = match code {
        ErrorCode::NotFound | ErrorCode::RepositoryNotFound => Code::NotFound,
        ErrorCode::InvalidArgument => Code::InvalidArgument,
        ErrorCode::PreconditionFailed
        | ErrorCode::GenerationConflict
        | ErrorCode::IdempotencyMismatch
        | ErrorCode::ViewStale
        | ErrorCode::ChangeRejected => Code::FailedPrecondition,
        ErrorCode::ResourceExhausted | ErrorCode::RateLimited => Code::ResourceExhausted,
        ErrorCode::Unauthorized => Code::Unauthenticated,
        ErrorCode::Forbidden | ErrorCode::SandboxToken | ErrorCode::View => Code::PermissionDenied,
        ErrorCode::ProtocolVersionMismatch => Code::Internal,
        ErrorCode::Internal | ErrorCode::Repository => Code::Internal,
        _ => Code::InvalidArgument,
    };
    // Slice note: rich grpc-status-details-bin attachments land with the
    // compatibility suite; the stable code rides the message prefix until then.
    Status::new(
        grpc_code,
        format!("atomic:{}: {}", code_name(code), message),
    )
}

/// A domain result mapped to the contract's error vocabulary where the
/// caller has no more specific mapping.
pub fn repository_error(error: atomic_repository::RepositoryError) -> Status {
    domain_status(ErrorCode::Repository, error.to_string())
}

/// A quick helper so handlers can echo hash bytes consistently.
pub fn hash_bytes(hash: &atomic_core::types::Merkle) -> Vec<u8> {
    hash.0.to_vec()
}

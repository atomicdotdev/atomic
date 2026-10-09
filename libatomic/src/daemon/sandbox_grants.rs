//! Sandbox grants: what a sandbox token reaches, and the seam a host plugs
//! its own grant store into.
//!
//! A remote sandbox is trusted only as far as its grant: exactly one view of
//! one repository, optionally as one acting identity, with an explicit
//! capability subset, until it expires. Provenance sessions and turns a
//! sandbox starts are bound to its grant's view, and it can touch no other.
//!
//! libatomic carries no transport. A host process serves the handlers and
//! decides how a caller is authenticated; a sandbox's grant reaches a handler
//! one of two ways (see [`DaemonState::sandbox_caller`]):
//!
//! 1. the host has already verified the caller and attached a
//!    [`SandboxGrant`] to the request's extensions
//!    (`request.extensions_mut().insert(grant)`), or
//! 2. the request carries the contract's `x-atomic-sandbox-token-bin`
//!    metadata, which the state's [`SandboxGrants`] resolves.
//!
//! A request with neither is a local, trusted caller — the host must not let
//! an untrusted caller reach the handlers without one.
//!
//! [`InMemorySandboxGrants`] is the default store: token hashes only, one live
//! token per view, gone when the process ends. A host that wants grants to
//! survive a restart implements [`SandboxGrants`] over its own database and
//! hands it to [`DaemonState::with_sandbox_grants`].

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use rand::RngCore;
use tonic::{Request, Status};

use crate::atomic::{ErrorCode, RepositoryRef, ViewRef};

use super::state::{domain_status, DaemonState, RepoHandle};

/// The gRPC metadata key a sandbox token travels in (binary metadata).
pub const SANDBOX_TOKEN_METADATA: &str = "x-atomic-sandbox-token-bin";

/// Bounds on a grant's lifetime, in seconds. Both ends matter: a zero TTL
/// would mint a token that is already dead, and an enormous one is either a
/// mistake or a way to make a token that outlives its purpose.
pub const MIN_TTL_SECS: u64 = 1;
pub const MAX_TTL_SECS: u64 = 365 * 24 * 60 * 60;
/// Two hours: long enough to outlast a missed renewal or two.
pub const DEFAULT_TTL_SECS: u64 = 2 * 60 * 60;

/// The only capabilities a sandbox grant may carry (CONTRACT.md "Sandbox
/// grants and visibility").
pub const DELEGABLE_CAPABILITIES: &[&str] = &[
    "sandbox.read",
    "sandbox.submit",
    "provenance.read",
    "provenance.write",
    "agent.checkpoint",
    "vault.read",
    "vault.write",
    "knowledge.read",
    "attestation.read",
    "attestation.write",
];

/// The (service, method) pairs a sandbox principal may call (the contract's
/// sandbox allowlist, `check_contract.py`'s `SANDBOX_METHODS`). A host routes
/// every other method away from a sandbox principal before dispatch; the
/// SandboxService data ops and the provenance turn/checkpoint RPCs here
/// additionally enforce the grant's scope themselves.
pub const SANDBOX_METHODS: &[(&str, &str)] = &[
    ("DaemonService", "Health"),
    ("DaemonService", "GetCapabilities"),
    ("SandboxService", "Materialize"),
    ("SandboxService", "GetFileStates"),
    ("SandboxService", "GetChanges"),
    ("SandboxService", "SubmitChange"),
    ("SandboxService", "PublishProvenance"),
    ("ProvenanceService", "ReserveTurn"),
    ("ProvenanceService", "AppendEnvelopes"),
    ("ProvenanceService", "PrepareCheckpoint"),
    ("ProvenanceService", "LoadFrozenEnvelopes"),
    ("ProvenanceService", "BindCheckpointHash"),
    ("ProvenanceService", "AcknowledgeCheckpoint"),
    ("ProvenanceService", "UpdateTurn"),
    ("ProvenanceService", "GetTurn"),
    ("ProvenanceService", "GetSession"),
    ("ProvenanceService", "GetProvenance"),
    ("ProvenanceService", "ExportProvenance"),
    ("VaultService", "InitVault"),
    ("VaultService", "GetVaultEntry"),
    ("VaultService", "ListVaultEntries"),
    ("VaultService", "GetVaultContext"),
    ("VaultService", "CreateVaultEntity"),
    ("VaultService", "UpdateVaultEntity"),
    ("VaultService", "DeleteVaultEntity"),
    ("VaultService", "ValidateVaultEntity"),
    ("VaultService", "LinkVaultEntities"),
    ("AttestationService", "PrepareAttestation"),
    ("AttestationService", "RecordAttestation"),
    ("AttestationService", "VerifyAttestation"),
    ("AttestationService", "ListAttestations"),
    ("KnowledgeService", "QueryGraph"),
];

/// The subset of [`SANDBOX_METHODS`] whose handlers here enforce a sandbox
/// grant's scope themselves (view, sessions, turns, capabilities). The
/// Vault/Knowledge/Attestation handlers and the session/provenance reads
/// are not yet view-scoped: until they are, a host should refuse a sandbox
/// principal on everything outside this list ([`is_sandbox_scoped`]).
pub const SANDBOX_SCOPED_METHODS: &[(&str, &str)] = &[
    ("DaemonService", "Health"),
    ("DaemonService", "GetCapabilities"),
    ("SandboxService", "Materialize"),
    ("SandboxService", "GetFileStates"),
    ("SandboxService", "GetChanges"),
    ("SandboxService", "SubmitChange"),
    ("SandboxService", "PublishProvenance"),
    ("ProvenanceService", "ReserveTurn"),
    ("ProvenanceService", "AppendEnvelopes"),
    ("ProvenanceService", "PrepareCheckpoint"),
    ("ProvenanceService", "LoadFrozenEnvelopes"),
    ("ProvenanceService", "BindCheckpointHash"),
    ("ProvenanceService", "AcknowledgeCheckpoint"),
    ("ProvenanceService", "UpdateTurn"),
    ("ProvenanceService", "GetTurn"),
];

/// Whether a gRPC path (`/atomic.SandboxService/Materialize`) is one a
/// sandbox principal may be dispatched to ([`SANDBOX_SCOPED_METHODS`]).
pub fn is_sandbox_scoped(grpc_path: &str) -> bool {
    let mut parts = grpc_path.trim_start_matches('/').splitn(2, '/');
    let (Some(service), Some(method)) = (parts.next(), parts.next()) else {
        return false;
    };
    let service = service.strip_prefix("atomic.").unwrap_or(service);
    SANDBOX_SCOPED_METHODS
        .iter()
        .any(|(s, m)| *s == service && *m == method)
}

/// What a sandbox token grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxGrant {
    /// The repository the grant was issued for ([`RepoHandle::repository_id`]).
    pub repository_id: Vec<u8>,
    /// The one view it reaches.
    pub view: String,
    /// The repository's internal id for `view` at issue. When set, a view
    /// deleted and recreated under the same name is not this grant's.
    pub view_id: Option<u64>,
    /// The identity the sandbox's work is attributed to (an agent's DID).
    pub acting_as: Option<String>,
    /// A nonempty subset of [`DELEGABLE_CAPABILITIES`].
    pub capabilities: BTreeSet<String>,
    /// Server-clock expiry.
    pub expires: DateTime<Utc>,
}

impl SandboxGrant {
    pub fn allows(&self, capability: &str) -> bool {
        self.capabilities.contains(capability)
    }

    pub fn is_live_at(&self, now: DateTime<Utc>) -> bool {
        self.expires > now
    }
}

/// A request to mint a grant ([`SandboxGrants::open`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRequest {
    pub repository_id: Vec<u8>,
    pub view: String,
    pub view_id: Option<u64>,
    pub acting_as: Option<String>,
    pub capabilities: BTreeSet<String>,
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    /// No live grant has this token (never issued, replaced or revoked).
    Unknown,
    Expired(DateTime<Utc>),
    NoTokenForView(String),
    InvalidTtl,
    InvalidCapabilities(String),
    /// The host's grant store failed.
    Store(String),
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "unknown or revoked sandbox token"),
            Self::Expired(at) => write!(f, "sandbox token expired at {}", at.to_rfc3339()),
            Self::NoTokenForView(view) => write!(f, "no live sandbox token for view '{view}'"),
            Self::InvalidTtl => write!(
                f,
                "a sandbox token must last between {MIN_TTL_SECS} and {MAX_TTL_SECS} seconds"
            ),
            Self::InvalidCapabilities(why) => write!(f, "invalid sandbox capabilities: {why}"),
            Self::Store(why) => write!(f, "sandbox grant store: {why}"),
        }
    }
}

impl std::error::Error for GrantError {}

impl GrantError {
    /// The contract's error for this refusal.
    pub fn status(&self) -> Status {
        let code = match self {
            Self::Unknown | Self::Expired(_) => ErrorCode::Unauthorized,
            Self::NoTokenForView(_) => ErrorCode::SandboxToken,
            Self::InvalidTtl | Self::InvalidCapabilities(_) => ErrorCode::InvalidArgument,
            Self::Store(_) => ErrorCode::Internal,
        };
        domain_status(code, self.to_string())
    }
}

/// The deadline a TTL of `ttl_secs` from `now` means, or
/// [`GrantError::InvalidTtl`]. Checked arithmetic throughout: the TTL comes
/// off the wire, and a request this refuses must be an error, never a panic
/// inside a store's lock.
pub fn grant_deadline(now: DateTime<Utc>, ttl_secs: u64) -> Result<DateTime<Utc>, GrantError> {
    if !(MIN_TTL_SECS..=MAX_TTL_SECS).contains(&ttl_secs) {
        return Err(GrantError::InvalidTtl);
    }
    let secs = i64::try_from(ttl_secs).map_err(|_| GrantError::InvalidTtl)?;
    let ttl = Duration::try_seconds(secs).ok_or(GrantError::InvalidTtl)?;
    now.checked_add_signed(ttl).ok_or(GrantError::InvalidTtl)
}

/// A requested capability list as a grant's set: nonempty, and every entry
/// delegable.
pub fn grant_capabilities(requested: &[String]) -> Result<BTreeSet<String>, GrantError> {
    if requested.is_empty() {
        return Err(GrantError::InvalidCapabilities(
            "name the capabilities the sandbox needs".to_string(),
        ));
    }
    let mut out = BTreeSet::new();
    for capability in requested {
        if !DELEGABLE_CAPABILITIES.contains(&capability.as_str()) {
            return Err(GrantError::InvalidCapabilities(format!(
                "'{capability}' cannot be delegated to a sandbox"
            )));
        }
        out.insert(capability.clone());
    }
    Ok(out)
}

/// Where sandbox grants live. The handlers only ever reach grants through
/// this trait; a host that persists grants (its own database) implements it
/// and installs it with [`DaemonState::with_sandbox_grants`].
///
/// Obligations of an implementation, all of which the in-memory default
/// meets: a view has at most one live token per repository (`open` replaces
/// it); `renew` changes only the expiry; `close` revokes; `resolve` refuses a
/// replaced, revoked or expired token; tokens are not stored in the clear.
/// Session and provenance-turn ownership is per (repository, view), so a
/// reissued token for the same view keeps its sessions and a token for
/// another view never gains them.
pub trait SandboxGrants: Send + Sync {
    /// Mint a token for `request.view`, replacing any live one. Returns the
    /// token (the only time it is ever available) and its grant.
    fn open(&self, request: GrantRequest) -> Result<(Vec<u8>, SandboxGrant), GrantError>;

    /// Extend the view's live token to now + `ttl_secs`; the token itself is
    /// unchanged.
    fn renew(
        &self,
        repository_id: &[u8],
        view: &str,
        ttl_secs: u64,
    ) -> Result<SandboxGrant, GrantError>;

    /// Revoke the view's token. Whether there was one.
    fn close(&self, repository_id: &[u8], view: &str) -> Result<bool, GrantError>;

    /// What `token` grants, if it is live.
    fn resolve(&self, token: &[u8]) -> Result<SandboxGrant, GrantError>;

    /// Whether `grant`'s sandbox may use provenance session `session_id`:
    /// one its view started, or — when `unused` (nothing has ever been
    /// recorded for it) — one nobody has, which becomes its own.
    fn claim_session(
        &self,
        grant: &SandboxGrant,
        session_id: &str,
        unused: bool,
    ) -> Result<bool, GrantError>;

    fn owns_session(&self, grant: &SandboxGrant, session_id: &str) -> Result<bool, GrantError>;

    /// Record that `grant`'s view reserved provenance turn `provenance_id`.
    fn bind_provenance(&self, grant: &SandboxGrant, provenance_id: u64) -> Result<(), GrantError>;

    fn owns_provenance(&self, grant: &SandboxGrant, provenance_id: u64)
        -> Result<bool, GrantError>;
}

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Who owns a session or a turn: a repository's view.
type Owner = (Vec<u8>, String);

#[derive(Default)]
struct Registry {
    /// blake3(token) → grant. A view has at most one live token.
    grants: HashMap<[u8; 32], SandboxGrant>,
    sessions: HashMap<(Vec<u8>, String), Owner>,
    provenance: HashMap<(Vec<u8>, u64), Owner>,
}

/// The default [`SandboxGrants`]: in memory, token hashes only, gone when
/// the process ends (every token then fails closed and the local admin
/// reissues). Cheap to clone; clones share the registry.
#[derive(Clone)]
pub struct InMemorySandboxGrants {
    inner: Arc<Mutex<Registry>>,
    clock: Clock,
}

impl Default for InMemorySandboxGrants {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for InMemorySandboxGrants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemorySandboxGrants")
            .finish_non_exhaustive()
    }
}

fn digest(token: &[u8]) -> [u8; 32] {
    *blake3::hash(token).as_bytes()
}

fn owner(grant: &SandboxGrant) -> Owner {
    (grant.repository_id.clone(), grant.view.clone())
}

impl InMemorySandboxGrants {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(Utc::now))
    }

    /// A registry reading time from `clock` instead of the system clock
    /// (tests; a host with its own trusted clock).
    pub fn with_clock(clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>) -> Self {
        Self {
            inner: Arc::default(),
            clock,
        }
    }

    /// The registry's lock. A panic while it was held poisons the mutex; the
    /// registry's invariants do not depend on unwinding, so a poisoned lock
    /// is recovered rather than turning one bad request into every later
    /// request panicking.
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl SandboxGrants for InMemorySandboxGrants {
    fn open(&self, request: GrantRequest) -> Result<(Vec<u8>, SandboxGrant), GrantError> {
        // Validated before the lock is taken: a bad request is an error,
        // never a panic inside it.
        let expires = grant_deadline((self.clock)(), request.ttl_secs)?;
        if request.capabilities.is_empty() {
            return Err(GrantError::InvalidCapabilities(
                "name the capabilities the sandbox needs".to_string(),
            ));
        }
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token = format!("ast_{}", data_encoding::BASE64URL_NOPAD.encode(&bytes)).into_bytes();
        let grant = SandboxGrant {
            repository_id: request.repository_id,
            view: request.view,
            view_id: request.view_id,
            acting_as: request.acting_as,
            capabilities: request.capabilities,
            expires,
        };
        let mut registry = self.lock();
        registry
            .grants
            .retain(|_, g| g.repository_id != grant.repository_id || g.view != grant.view);
        registry.grants.insert(digest(&token), grant.clone());
        Ok((token, grant))
    }

    fn renew(
        &self,
        repository_id: &[u8],
        view: &str,
        ttl_secs: u64,
    ) -> Result<SandboxGrant, GrantError> {
        let now = (self.clock)();
        let expires = grant_deadline(now, ttl_secs)?;
        let mut registry = self.lock();
        let grant = registry
            .grants
            .values_mut()
            .find(|g| g.repository_id == repository_id && g.view == view && g.is_live_at(now))
            .ok_or_else(|| GrantError::NoTokenForView(view.to_string()))?;
        grant.expires = expires;
        Ok(grant.clone())
    }

    fn close(&self, repository_id: &[u8], view: &str) -> Result<bool, GrantError> {
        let mut registry = self.lock();
        let before = registry.grants.len();
        registry
            .grants
            .retain(|_, g| g.repository_id != repository_id || g.view != view);
        Ok(registry.grants.len() != before)
    }

    fn resolve(&self, token: &[u8]) -> Result<SandboxGrant, GrantError> {
        let now = (self.clock)();
        let registry = self.lock();
        let grant = registry
            .grants
            .get(&digest(token))
            .ok_or(GrantError::Unknown)?;
        if !grant.is_live_at(now) {
            return Err(GrantError::Expired(grant.expires));
        }
        Ok(grant.clone())
    }

    fn claim_session(
        &self,
        grant: &SandboxGrant,
        session_id: &str,
        unused: bool,
    ) -> Result<bool, GrantError> {
        let mut registry = self.lock();
        let key = (grant.repository_id.clone(), session_id.to_string());
        Ok(match registry.sessions.get(&key) {
            Some(existing) => *existing == owner(grant),
            None if unused => {
                registry.sessions.insert(key, owner(grant));
                true
            }
            None => false,
        })
    }

    fn owns_session(&self, grant: &SandboxGrant, session_id: &str) -> Result<bool, GrantError> {
        let key = (grant.repository_id.clone(), session_id.to_string());
        Ok(self.lock().sessions.get(&key) == Some(&owner(grant)))
    }

    fn bind_provenance(&self, grant: &SandboxGrant, provenance_id: u64) -> Result<(), GrantError> {
        self.lock()
            .provenance
            .insert((grant.repository_id.clone(), provenance_id), owner(grant));
        Ok(())
    }

    fn owns_provenance(
        &self,
        grant: &SandboxGrant,
        provenance_id: u64,
    ) -> Result<bool, GrantError> {
        let key = (grant.repository_id.clone(), provenance_id);
        Ok(self.lock().provenance.get(&key) == Some(&owner(grant)))
    }
}

/// A request's sandbox principal: its grant, and the token it came with
/// (absent when the host attached the grant itself).
#[derive(Debug, Clone)]
pub struct SandboxCaller {
    pub grant: SandboxGrant,
    token: Option<Vec<u8>>,
}

impl SandboxCaller {
    pub fn view(&self) -> &str {
        &self.grant.view
    }

    /// Refuse unless the grant carries `capability`.
    pub fn require(&self, capability: &str) -> Result<(), Status> {
        if self.grant.allows(capability) {
            return Ok(());
        }
        Err(domain_status(
            ErrorCode::Forbidden,
            format!("this sandbox's grant does not include {capability}"),
        ))
    }

    /// Refuse unless the request is for the grant's repository (and names no
    /// host workspace) and `named` — the view the request names, if any — is
    /// the grant's.
    pub fn require_target(
        &self,
        handle: &RepoHandle,
        repository: &RepositoryRef,
        named: Option<&ViewRef>,
    ) -> Result<(), Status> {
        if repository.workspace_id.is_some() {
            return Err(domain_status(
                ErrorCode::Forbidden,
                "a sandbox request may not name a host workspace",
            ));
        }
        if handle.repository_id() != self.grant.repository_id {
            return Err(domain_status(
                ErrorCode::Forbidden,
                "this sandbox's grant is for another repository",
            ));
        }
        if let Some(named) = named {
            let by_name = named.name.as_deref().is_some_and(|n| n != self.grant.view);
            let by_id = !named.view_id.is_empty() && named.view_id != view_ref_id(&self.grant.view);
            if by_name || by_id {
                return Err(domain_status(
                    ErrorCode::Forbidden,
                    format!(
                        "that view is not this sandbox's (view '{}')",
                        self.grant.view
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Check the grant again — after time spent waiting for the repository,
    /// it may have expired or been revoked.
    pub fn revalidate(&self, state: &DaemonState) -> Result<(), Status> {
        match &self.token {
            Some(token) => state
                .sandbox_grants()
                .resolve(token)
                .map(|_| ())
                .map_err(|e| e.status()),
            None if self.grant.is_live_at(Utc::now()) => Ok(()),
            None => Err(GrantError::Expired(self.grant.expires).status()),
        }
    }
}

/// The `ViewRef.view_id` the handlers issue for a view name.
pub fn view_ref_id(name: &str) -> Vec<u8> {
    blake3::hash(name.as_bytes()).as_bytes().to_vec()
}

impl DaemonState {
    /// The sandbox principal a request acts under, or `None` for a local,
    /// trusted caller.
    ///
    /// A [`SandboxGrant`] the host attached to the request's extensions wins
    /// (the host verified the caller; its expiry is still checked). Otherwise
    /// a token in [`SANDBOX_TOKEN_METADATA`] is resolved through
    /// [`DaemonState::sandbox_grants`]. More than one token, or a token beside
    /// an `authorization` credential, is refused: a restricted token is never
    /// read as some other identity.
    pub fn sandbox_caller<T>(&self, request: &Request<T>) -> Result<Option<SandboxCaller>, Status> {
        if let Some(grant) = request.extensions().get::<SandboxGrant>() {
            if !grant.is_live_at(Utc::now()) {
                return Err(GrantError::Expired(grant.expires).status());
            }
            return Ok(Some(SandboxCaller {
                grant: grant.clone(),
                token: None,
            }));
        }
        let metadata = request.metadata();
        let mut tokens = metadata.get_all_bin(SANDBOX_TOKEN_METADATA).iter();
        let Some(token) = tokens.next() else {
            return Ok(None);
        };
        if tokens.next().is_some() || metadata.contains_key("authorization") {
            return Err(domain_status(
                ErrorCode::Unauthorized,
                "a request carries one credential",
            ));
        }
        let token = token.to_bytes().map_err(|_| {
            domain_status(ErrorCode::Unauthorized, "malformed sandbox token metadata")
        })?;
        let grant = self
            .sandbox_grants()
            .resolve(&token)
            .map_err(|e| e.status())?;
        Ok(Some(SandboxCaller {
            grant,
            token: Some(token.to_vec()),
        }))
    }

    /// Refuse a sandbox principal: for local-only methods.
    pub fn local_only<T>(&self, request: &Request<T>, method: &str) -> Result<(), Status> {
        let has_token = request.metadata().get_bin(SANDBOX_TOKEN_METADATA).is_some();
        if request.extensions().get::<SandboxGrant>().is_some() || has_token {
            return Err(domain_status(
                ErrorCode::Forbidden,
                format!("only a local caller may call {method}"),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 60 * 60;

    fn request(view: &str, ttl_secs: u64) -> GrantRequest {
        GrantRequest {
            repository_id: b"repo".to_vec(),
            view: view.to_string(),
            view_id: None,
            acting_as: None,
            capabilities: ["sandbox.read".to_string()].into(),
            ttl_secs,
        }
    }

    fn settable_clock() -> (Arc<Mutex<DateTime<Utc>>>, InMemorySandboxGrants) {
        let now = Arc::new(Mutex::new(Utc::now()));
        let read = Arc::clone(&now);
        let grants = InMemorySandboxGrants::with_clock(Arc::new(move || *read.lock().unwrap()));
        (now, grants)
    }

    #[test]
    fn a_token_reaches_its_view_until_it_ends() {
        let (now, reg) = settable_clock();
        let (t, g) = reg.open(request("exp-1", 24 * HOUR)).unwrap();
        assert!(t.starts_with(b"ast_"));
        assert_eq!(reg.resolve(&t).unwrap().view, "exp-1");
        assert_eq!(reg.resolve(b"ast_made_up"), Err(GrantError::Unknown));

        *now.lock().unwrap() += Duration::seconds(5);
        let renewed = reg.renew(b"repo", "exp-1", 24 * HOUR).unwrap();
        assert!(renewed.expires > g.expires);
        assert_eq!(reg.resolve(&t).unwrap().expires, renewed.expires);

        assert!(reg.close(b"repo", "exp-1").unwrap());
        assert_eq!(reg.resolve(&t), Err(GrantError::Unknown));
        assert!(reg.renew(b"repo", "exp-1", HOUR).is_err());
        assert!(!reg.close(b"repo", "exp-1").unwrap());
    }

    #[test]
    fn expired_and_replaced_tokens_are_refused() {
        let (now, reg) = settable_clock();
        let (t, _) = reg.open(request("exp-1", 60)).unwrap();
        *now.lock().unwrap() += Duration::seconds(61);
        assert!(matches!(reg.resolve(&t), Err(GrantError::Expired(_))));
        assert!(reg.renew(b"repo", "exp-1", HOUR).is_err());

        let (old, _) = reg.open(request("exp-2", HOUR)).unwrap();
        let (new, _) = reg.open(request("exp-2", HOUR)).unwrap();
        assert_eq!(reg.resolve(&old), Err(GrantError::Unknown));
        assert!(reg.resolve(&new).is_ok());

        // The same view name in another repository is another grant.
        let mut elsewhere = request("exp-2", HOUR);
        elsewhere.repository_id = b"other".to_vec();
        let (other, _) = reg.open(elsewhere).unwrap();
        assert!(reg.resolve(&new).is_ok());
        assert!(reg.resolve(&other).is_ok());
    }

    /// A TTL comes off the wire; an absurd one is an error, never a panic
    /// inside the registry's lock, and the registry keeps working.
    #[test]
    fn an_absurd_lifetime_is_an_error_and_not_a_panic() {
        let reg = InMemorySandboxGrants::new();
        let (t, _) = reg.open(request("exp-1", HOUR)).unwrap();
        for secs in [
            0,
            MAX_TTL_SECS + 1,
            u64::MAX,
            i64::MAX as u64,
            u64::MAX / 1_000,
        ] {
            assert_eq!(
                reg.renew(b"repo", "exp-1", secs),
                Err(GrantError::InvalidTtl),
                "ttl {secs}"
            );
            assert_eq!(
                reg.open(request("exp-2", secs)).err(),
                Some(GrantError::InvalidTtl),
                "ttl {secs}"
            );
        }
        assert!(reg.open(request("low", MIN_TTL_SECS)).is_ok());
        assert!(reg.open(request("high", MAX_TTL_SECS)).is_ok());
        assert_eq!(reg.resolve(&t).unwrap().view, "exp-1");
    }

    #[test]
    fn a_poisoned_lock_does_not_take_the_registry_down() {
        let reg = InMemorySandboxGrants::new();
        let (t, _) = reg.open(request("exp-1", HOUR)).unwrap();
        let poisoner = reg.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lock();
            panic!("something unrelated blew up mid-request");
        })
        .join();
        assert_eq!(reg.resolve(&t).unwrap().view, "exp-1");
        assert!(reg.open(request("exp-2", HOUR)).is_ok());
        assert!(reg.close(b"repo", "exp-1").unwrap());
    }

    #[test]
    fn a_session_belongs_to_the_view_that_started_it() {
        let reg = InMemorySandboxGrants::new();
        let (_, one) = reg.open(request("exp-1", HOUR)).unwrap();
        let (_, two) = reg.open(request("exp-2", HOUR)).unwrap();
        assert!(reg.claim_session(&one, "s1", true).unwrap());
        assert!(
            reg.claim_session(&one, "s1", false).unwrap(),
            "its own, again"
        );
        assert!(
            !reg.claim_session(&two, "s1", true).unwrap(),
            "another view's"
        );
        assert!(
            !reg.claim_session(&two, "s-local", false).unwrap(),
            "an existing session it didn't start"
        );
        assert!(reg.owns_session(&one, "s1").unwrap());
        assert!(!reg.owns_session(&two, "s1").unwrap());

        // A reissued token for the same view keeps its sessions.
        let (_, reissued) = reg.open(request("exp-1", HOUR)).unwrap();
        assert!(reg.owns_session(&reissued, "s1").unwrap());

        reg.bind_provenance(&one, 7).unwrap();
        assert!(reg.owns_provenance(&one, 7).unwrap());
        assert!(!reg.owns_provenance(&two, 7).unwrap());
        assert!(!reg.owns_provenance(&one, 8).unwrap());
    }

    #[test]
    fn only_scoped_methods_reach_a_sandbox() {
        assert!(is_sandbox_scoped("/atomic.SandboxService/SubmitChange"));
        assert!(is_sandbox_scoped("/atomic.ProvenanceService/ReserveTurn"));
        assert!(!is_sandbox_scoped("/atomic.SandboxService/OpenSandbox"));
        assert!(!is_sandbox_scoped(
            "/atomic.RepositoryMutationService/Record"
        ));
        assert!(!is_sandbox_scoped("/atomic.VaultService/CreateVaultEntity"));
        assert!(!is_sandbox_scoped("garbage"));
        for scoped in SANDBOX_SCOPED_METHODS {
            assert!(
                SANDBOX_METHODS.contains(scoped),
                "{scoped:?} is in the allowlist"
            );
        }
    }

    #[test]
    fn only_delegable_capabilities_are_granted() {
        assert!(grant_capabilities(&[]).is_err());
        assert!(grant_capabilities(&["repository.write".into()]).is_err());
        assert!(grant_capabilities(&["sandbox.admin".into()]).is_err());
        assert_eq!(
            grant_capabilities(&["sandbox.read".into(), "sandbox.submit".into()])
                .unwrap()
                .len(),
            2
        );
    }
}

//! The owner's remote transport: the same protocol over iroh, for sandboxes
//! on another machine.
//!
//! The local socket trusts its caller (the filesystem guards it). A remote
//! caller is trusted only as far as its sandbox token: every frame carries
//! it, and it reaches exactly one view, optionally as one identity, until it
//! expires. Tokens live **in memory only** — they end when the owner exits —
//! and only their hashes are kept.
//!
//! Provenance a remote sandbox records is bound to its token the same way: a
//! session it starts is its own, and it can touch no other.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context};
use chrono::{DateTime, Duration, Utc};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, SecretKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// ALPN for the owner protocol over iroh.
pub(crate) const ALPN: &[u8] = b"atomic-owner/1";

/// The owner's iroh key, kept beside its lock so its address survives a
/// restart and existing pointers keep working.
const IROH_KEY_FILE: &str = "owner-iroh.key";

/// What a sandbox token grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grant {
    pub(crate) view: String,
    /// The identity the sandbox's work is attributed to (e.g. an agent's).
    pub(crate) acting_as: Option<String>,
    pub(crate) expires: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TokenError {
    #[error("unknown or revoked sandbox token")]
    Unknown,
    #[error("sandbox token expired at {0}")]
    Expired(DateTime<Utc>),
    #[error("no live token for view '{0}'")]
    NoTokenForView(String),
    #[error("a sandbox token must last between 1 second and {MAX_TTL_SECS} seconds")]
    InvalidTtl,
}

/// Bounds on a requested token lifetime, in seconds. Both ends matter: a
/// non-positive TTL would mint a token that is already dead, and an enormous
/// one is either a mistake or a way to make a token that outlives the owner.
/// The lifetime is validated and turned into a deadline *before* the registry
/// lock is taken, so a bad value can never be the thing that panics inside it.
const MIN_TTL_SECS: i64 = 1;
const MAX_TTL_SECS: i64 = 365 * 24 * 60 * 60;

/// The deadline a TTL of `secs` seconds from now means, or [`TokenError::InvalidTtl`].
///
/// `Duration::seconds` panics outside its range and `DateTime + Duration`
/// panics on overflow, and both are reachable from a flag and then a wire
/// frame. Everything here is checked arithmetic instead, so a request this
/// rejects is a request that returns an error.
fn deadline(secs: i64) -> Result<DateTime<Utc>, TokenError> {
    if !(MIN_TTL_SECS..=MAX_TTL_SECS).contains(&secs) {
        return Err(TokenError::InvalidTtl);
    }
    let ttl = Duration::try_seconds(secs).ok_or(TokenError::InvalidTtl)?;
    Utc::now()
        .checked_add_signed(ttl)
        .ok_or(TokenError::InvalidTtl)
}

#[derive(Default)]
struct Tokens {
    /// Token digest → grant. A view has at most one live token.
    grants: HashMap<String, Grant>,
    /// Provenance sessions and turns each view's sandbox started.
    sessions: HashMap<String, String>,
    provenance: HashMap<u64, String>,
}

/// In-memory, view-scoped sandbox tokens. Cheap to clone (shared).
#[derive(Clone, Default)]
pub(crate) struct TokenRegistry {
    inner: Arc<Mutex<Tokens>>,
}

impl TokenRegistry {
    /// The registry's lock.
    ///
    /// A panic while holding it poisons the mutex, and every method would then
    /// panic on entry — one bad request taking down every sandbox plus the
    /// local `create`/`close` until the owner restarts. The registry's
    /// invariants do not depend on unwinding, so a poisoned lock is recovered
    /// rather than propagated.
    fn lock(&self) -> std::sync::MutexGuard<'_, Tokens> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn digest(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

impl TokenRegistry {
    /// Mint a token for `view`, valid for `ttl_secs` seconds. Minting again
    /// replaces the view's token.
    ///
    /// The lifetime is turned into a deadline before the lock is taken, so a
    /// bad `--ttl` is an error rather than a panic inside the registry.
    pub(crate) fn mint(
        &self,
        view: &str,
        acting_as: Option<String>,
        ttl_secs: i64,
    ) -> Result<(String, Grant), TokenError> {
        let expires = deadline(ttl_secs)?;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token = format!("ast_{}", data_encoding::BASE64URL_NOPAD.encode(&bytes));
        let grant = Grant {
            view: view.to_string(),
            acting_as,
            expires,
        };
        let mut t = self.lock();
        t.grants.retain(|_, g| g.view != view);
        t.grants.insert(digest(&token), grant.clone());
        Ok((token, grant))
    }

    /// Extend `view`'s live token to now + `ttl_secs` seconds; the same token
    /// keeps working.
    pub(crate) fn renew(&self, view: &str, ttl_secs: i64) -> Result<DateTime<Utc>, TokenError> {
        let expires = deadline(ttl_secs)?;
        let mut t = self.lock();
        let now = Utc::now();
        let grant = t
            .grants
            .values_mut()
            .find(|g| g.view == view && g.expires > now)
            .ok_or_else(|| TokenError::NoTokenForView(view.to_string()))?;
        grant.expires = expires;
        Ok(grant.expires)
    }

    /// End `view`'s token now.
    pub(crate) fn revoke(&self, view: &str) -> bool {
        let mut t = self.lock();
        let before = t.grants.len();
        t.grants.retain(|_, g| g.view != view);
        t.grants.len() != before
    }

    /// What `token` grants, if it is live.
    pub(crate) fn check(&self, token: &str) -> Result<Grant, TokenError> {
        let t = self.lock();
        let grant = t.grants.get(&digest(token)).ok_or(TokenError::Unknown)?;
        if grant.expires <= Utc::now() {
            return Err(TokenError::Expired(grant.expires));
        }
        Ok(grant.clone())
    }

    /// Whether `view`'s sandbox may use provenance session `session_id`:
    /// one it started, or one nobody has (`fresh`), which becomes its own.
    pub(crate) fn claim_session(&self, view: &str, session_id: &str, fresh: bool) -> bool {
        let mut t = self.lock();
        match t.sessions.get(session_id) {
            Some(owner) => owner == view,
            None if fresh => {
                t.sessions.insert(session_id.to_string(), view.to_string());
                true
            }
            None => false,
        }
    }

    pub(crate) fn owns_session(&self, view: &str, session_id: &str) -> bool {
        self.lock().sessions.get(session_id).map(String::as_str) == Some(view)
    }

    pub(crate) fn bind_provenance(&self, view: &str, provenance_id: u64) {
        self.lock()
            .provenance
            .insert(provenance_id, view.to_string());
    }

    pub(crate) fn owns_provenance(&self, view: &str, provenance_id: u64) -> bool {
        self.lock()
            .provenance
            .get(&provenance_id)
            .map(String::as_str)
            == Some(view)
    }
}

/// A remote sandbox's `.atomic-sandbox`: where its repository's owner is,
/// the view it works on, and its token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RemotePointer {
    pub(crate) remote: EndpointAddr,
    pub(crate) view: String,
    pub(crate) token: String,
}

/// Walk up from `start` to the nearest `.atomic-sandbox`; if it names a
/// remote owner, return the sandbox root and the pointer.
pub(crate) fn find_remote_pointer(start: &Path) -> Option<(std::path::PathBuf, RemotePointer)> {
    let mut dir = std::path::absolute(start).ok()?;
    loop {
        let path = dir.join(atomic_repository::SANDBOX_POINTER);
        if path.is_file() {
            let bytes = std::fs::read(&path).ok()?;
            return serde_json::from_slice(&bytes).ok().map(|p| (dir, p));
        }
        if dir.join(".atomic").is_dir() || !dir.pop() {
            return None;
        }
    }
}

/// The owner's iroh endpoint, with a key persisted in `dot_dir`.
pub(crate) async fn bind(dot_dir: &Path, offline: bool) -> anyhow::Result<Endpoint> {
    let key_path = dot_dir.join(IROH_KEY_FILE);
    let key = match std::fs::read(&key_path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = bytes
                .try_into()
                .map_err(|_| anyhow!("{} is not a 32-byte key", key_path.display()))?;
            SecretKey::from_bytes(&bytes)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            write_private(&key_path, &key.to_bytes())?;
            key
        }
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", key_path.display())),
    };
    Ok(builder(offline)?
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?)
}

/// The address to put in a pointer: once online, what iroh knows (relay and
/// direct addresses); offline, the bound sockets — unspecified ones as
/// loopback, which is all an offline owner can promise.
pub(crate) async fn pointer_addr(endpoint: &Endpoint, offline: bool) -> EndpointAddr {
    if !offline {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), endpoint.online()).await;
    }
    let addr = endpoint.addr();
    if addr.ip_addrs().next().is_some() || !offline {
        return addr;
    }
    endpoint
        .bound_sockets()
        .into_iter()
        .fold(addr, |addr, mut socket| {
            if socket.ip().is_unspecified() {
                socket.set_ip(match socket {
                    std::net::SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                    std::net::SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
                });
            }
            addr.with_ip_addr(socket)
        })
}

/// A client endpoint for dialing an owner.
async fn dialer(offline: bool) -> anyhow::Result<Endpoint> {
    Ok(builder(offline)?.bind().await?)
}

/// An endpoint builder: the relay-less preset when offline, and IPv4 only
/// when `ATOMIC_IROH_IPV4_ONLY` is set — for hosts where creating an IPv6
/// socket misbehaves (it takes down the process under some microVM network
/// backends), so the endpoint never opens one.
fn builder(offline: bool) -> anyhow::Result<iroh::endpoint::Builder> {
    let builder = if offline {
        Endpoint::builder(presets::Minimal)
    } else {
        Endpoint::builder(presets::N0)
    };
    if !ipv4_only() {
        return Ok(builder);
    }
    builder
        .clear_ip_transports()
        .bind_addr("0.0.0.0:0")
        .map_err(|e| anyhow!("invalid bind address: {e}"))
}

fn ipv4_only() -> bool {
    std::env::var("ATOMIC_IROH_IPV4_ONLY").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// Whether owners and clients skip iroh's relays and address lookup, and
/// dial by the direct addresses a pointer carries (tests, air-gapped hosts).
pub(crate) fn offline() -> bool {
    std::env::var_os("ATOMIC_OWNER_IROH_OFFLINE").is_some()
}

/// One iroh bi-stream as `AsyncRead + AsyncWrite`: one request, as on the
/// local socket.
pub(crate) struct BiStream {
    send: SendStream,
    recv: RecvStream,
    _connection: Connection,
    /// A dialing client's own endpoint, which must outlive the stream.
    dialer: Option<Endpoint>,
}

impl BiStream {
    /// Dial the owner at `addr` and open one request stream.
    pub(crate) async fn dial(addr: EndpointAddr) -> anyhow::Result<Self> {
        let endpoint = dialer(offline()).await?;
        let id = addr.id;
        let connection = endpoint
            .connect(addr, ALPN)
            .await
            .with_context(|| format!("database owner {id} is not reachable"))?;
        let (send, recv) = connection.open_bi().await?;
        Ok(Self {
            send,
            recv,
            _connection: connection,
            dialer: Some(endpoint),
        })
    }

    /// Done with the request: close the dialing endpoint gracefully.
    pub(crate) async fn close(self) {
        if let Some(endpoint) = self.dialer {
            endpoint.close().await;
        }
    }

    pub(crate) async fn accept(connection: &Connection) -> Option<Self> {
        let (send, recv) = connection.accept_bi().await.ok()?;
        Some(Self {
            send,
            recv,
            _connection: connection.clone(),
            dialer: None,
        })
    }
}

impl tokio::io::AsyncRead for BiStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for BiStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.send)
            .poll_write(cx, buf)
            .map_err(std::io::Error::other)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.send)
            .poll_flush(cx)
            .map_err(std::io::Error::other)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.send)
            .poll_shutdown(cx)
            .map_err(std::io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 60 * 60;

    #[test]
    fn a_token_reaches_its_view_until_it_ends() {
        let reg = TokenRegistry::default();
        let (t, g) = reg
            .mint("exp-1", Some("did:key:zAgent".into()), 24 * HOUR)
            .unwrap();
        assert_eq!(reg.check(&t).unwrap().view, "exp-1");
        assert_eq!(reg.check("ast_made_up"), Err(TokenError::Unknown));

        std::thread::sleep(std::time::Duration::from_millis(5));
        let after = reg.renew("exp-1", 24 * HOUR).unwrap();
        assert!(after > g.expires);
        assert_eq!(reg.check(&t).unwrap().expires, after);

        assert!(reg.revoke("exp-1"));
        assert_eq!(reg.check(&t), Err(TokenError::Unknown));
        assert!(reg.renew("exp-1", HOUR).is_err());
    }

    #[test]
    fn expired_and_replaced_tokens_are_refused() {
        let reg = TokenRegistry::default();
        let (t, _) = reg.mint("exp-1", None, 60).unwrap();
        // Backdate it rather than minting one that is already dead: a
        // non-positive lifetime is refused now, which is the point below.
        reg.lock()
            .grants
            .values_mut()
            .for_each(|g| g.expires = Utc::now() - Duration::seconds(1));
        assert!(matches!(reg.check(&t), Err(TokenError::Expired(_))));
        assert!(reg.renew("exp-1", HOUR).is_err());

        let (old, _) = reg.mint("exp-2", None, HOUR).unwrap();
        let (new, _) = reg.mint("exp-2", None, HOUR).unwrap();
        assert_eq!(reg.check(&old), Err(TokenError::Unknown));
        assert!(reg.check(&new).is_ok());
    }

    /// `--ttl` reaches the owner as an `i64` off a wire frame.
    /// `Duration::seconds` panics outside its range and `now + Duration`
    /// panics on overflow, and both used to happen while holding the registry
    /// lock — so one absurd value poisoned the mutex and took every other
    /// sandbox and the local `create`/`close` down with it.
    #[test]
    fn an_absurd_lifetime_is_an_error_and_not_a_panic() {
        let reg = TokenRegistry::default();
        // Establish a live token first: the panic used to land in renew, on
        // the way to extending one.
        let (t, _) = reg.mint("exp-1", None, HOUR).unwrap();

        for secs in [
            0,
            -1,
            i64::MIN,
            MAX_TTL_SECS + 1,
            i64::MAX,
            // Below Duration::seconds' own i64-milliseconds ceiling, so this
            // one panicked inside chrono rather than at the addition.
            i64::MAX / 1_000,
        ] {
            assert_eq!(
                reg.renew("exp-1", secs),
                Err(TokenError::InvalidTtl),
                "--ttl {secs} should be refused"
            );
            assert_eq!(
                reg.mint("exp-2", None, secs).err(),
                Some(TokenError::InvalidTtl),
                "minting with --ttl {secs} should be refused"
            );
        }

        // The ends of the accepted range, and the registry still works.
        assert!(reg.mint("low", None, MIN_TTL_SECS).is_ok());
        assert!(reg.mint("high", None, MAX_TTL_SECS).is_ok());

        // Nothing above was applied, and the registry is intact.
        assert_eq!(reg.check(&t).unwrap().view, "exp-1");
        assert!(reg.revoke("exp-1"));
        assert!(reg.claim_session("exp-1", "s1", true));
    }

    /// A panic anywhere else in an owner request used to poison the registry,
    /// and because every method took the lock with `unwrap`, the next caller
    /// panicked too — permanently, for every sandbox and for the local
    /// `create`/`close`, until the owner was restarted.
    #[test]
    fn a_poisoned_lock_does_not_take_the_registry_down() {
        let reg = TokenRegistry::default();
        let (t, _) = reg.mint("exp-1", None, HOUR).unwrap();

        let poisoner = reg.clone();
        let _ = std::thread::spawn(move || {
            let _held = poisoner.lock();
            panic!("something unrelated blew up mid-request");
        })
        .join();

        // Still usable, and the token it already had still resolves.
        assert_eq!(reg.check(&t).unwrap().view, "exp-1");
        assert!(reg.mint("exp-2", None, HOUR).is_ok());
        assert!(reg.claim_session("exp-1", "s1", true));
        assert!(reg.revoke("exp-1"));
    }

    #[test]
    fn a_session_belongs_to_the_view_that_started_it() {
        let reg = TokenRegistry::default();
        assert!(reg.claim_session("exp-1", "s1", true));
        assert!(reg.claim_session("exp-1", "s1", false), "its own, again");
        assert!(!reg.claim_session("exp-2", "s1", true), "another view's");
        assert!(
            !reg.claim_session("exp-2", "s-local", false),
            "an existing session it didn't start"
        );
        assert!(reg.owns_session("exp-1", "s1"));
        assert!(!reg.owns_session("exp-2", "s1"));
    }
}

//! Shared authentication helpers for remote commands.
//!
//! Resolves the caller's identity from the remote URL (or an explicit
//! override), mints a short-lived self-signed EdDSA JWT for that identity,
//! and attaches it as the `Authorization: Bearer` header.
//!
//! A raw public key is NOT a credential — the JWT (signed with the identity's
//! private key) proves possession of that key. See [`crate::commands::token`]
//! for the minting mechanics.
//!
//! Identity resolution order:
//! 1. Explicit override — `identity_override` from the `--identity` flag.
//! 2. URL userinfo — `http://bob@alice.localhost:8080/...` → identity "bob"
//! 3. Configured server binding — a `[servers.*]`/`[server]` profile whose host
//!    is the longest suffix of the remote host supplies its `identity`. Identity
//!    follows the server you registered with (the binding `atomic identity
//!    register` writes), so any tenant under it resolves without `--identity`.
//!    When two profiles' hosts match equally well, the **active** profile
//!    (per `default_server`, else the legacy `[server]` block) wins.
//! 4. Subdomain — `http://alice.localhost:8080/...` → identity "alice" (legacy
//!    last resort).
//! 5. Default identity — if none of the above resolves to an identity that
//!    exists locally, the store's default identity is used. This makes the
//!    common single-identity case "just work": pushing to
//!    `https://aaron.atomic.storage/...` with a default identity named
//!    `aaron-claude` authenticates as `aaron-claude` without `--identity`.
//!    The default fallback does NOT apply when `--identity` explicitly names an
//!    identity that doesn't exist (that's a clear error, not a substitution).

use atomic_identity::{Identity, IdentityStore};
use atomic_remote::HttpRemoteConfig;
use url::Url;

use crate::error::{CliError, CliResult};

/// One `[server]`/`[servers.*]` profile that declares an identity: its host,
/// its bound identity, and whether it is the active profile (the one
/// `default_server` selects, or the legacy block when no name is set).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerBinding {
    host: String,
    identity: String,
    active: bool,
}

/// Resolve the identity name to authenticate as.
///
/// Priority:
/// 1. An explicit override (the `--identity` flag).
/// 2. URL userinfo (`http://bob@host/...`).
/// 3. A configured server profile whose host is the longest dot-boundary
///    suffix of the remote host (`[servers.prod] url=... identity=...`). This is
///    the binding that makes pushes to *any* tenant under a server you've
///    registered with ("just works" without `--identity`): identity follows the
///    server, not the tenant subdomain. Equal-length matches are won by the
///    active profile.
/// 4. The remote host's leading subdomain label (`http://bob.host/...`) — the
///    legacy heuristic, kept only as a last resort.
///
/// Returns `None` only when there is no override and nothing inferable from the
/// URL or config.
fn resolve_identity_name_with_override(
    remote_url: &str,
    identity_override: Option<&str>,
) -> Option<String> {
    if let Some(name) = identity_override {
        return Some(name.to_string());
    }
    resolve_identity_from_url(remote_url, &configured_server_identity_bindings())
}

/// Collect the server profiles from the global config — both the default
/// `[server]` block and every named `[servers.*]` profile — that declare an
/// identity, marking which one is **active** (the profile `default_server`
/// names, or the legacy block when unset). Servers without an identity binding
/// are skipped.
///
/// Returns an empty list when no config exists or it can't be read, so
/// resolution degrades cleanly to URL-based inference.
fn configured_server_identity_bindings() -> Vec<ServerBinding> {
    let config = match atomic_config::GlobalConfig::load() {
        Ok(c) => c,
        Err(e) => {
            log::debug!("Could not load global config for server identities: {e}");
            return Vec::new();
        }
    };

    let host_of = |server: &atomic_config::ServerConfig| {
        server
            .url
            .as_deref()
            .and_then(|u| Url::parse(u).ok())
            .and_then(|u| u.host_str().map(String::from))
    };

    let mut bindings = Vec::new();
    let mut consider = |server: &atomic_config::ServerConfig, active: bool| {
        if let (Some(host), Some(identity)) = (host_of(server), server.identity.as_ref()) {
            bindings.push(ServerBinding {
                host,
                identity: identity.clone(),
                active,
            });
        }
    };

    // The active profile is resolved exactly as management commands resolve
    // it (`GlobalConfig::resolve_server`): `default_server` → named profile,
    // else the legacy block. A dangling `default_server` name degrades to
    // no active marker rather than failing auth resolution.
    let active_named = config
        .default_server
        .as_deref()
        .and_then(|name| config.servers.get(name));
    match active_named {
        Some(profile) => {
            consider(profile, true);
            for (name, server) in &config.servers {
                if Some(name.as_str()) != config.default_server.as_deref() {
                    consider(server, false);
                }
            }
            consider(&config.server, false);
        }
        None => {
            for server in config.servers.values() {
                consider(server, false);
            }
            consider(&config.server, true);
        }
    }
    bindings
}

/// Pure identity resolution from a URL plus the configured server bindings.
///
/// userinfo → server-host match (longest suffix, active wins ties) → first-label
/// subdomain. Split from the config-loading wrapper so it can be unit-tested
/// without a config file on disk.
fn resolve_identity_from_url(remote_url: &str, servers: &[ServerBinding]) -> Option<String> {
    let url = Url::parse(remote_url).ok()?;

    // 1. Explicit in the URL: http://bob@host/...
    let username = url.username();
    if !username.is_empty() {
        return Some(username.to_string());
    }

    let host = url.host_str()?;

    // 2. Configured server binding: identity follows the server (apex), so any
    //    tenant under it resolves to the same identity.
    if let Some(identity) = match_server_identity(host, servers) {
        return Some(identity);
    }

    // 3. Legacy: treat the first DNS label as the identity name.
    extract_subdomain(host)
}

/// Pick the identity bound to the configured server whose host is the longest
/// dot-boundary suffix of `remote_host`. When several profiles' hosts match
/// with the same length (e.g. the legacy `[server]` block and a named profile
/// pointing at the same server), the **active** profile wins — the one
/// `default_server` selects, else the legacy block.
///
/// Longest-suffix still wins over active: a more specific server
/// (`staging.atomic.storage`) beats a broader active one (`atomic.storage`)
/// for hosts under both — e.g. `x.staging.atomic.storage` resolves to the
/// staging identity, not prod.
fn match_server_identity(remote_host: &str, servers: &[ServerBinding]) -> Option<String> {
    servers
        .iter()
        .filter(|b| host_is_under(remote_host, &b.host))
        .max_by_key(|b| (b.host.len(), b.active))
        .map(|b| b.identity.clone())
}

/// Whether `remote_host` is the server host itself or a subdomain of it,
/// matching only on dot boundaries (so `notatomic.storage` does NOT match
/// `atomic.storage`). Comparison is case-insensitive (DNS is).
fn host_is_under(remote_host: &str, server_host: &str) -> bool {
    let remote = remote_host.to_ascii_lowercase();
    let server = server_host.to_ascii_lowercase();
    remote == server || remote.ends_with(&format!(".{server}"))
}

/// Load a local identity by name, falling back to a case-insensitive match.
///
/// Tenant slugs are lowercase (e.g. `aaron`), but locally stored identities are
/// free-form and frequently mixed-case (e.g. `Aaron`). A subdomain-inferred
/// name would therefore never match the local identity on a strict comparison.
/// An exact match always wins; otherwise the first identity whose name matches
/// case-insensitively is returned.
fn load_identity_lenient(store: &IdentityStore, name: &str) -> Option<Identity> {
    if let Ok(identity) = store.load_by_name(name) {
        return Some(identity);
    }

    let wanted = name.to_lowercase();
    store
        .list()
        .ok()?
        .into_iter()
        .find(|identity| identity.name.to_lowercase() == wanted)
}

/// Resolve a concrete identity for a remote operation, falling back to the
/// default identity when the inferred name doesn't match any local identity.
///
/// This is the single place that bridges name resolution (URL subdomain,
/// server binding, `--identity` flag) and the on-disk identity store. The
/// fallback to the default identity makes the common case "just work": a user
/// with a single registered identity can push to any tenant under their server
/// without passing `--identity`, even when the subdomain label (`aaron`) does
/// not match the local identity name (`aaron-claude`).
///
/// Resolution:
/// 1. If `inferred_name` is `Some` and matches a local identity (lenient
///    case-insensitive match), use it.
/// 2. Otherwise, if `explicit` is false, fall back to the store's default
///    identity.
/// 3. If `explicit` is true (the name came from `--identity`), return `None`
///    so the caller reports a clear "identity not found" error — we never
///    silently substitute the default for an explicit flag.
/// 4. If neither the inferred name nor the default resolves, return `None`.
///
/// `inferred_source` labels the resolution path in debug logs (e.g.
/// `"subdomain 'aaron'"`, `"--identity 'aaron-claude'"`).
fn resolve_identity_with_default_fallback(
    store: &IdentityStore,
    inferred_name: Option<&str>,
    explicit: bool,
    inferred_source: &str,
) -> Option<Identity> {
    let resolved_name = decide_identity_with_default_fallback(
        inferred_name,
        explicit,
        |name| load_identity_lenient(store, name).is_some(),
        store.get_default().ok().flatten().map(|id| id.name.clone()),
    );

    match resolved_name.as_deref() {
        Some(name) if inferred_name == Some(name) => {
            log::debug!("Resolved identity '{name}' via {inferred_source}");
        }
        Some(name) => {
            log::debug!("Falling back to default identity '{name}'");
        }
        None => {
            if explicit {
                log::debug!(
                    "Explicit identity {:?} ({inferred_source}) not found; \
                     not falling back to default",
                    inferred_name
                );
            } else {
                log::debug!(
                    "No usable identity for {inferred_source} \
                     (inferred={inferred_name:?}) and no default set"
                );
            }
        }
    }

    resolved_name.and_then(|name| load_identity_lenient(store, &name))
}

/// Pure decision core for identity resolution with a default fallback.
///
/// Split from [`resolve_identity_with_default_fallback`] so the fallback logic
/// can be unit-tested without an on-disk identity store.
///
/// * `inferred_name` — the name resolved from the URL/`--identity` (if any).
/// * `explicit` — whether the name came from `--identity` (vs. URL inference).
/// * `inferred_exists` — whether an identity with the given name is local.
/// * `default_name` — the store's default identity name, if one is set.
///
/// Returns the identity name to authenticate as, or `None` when nothing usable
/// resolves (the caller should error).
fn decide_identity_with_default_fallback(
    inferred_name: Option<&str>,
    explicit: bool,
    inferred_exists: impl Fn(&str) -> bool,
    default_name: Option<String>,
) -> Option<String> {
    if let Some(name) = inferred_name {
        if inferred_exists(name) {
            return Some(name.to_string());
        }
        // Inferred name not found locally. If it came from --identity, that's
        // an explicit miss — error, don't silently substitute the default.
        if explicit {
            return None;
        }
    }
    // Fall back to the default identity (subdomain-inferred miss, or nothing
    // inferable at all). The subdomain is the org slug, not the identity name,
    // so a mismatch here is the expected, common case.
    default_name
}

/// Attach a Bearer JWT auth header to the remote config.
///
/// `identity_override` — if provided, this identity name is used directly,
/// bypassing URL-based inference. Set from `RemoteEntry.identity` or the
/// `--identity` CLI flag.
///
/// If the identity cannot be resolved (no override, no userinfo, no subdomain,
/// or identity not found in the store) or login fails, the config is returned
/// unmodified and a debug log is emitted. This keeps push/pull/clone working
/// against servers that don't require auth (e.g. public reads).
pub async fn attach_identity(
    config: HttpRemoteConfig,
    remote_url: &str,
    identity_override: Option<&str>,
) -> HttpRemoteConfig {
    if identity_override.is_some() {
        log::debug!("Using explicit identity override: {:?}", identity_override);
    }

    // Priority 1: explicit override (--identity); 2/3: URL userinfo or subdomain.
    let inferred = resolve_identity_name_with_override(remote_url, identity_override);
    let source = if identity_override.is_some() {
        format!("--identity {:?}", identity_override)
    } else {
        "URL/config inference".to_string()
    };

    let store = match IdentityStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            log::debug!("Failed to open identity store: {}", e);
            return config;
        }
    };

    // Resolve a concrete identity, falling back to the default when the
    // inferred name doesn't match any local identity (e.g. the `aaron`
    // subdomain vs. a local identity named `aaron-claude`). An explicit
    // --identity that doesn't exist does NOT fall back (returns None → no
    // auth header, so the server rejects it with a clear 401).
    let explicit = identity_override.is_some();
    let identity = match resolve_identity_with_default_fallback(
        &store,
        inferred.as_deref(),
        explicit,
        &source,
    ) {
        Some(id) => id,
        None => {
            log::debug!(
                "No usable identity for {} (inferred={:?}) and no default set",
                remote_url,
                inferred
            );
            return config;
        }
    };

    // Tokens are keyed to the apex server (where the identity registered), not
    // the tenant subdomain — strip the leading subdomain label from the host.
    let server = match apex_server_url(remote_url) {
        Some(s) => s,
        None => {
            log::debug!("Could not derive apex server URL from: {}", remote_url);
            return config;
        }
    };

    let identity_name = identity.name.clone();
    match crate::commands::token::get_token(&server, &identity).await {
        Ok(jwt) => {
            log::debug!("Attaching Bearer JWT for identity '{}'", identity_name);
            config.with_header("Authorization", format!("Bearer {}", jwt))
        }
        Err(e) => {
            // Non-fatal: a server that doesn't require auth still works for
            // public reads. Auth-required operations will then fail with a
            // clear 401 from the server.
            log::debug!("Could not obtain JWT for '{}': {}", identity_name, e);
            config
        }
    }
}

/// What went wrong (if anything) when checking for usable push credentials.
///
/// Authentication for atomic-storage is a *self-signed* EdDSA JWT: the CLI
/// mints a fresh, short-lived token per request by signing with the identity's
/// Ed25519 private key (see [`crate::commands::token`]). There is no stored
/// token to inspect for expiry — a token is only ever created at the moment of
/// use and is valid for its full TTL. So "having usable credentials" reduces to
/// three local conditions, each a distinct failure with its own remedy:
///
/// 1. an identity is resolvable (from the URL, `--identity`, or the default),
/// 2. that identity exists in the local store, and
/// 3. its keypair can be loaded (so a token can actually be signed).
#[derive(Debug, PartialEq, Eq)]
pub enum CredentialIssue {
    /// No identity could be resolved from the remote URL or `--identity`, and
    /// no default identity is set, so there is nothing to authenticate as.
    NoIdentity,
    /// `--identity` explicitly named an identity that doesn't exist locally.
    /// We do not silently fall back to the default for an explicit flag — the
    /// name itself is the user's input and must be pointed at.
    ExplicitIdentityNotFound { name: String },
    /// The resolved identity exists but its signing keypair could not be
    /// loaded, so no token can be minted.
    KeypairUnavailable { name: String },
}

impl CredentialIssue {
    /// Render an actionable, accurate error message for this issue.
    ///
    /// All messages point at registering an identity, since that is the single
    /// command that establishes a usable credential for a remote.
    fn message(&self) -> String {
        match self {
            CredentialIssue::NoIdentity => format!(
                "Not authenticated for {REMEDY_PREFIX}: could not determine an \
                 identity from the remote URL, no --identity was given, and no \
                 default identity is set. Pass --identity <NAME> (see \
                 `atomic identity list`) or set a default with \
                 `atomic identity new <name> --set-default`. {REMEDY_SUFFIX}"
            ),
            CredentialIssue::ExplicitIdentityNotFound { name } => format!(
                "Not authenticated for {REMEDY_PREFIX}: --identity '{name}' does \
                 not match any identity on this machine (see `atomic identity \
                 list`). {REMEDY_SUFFIX}"
            ),
            CredentialIssue::KeypairUnavailable { name } => format!(
                "Not authenticated for {REMEDY_PREFIX}: could not load the signing \
                 key for identity '{name}'. {REMEDY_SUFFIX}"
            ),
        }
    }
}

const REMEDY_PREFIX: &str = "this remote";
const REMEDY_SUFFIX: &str =
    "Register your identity with the server first: `atomic identity register <server-url>`.";

/// Verify, before a push begins, that the client can produce credentials for
/// `remote_url`. Fails fast with an actionable error instead of letting the
/// push proceed into a confusing 404 (which the server returns for private
/// projects the caller isn't authorized to see).
///
/// Returns `Ok(())` when an identity is resolvable (from `--identity`, the
/// URL, or the default identity), exists in the local store, and has a
/// loadable signing keypair.
///
/// `identity_override` is the `--identity` flag value; it takes priority over
/// any identity inferred from the URL, exactly as [`attach_identity`] resolves
/// it. The two must agree, or the fail-fast check here would reject a push that
/// `attach_identity` would have authenticated.
///
/// # Default-identity fallback
///
/// When the inferred identity name (from the subdomain or server binding) does
/// not match any local identity, or when no identity is inferable at all, the
/// default identity is used instead. This makes the common single-identity case
/// "just work" without `--identity`. The fallback does **not** apply when
/// `--identity` explicitly names an identity that doesn't exist — that is a
/// clear error, not a silent substitution.
pub fn check_push_credentials(remote_url: &str, identity_override: Option<&str>) -> CliResult<()> {
    let store = IdentityStore::open_default()
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}")))?;

    let inferred = resolve_identity_name_with_override(remote_url, identity_override);
    let explicit = identity_override.is_some();

    // Resolve a concrete identity, falling back to the default when the
    // inferred name doesn't match (and the name didn't come from --identity).
    let identity =
        resolve_identity_with_default_fallback(&store, inferred.as_deref(), explicit, "push");
    let resolved_name = identity.as_ref().map(|id| id.name.clone());

    let issue = evaluate_push_credentials(
        explicit,
        inferred.as_deref(),
        resolved_name.as_deref(),
        |name| {
            load_identity_lenient(&store, name)
                .map(|id| store.load_keypair(&id.id, None).is_ok())
                .unwrap_or(false)
        },
    );

    match issue {
        Some(issue) => Err(CliError::AuthenticationFailed {
            remote: format!("{remote_url}: {}", issue.message()),
        }),
        None => Ok(()),
    }
}

/// Pure decision core for [`check_push_credentials`], split out so it can be
/// unit-tested without touching the on-disk identity store.
///
/// * `explicit` — whether `--identity` was passed.
/// * `inferred` — the name inferred from the URL/flag (before fallback); used
///   only to name the missing identity in the error message.
/// * `resolved_name` — the identity name to use **after** the default fallback
///   (`None` when nothing usable resolved).
/// * `keypair_loadable` — whether that identity's signing keypair can be loaded.
///
/// Returns `None` when credentials are usable, or `Some(issue)` describing the
/// problem.
fn evaluate_push_credentials(
    explicit: bool,
    inferred: Option<&str>,
    resolved_name: Option<&str>,
    keypair_loadable: impl Fn(&str) -> bool,
) -> Option<CredentialIssue> {
    match resolved_name {
        Some(name) => {
            if !keypair_loadable(name) {
                return Some(CredentialIssue::KeypairUnavailable {
                    name: name.to_string(),
                });
            }
            None
        }
        None => {
            // No usable identity. If --identity was explicit, name the missing
            // identity so the user knows which input was wrong. Otherwise the
            // fallback to default also failed (no default) — report that.
            if explicit {
                Some(CredentialIssue::ExplicitIdentityNotFound {
                    name: inferred.unwrap_or_default().to_string(),
                })
            } else {
                Some(CredentialIssue::NoIdentity)
            }
        }
    }
}

/// Derive the apex server URL (scheme + host without the leading subdomain
/// label + port) from a tenant-scoped remote URL.
///
/// `http://alice.localhost:8080/workspaces/w/projects/p/code`
///   → `http://localhost:8080`
/// `https://alice.atomic.storage/...` → `https://atomic.storage`
/// `http://localhost:8080/...` (no subdomain) → `http://localhost:8080`
fn apex_server_url(remote_url: &str) -> Option<String> {
    let url = Url::parse(remote_url).ok()?;
    let scheme = url.scheme();
    let host = url.host_str()?;

    // Strip the first label if there is a subdomain; leave a bare host
    // (e.g. "localhost") untouched.
    let apex_host = match host.find('.') {
        Some(dot) => &host[dot + 1..],
        None => host,
    };

    match url.port() {
        Some(port) => Some(format!("{scheme}://{apex_host}:{port}")),
        None => Some(format!("{scheme}://{apex_host}")),
    }
}

/// Extract the identity name from a remote URL alone (no server config).
///
/// Tries URL userinfo first (`bob@host`), then the subdomain (`bob.host`).
/// This is the config-free path; [`resolve_identity_name_with_override`] layers
/// the `--identity` override and configured server bindings on top.
fn resolve_identity_name(remote_url: &str) -> Option<String> {
    resolve_identity_from_url(remote_url, &[])
}

/// Extract the first subdomain label from a host string.
///
/// `"alice.localhost"` → `Some("alice")`
/// `"alice.atomic.storage"` → `Some("alice")`
/// `"localhost"` → `None`
fn extract_subdomain(host: &str) -> Option<String> {
    // Strip port if somehow present (Url should have parsed it out, but be safe).
    let host = host.split(':').next().unwrap_or(host);

    let dot_pos = host.find('.')?;
    let subdomain = &host[..dot_pos];

    if subdomain.is_empty() {
        return None;
    }

    Some(subdomain.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apex_strips_subdomain_with_port() {
        assert_eq!(
            apex_server_url("http://alice.localhost:8080/workspaces/w/projects/p/code").as_deref(),
            Some("http://localhost:8080")
        );
    }

    #[test]
    fn apex_strips_subdomain_full_domain() {
        assert_eq!(
            apex_server_url("https://alice.atomic.storage/workspaces/w/projects/p/code").as_deref(),
            Some("https://atomic.storage")
        );
    }

    #[test]
    fn apex_bare_host_unchanged() {
        assert_eq!(
            apex_server_url("http://localhost:8080/workspaces/w/projects/p/code").as_deref(),
            Some("http://localhost:8080")
        );
    }

    #[test]
    fn apex_invalid_url_is_none() {
        assert_eq!(apex_server_url("not a url"), None);
    }

    #[test]
    fn userinfo_takes_priority() {
        let name =
            resolve_identity_name("http://bob@alice.localhost:8080/workspaces/w/projects/p/code");
        assert_eq!(name.as_deref(), Some("bob"));
    }

    #[test]
    fn subdomain_fallback() {
        let name =
            resolve_identity_name("http://alice.localhost:8080/workspaces/w/projects/p/code");
        assert_eq!(name.as_deref(), Some("alice"));
    }

    #[test]
    fn full_domain_subdomain() {
        let name =
            resolve_identity_name("https://alice.atomic.storage/workspaces/w/projects/p/code");
        assert_eq!(name.as_deref(), Some("alice"));
    }

    #[test]
    fn no_subdomain_returns_none() {
        let name = resolve_identity_name("http://localhost:8080/workspaces/w/projects/p/code");
        assert_eq!(name, None);
    }

    #[test]
    fn invalid_url_returns_none() {
        let name = resolve_identity_name("not a url");
        assert_eq!(name, None);
    }

    #[test]
    fn extract_subdomain_simple() {
        assert_eq!(
            extract_subdomain("alice.localhost"),
            Some("alice".to_string())
        );
    }

    #[test]
    fn extract_subdomain_nested() {
        assert_eq!(
            extract_subdomain("alice.atomic.storage"),
            Some("alice".to_string())
        );
    }

    #[test]
    fn extract_subdomain_bare() {
        assert_eq!(extract_subdomain("localhost"), None);
    }

    #[test]
    fn extract_subdomain_empty_label() {
        assert_eq!(extract_subdomain(".localhost"), None);
    }

    // -- pre-push credential check (evaluate_push_credentials) --

    #[test]
    fn creds_ok_when_identity_resolves_and_keypair_loads() {
        // A concrete identity resolved (after any fallback) with a loadable key.
        let issue = evaluate_push_credentials(false, Some("alice"), Some("alice"), |_| true);
        assert_eq!(issue, None);
    }

    #[test]
    fn creds_fail_when_no_identity_and_no_default() {
        // Nothing inferred, nothing resolved → NoIdentity.
        let issue = evaluate_push_credentials(false, None, None, |_| true);
        assert_eq!(issue, Some(CredentialIssue::NoIdentity));
    }

    #[test]
    fn creds_fail_when_explicit_identity_not_found() {
        // --identity foo where foo doesn't exist; explicit so no default fallback.
        let issue = evaluate_push_credentials(true, Some("foo"), None, |_| true);
        assert_eq!(
            issue,
            Some(CredentialIssue::ExplicitIdentityNotFound {
                name: "foo".to_string()
            })
        );
    }

    #[test]
    fn creds_fail_when_keypair_cannot_load() {
        // Identity resolved but its signing key can't be loaded — no token can
        // be minted. This stands in for an "expired"/unusable credential, which
        // in this self-signed-JWT model means "cannot sign right now".
        let issue = evaluate_push_credentials(false, Some("alice"), Some("alice"), |_| false);
        assert_eq!(
            issue,
            Some(CredentialIssue::KeypairUnavailable {
                name: "alice".to_string()
            })
        );
    }

    #[test]
    fn credential_issue_messages_are_actionable() {
        // Every issue must name the remedy so the failure is self-explanatory.
        for issue in [
            CredentialIssue::NoIdentity,
            CredentialIssue::ExplicitIdentityNotFound {
                name: "alice".to_string(),
            },
            CredentialIssue::KeypairUnavailable {
                name: "alice".to_string(),
            },
        ] {
            let msg = issue.message();
            assert!(msg.contains("Not authenticated"));
            assert!(msg.contains("atomic identity register"));
        }
    }

    // -- default-identity fallback (decide_identity_with_default_fallback) --

    #[test]
    fn fallback_uses_inferred_when_it_exists() {
        // The subdomain name matches a local identity — use it directly.
        let name = decide_identity_with_default_fallback(
            Some("aaron"),
            false,
            |n| n == "aaron",
            Some("aaron-claude".to_string()),
        );
        assert_eq!(name.as_deref(), Some("aaron"));
    }

    #[test]
    fn fallback_to_default_when_subdomain_name_not_local() {
        // The reported bug: pushing to aaron.atomic.storage infers "aaron", but
        // the only local identity is "aaron-claude". Fall back to the default.
        let name = decide_identity_with_default_fallback(
            Some("aaron"),
            false,
            |n| n == "aaron-claude", // "aaron" does NOT exist locally
            Some("aaron-claude".to_string()),
        );
        assert_eq!(name.as_deref(), Some("aaron-claude"));
    }

    #[test]
    fn fallback_to_default_when_nothing_inferred() {
        // A bare host (no subdomain) with no --identity → use the default.
        let name = decide_identity_with_default_fallback(
            None,
            false,
            |_| false,
            Some("aaron-claude".to_string()),
        );
        assert_eq!(name.as_deref(), Some("aaron-claude"));
    }

    #[test]
    fn fallback_returns_none_when_no_default() {
        // No inference and no default set → nothing to authenticate as.
        let name = decide_identity_with_default_fallback(None, false, |_| false, None);
        assert_eq!(name, None);
    }

    #[test]
    fn fallback_returns_none_when_subdomain_miss_and_no_default() {
        let name = decide_identity_with_default_fallback(Some("aaron"), false, |_| false, None);
        assert_eq!(name, None);
    }

    #[test]
    fn fallback_does_not_substitute_default_for_explicit_identity() {
        // --identity foo where foo doesn't exist must NOT silently use the
        // default — that's a clear error, not a substitution.
        let name = decide_identity_with_default_fallback(
            Some("foo"),
            true,      // explicit
            |_| false, // "foo" not in store
            Some("aaron-claude".to_string()),
        );
        assert_eq!(name, None);
    }

    #[test]
    fn fallback_uses_explicit_identity_when_it_exists() {
        // --identity aaron-claude where it exists → use it (explicit is fine
        // when the name actually matches).
        let name = decide_identity_with_default_fallback(
            Some("aaron-claude"),
            true,
            |_| true,
            Some("other".to_string()),
        );
        assert_eq!(name.as_deref(), Some("aaron-claude"));
    }

    // -- identity override resolution --

    #[test]
    fn override_takes_priority_over_subdomain() {
        // The whole point of --identity: it wins over the host's DNS label.
        let name = resolve_identity_name_with_override(
            "https://aaron.atomic.storage/workspaces/w/projects/p/code",
            Some("Aaron"),
        );
        assert_eq!(name.as_deref(), Some("Aaron"));
    }

    #[test]
    fn override_wins_even_on_apex_host() {
        // Apex host has no usable subdomain, but --identity still resolves.
        let name = resolve_identity_name_with_override(
            "https://atomic.storage/workspaces/w/projects/p/code",
            Some("Aaron"),
        );
        assert_eq!(name.as_deref(), Some("Aaron"));
    }

    #[test]
    fn no_override_falls_back_to_subdomain() {
        // Use the pure resolver with no configured servers — the wrapper loads
        // the real global config, which is not hermetic for a unit test.
        let name = resolve_identity_from_url(
            "https://aaron.atomic.storage/workspaces/w/projects/p/code",
            &[],
        );
        assert_eq!(name.as_deref(), Some("aaron"));
    }

    // -- configured server-host identity binding --

    fn binding(host: &str, identity: &str) -> ServerBinding {
        ServerBinding {
            host: host.to_string(),
            identity: identity.to_string(),
            active: false,
        }
    }

    fn servers() -> Vec<ServerBinding> {
        vec![
            binding("atomic.storage", "Aaron"),
            binding("staging.atomic.storage", "aaron-staging"),
        ]
    }

    #[test]
    fn host_is_under_matches_self_and_subdomains_on_dot_boundary() {
        assert!(host_is_under("atomic.storage", "atomic.storage")); // exact
        assert!(host_is_under("aaron.atomic.storage", "atomic.storage")); // tenant
        assert!(host_is_under("a.b.atomic.storage", "atomic.storage")); // nested
        assert!(host_is_under("ATOMIC.STORAGE", "atomic.storage")); // case-insensitive
                                                                    // Not a dot-boundary suffix — must NOT match.
        assert!(!host_is_under("notatomic.storage", "atomic.storage"));
        assert!(!host_is_under("atomic.storage.evil.com", "atomic.storage"));
    }

    #[test]
    fn any_tenant_under_a_server_resolves_to_that_servers_identity() {
        for host in [
            "aaron.atomic.storage",
            "bradley.atomic.storage",
            "atomic.atomic.storage",
            "atomic.storage", // apex direct
        ] {
            let url = format!("https://{host}/workspaces/w/projects/p/code");
            assert_eq!(
                resolve_identity_from_url(&url, &servers()).as_deref(),
                Some("Aaron"),
                "host {host} should bind to the prod identity"
            );
        }
    }

    #[test]
    fn longest_suffix_wins_so_staging_beats_prod() {
        // Every staging host also ends in `atomic.storage`; the more specific
        // server must win.
        for host in ["x.staging.atomic.storage", "staging.atomic.storage"] {
            let url = format!("https://{host}/workspaces/w/projects/p/code");
            assert_eq!(
                resolve_identity_from_url(&url, &servers()).as_deref(),
                Some("aaron-staging"),
                "host {host} should bind to the staging identity"
            );
        }
    }

    #[test]
    fn active_profile_wins_an_equal_host_tie() {
        // The legacy [server] block and a named profile point at the SAME
        // server. The active profile's identity must win — previously the
        // named profile always won on a tie, ignoring `default_server`.
        let mut active_legacy = servers();
        active_legacy.push(ServerBinding {
            host: "localhost".to_string(),
            identity: "legacy-identity".to_string(),
            active: true,
        });
        active_legacy.push(binding("localhost", "named-identity"));

        let url = "http://localhost:8444/workspaces/w/projects/p/code";
        assert_eq!(
            resolve_identity_from_url(url, &active_legacy).as_deref(),
            Some("legacy-identity"),
            "the active binding must win an equal-host tie"
        );

        // Flip which binding is active → the answer flips.
        let mut active_named = servers();
        active_named.push(binding("localhost", "legacy-identity"));
        active_named.push(ServerBinding {
            host: "localhost".to_string(),
            identity: "named-identity".to_string(),
            active: true,
        });
        assert_eq!(
            resolve_identity_from_url(url, &active_named).as_deref(),
            Some("named-identity")
        );
    }

    #[test]
    fn longest_suffix_beats_active_shorter_host() {
        // A more specific (longer-host) profile outranks a broader active
        // one: pushing to staging uses staging's identity, not the active
        // prod profile's.
        let mut b = servers();
        b[0].active = true; // atomic.storage / "Aaron" is active
        let url = "https://x.staging.atomic.storage/workspaces/w/projects/p/code";
        assert_eq!(
            resolve_identity_from_url(url, &b).as_deref(),
            Some("aaron-staging"),
            "host specificity outranks the active marker"
        );
    }

    #[test]
    fn active_binding_supplies_identity_for_bare_local_host() {
        // `atomic identity register http://localhost:8444` (no --identity)
        // now binds the registering identity to the legacy [server] block,
        // which is active when no default_server names a profile. A push to
        // that host must resolve to it instead of falling through to the
        // subdomain heuristic or the store default.
        let bindings = vec![ServerBinding {
            host: "localhost".to_string(),
            identity: "leefaus".to_string(),
            active: true,
        }];
        let url = "http://localhost:8444/workspaces/w/projects/p/code";
        assert_eq!(
            resolve_identity_from_url(url, &bindings).as_deref(),
            Some("leefaus")
        );
        // Tenant subdomains of the same server inherit the binding too.
        let url = "http://aaron.localhost:8444/workspaces/w/projects/p/code";
        assert_eq!(
            resolve_identity_from_url(url, &bindings).as_deref(),
            Some("leefaus")
        );
    }

    #[test]
    fn userinfo_beats_configured_server() {
        // An explicit user in the URL is a stronger, per-invocation signal.
        let url = "https://bob@aaron.atomic.storage/workspaces/w/projects/p/code";
        assert_eq!(
            resolve_identity_from_url(url, &servers()).as_deref(),
            Some("bob")
        );
    }

    #[test]
    fn falls_back_to_subdomain_when_no_server_matches() {
        // A host under no configured server keeps the legacy behaviour.
        let url = "https://someone.example.com/workspaces/w/projects/p/code";
        assert_eq!(
            resolve_identity_from_url(url, &servers()).as_deref(),
            Some("someone")
        );
    }

    #[test]
    fn override_resolves_when_subdomain_label_is_unregistered() {
        // Regression: the subdomain label ("aaron") is not a local identity
        // name, but the explicit --identity ("Aaron") is — the override must be
        // what gets evaluated, not the label. With the default-fallback logic,
        // --identity "Aaron" resolves to "Aaron" (it exists) and is usable.
        let inferred = resolve_identity_name_with_override(
            "https://aaron.atomic.storage/workspaces/w/projects/p/code",
            Some("Aaron"),
        );
        let resolved = decide_identity_with_default_fallback(
            inferred.as_deref(),
            true,                   // explicit
            |name| name == "Aaron", // only "Aaron" exists locally
            Some("other".to_string()),
        );
        assert_eq!(resolved.as_deref(), Some("Aaron"));

        // And the credential check passes for that resolved identity.
        let issue = evaluate_push_credentials(true, inferred.as_deref(), Some("Aaron"), |_| true);
        assert_eq!(issue, None);
    }
}

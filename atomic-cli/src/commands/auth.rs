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
//!    follows the server you registered with, so any tenant under it resolves
//!    without `--identity`.
//! 4. Subdomain — `http://alice.localhost:8080/...` → identity "alice" (legacy
//!    last resort).

use atomic_identity::{Identity, IdentityStore};
use atomic_remote::HttpRemoteConfig;
use url::Url;

use crate::error::{CliError, CliResult};

/// Resolve the identity name to authenticate as.
///
/// Priority:
/// 1. An explicit override (the `--identity` flag).
/// 2. URL userinfo (`http://bob@host/...`).
/// 3. A configured server profile whose host is the longest dot-boundary
///    suffix of the remote host (`[servers.prod] url=... identity=...`). This is
///    the binding that makes pushes to *any* tenant under a server you've
///    registered with ("just works" without `--identity`): identity follows the
///    server, not the tenant subdomain.
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
    resolve_identity_from_url(remote_url, &configured_server_identities())
}

/// Collect `(server_host, identity)` pairs from the global config — both the
/// default `[server]` block and every named `[servers.*]` profile — that
/// declare an identity. Servers without an identity binding are skipped.
///
/// Returns an empty list when no config exists or it can't be read, so
/// resolution degrades cleanly to URL-based inference.
fn configured_server_identities() -> Vec<(String, String)> {
    let config = match atomic_config::GlobalConfig::load() {
        Ok(c) => c,
        Err(e) => {
            log::debug!("Could not load global config for server identities: {e}");
            return Vec::new();
        }
    };

    let mut pairs = Vec::new();
    let mut consider = |server: &atomic_config::ServerConfig| {
        if let (Some(url), Some(identity)) = (server.url.as_ref(), server.identity.as_ref()) {
            if let Some(host) = Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(String::from))
            {
                pairs.push((host, identity.clone()));
            }
        }
    };

    consider(&config.server);
    for server in config.servers.values() {
        consider(server);
    }
    pairs
}

/// Pure identity resolution from a URL plus the configured server bindings.
///
/// userinfo → server-host match (longest suffix) → first-label subdomain.
/// Split from the config-loading wrapper so it can be unit-tested without a
/// config file on disk.
fn resolve_identity_from_url(remote_url: &str, servers: &[(String, String)]) -> Option<String> {
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
/// dot-boundary suffix of `remote_host`.
///
/// Longest-suffix wins so a more specific server (`staging.atomic.storage`)
/// beats a broader one (`atomic.storage`) for hosts under both — e.g.
/// `x.staging.atomic.storage` resolves to the staging identity, not prod.
fn match_server_identity(remote_host: &str, servers: &[(String, String)]) -> Option<String> {
    servers
        .iter()
        .filter(|(server_host, _)| host_is_under(remote_host, server_host))
        .max_by_key(|(server_host, _)| server_host.len())
        .map(|(_, identity)| identity.clone())
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
    let identity_name = match resolve_identity_name_with_override(remote_url, identity_override) {
        Some(name) => name,
        None => {
            log::debug!("No identity resolvable from URL: {}", remote_url);
            return config;
        }
    };

    log::debug!("Resolved identity name: {}", identity_name);

    let store = match IdentityStore::open_default() {
        Ok(s) => s,
        Err(e) => {
            log::debug!("Failed to open identity store: {}", e);
            return config;
        }
    };

    // Match by name, tolerating tenant-slug/identity-name case differences
    // (e.g. the `aaron` subdomain selecting the local `Aaron` identity).
    let identity = match load_identity_lenient(&store, &identity_name) {
        Some(id) => id,
        None => {
            log::debug!("Identity '{}' not found in store", identity_name);
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
/// 1. an identity is resolvable from the remote URL,
/// 2. that identity exists in the local store, and
/// 3. its keypair can be loaded (so a token can actually be signed).
#[derive(Debug, PartialEq, Eq)]
pub enum CredentialIssue {
    /// No identity could be resolved from the remote URL (no userinfo and no
    /// subdomain) and `--identity` was not supplied. We can't know who to
    /// authenticate as.
    NoIdentityInUrl,
    /// An identity name was resolved, but no such identity exists locally.
    /// `from_flag` records whether the name came from the `--identity` flag
    /// (vs. being inferred from the remote URL), so the message can point the
    /// user at the right knob.
    IdentityNotFound { name: String, from_flag: bool },
    /// The identity exists but its signing keypair could not be loaded, so no
    /// token can be minted.
    KeypairUnavailable { name: String },
}

impl CredentialIssue {
    /// Render an actionable, accurate error message for this issue.
    ///
    /// All messages point at registering an identity, since that is the single
    /// command that establishes a usable credential for a remote.
    fn message(&self) -> String {
        match self {
            CredentialIssue::NoIdentityInUrl => format!(
                "Not authenticated for {REMEDY_PREFIX}: could not determine an \
                 identity from the remote URL, and no --identity was given. \
                 Pass --identity <NAME> (see `atomic identity list`). {REMEDY_SUFFIX}"
            ),
            // When the name was inferred from the URL, the fix is usually to
            // pass --identity explicitly; when it came from --identity, the
            // name itself is wrong. Tailor the hint to the source.
            CredentialIssue::IdentityNotFound {
                name,
                from_flag: false,
            } => format!(
                "Not authenticated for {REMEDY_PREFIX}: identity '{name}' \
                 (inferred from the remote URL) is not registered on this \
                 machine. Pass --identity <NAME> to select a local identity \
                 (see `atomic identity list`). {REMEDY_SUFFIX}"
            ),
            CredentialIssue::IdentityNotFound {
                name,
                from_flag: true,
            } => format!(
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
/// Returns `Ok(())` when an identity is resolvable (from `--identity` or the
/// URL), exists in the local store, and has a loadable signing keypair.
///
/// `identity_override` is the `--identity` flag value; it takes priority over
/// any identity inferred from the URL, exactly as [`attach_identity`] resolves
/// it. The two must agree, or the fail-fast check here would reject a push that
/// `attach_identity` would have authenticated.
pub fn check_push_credentials(remote_url: &str, identity_override: Option<&str>) -> CliResult<()> {
    let store = IdentityStore::open_default()
        .map_err(|e| CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}")))?;

    let issue = evaluate_push_credentials(
        resolve_identity_name_with_override(remote_url, identity_override),
        identity_override.is_some(),
        |name| load_identity_lenient(&store, name).is_some(),
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
/// * `identity_name` — the resolved identity (override or URL-inferred), if any.
/// * `from_flag` — whether `identity_name` came from `--identity` (vs. the URL).
/// * `identity_exists` — whether an identity with the given name is in the store.
/// * `keypair_loadable` — whether that identity's signing keypair can be loaded.
///
/// Returns `None` when credentials are usable, or `Some(issue)` describing the
/// first problem encountered.
fn evaluate_push_credentials(
    identity_name: Option<String>,
    from_flag: bool,
    identity_exists: impl Fn(&str) -> bool,
    keypair_loadable: impl Fn(&str) -> bool,
) -> Option<CredentialIssue> {
    let name = match identity_name {
        Some(n) => n,
        None => return Some(CredentialIssue::NoIdentityInUrl),
    };

    if !identity_exists(&name) {
        return Some(CredentialIssue::IdentityNotFound { name, from_flag });
    }

    if !keypair_loadable(&name) {
        return Some(CredentialIssue::KeypairUnavailable { name });
    }

    None
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

    // -- pre-push credential check --

    #[test]
    fn creds_ok_when_identity_resolves_and_keypair_loads() {
        let issue = evaluate_push_credentials(
            Some("alice".to_string()),
            false,
            |_| true, // identity exists
            |_| true, // keypair loadable
        );
        assert_eq!(issue, None);
    }

    #[test]
    fn creds_fail_when_no_identity_in_url() {
        // A bare host with no userinfo and no subdomain resolves to no identity.
        let issue = evaluate_push_credentials(None, false, |_| true, |_| true);
        assert_eq!(issue, Some(CredentialIssue::NoIdentityInUrl));
    }

    #[test]
    fn creds_fail_when_identity_missing_from_store() {
        let issue = evaluate_push_credentials(
            Some("alice".to_string()),
            false,
            |_| false, // identity not in store
            |_| true,
        );
        assert_eq!(
            issue,
            Some(CredentialIssue::IdentityNotFound {
                name: "alice".to_string(),
                from_flag: false,
            })
        );
    }

    #[test]
    fn creds_fail_when_keypair_cannot_load() {
        // Identity exists but its signing key can't be loaded — no token can be
        // minted. This stands in for an "expired"/unusable credential, which in
        // this self-signed-JWT model means "cannot sign right now".
        let issue = evaluate_push_credentials(
            Some("alice".to_string()),
            false,
            |_| true,  // identity exists
            |_| false, // keypair unavailable
        );
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
            CredentialIssue::NoIdentityInUrl,
            CredentialIssue::IdentityNotFound {
                name: "alice".to_string(),
                from_flag: false,
            },
            CredentialIssue::IdentityNotFound {
                name: "alice".to_string(),
                from_flag: true,
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

    fn servers() -> Vec<(String, String)> {
        vec![
            ("atomic.storage".to_string(), "Aaron".to_string()),
            (
                "staging.atomic.storage".to_string(),
                "aaron-staging".to_string(),
            ),
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
        // what gets evaluated, not the label.
        let issue = evaluate_push_credentials(
            resolve_identity_name_with_override(
                "https://aaron.atomic.storage/workspaces/w/projects/p/code",
                Some("Aaron"),
            ),
            true,
            |name| name == "Aaron", // only "Aaron" exists locally
            |name| name == "Aaron",
        );
        assert_eq!(issue, None);
    }
}

//! Shared authentication helpers for remote commands.
//!
//! Resolves the caller's identity from the remote URL, mints a short-lived
//! self-signed EdDSA JWT for that identity, and attaches it as the
//! `Authorization: Bearer` header.
//!
//! A raw public key is NOT a credential — the JWT (signed with the identity's
//! private key) proves possession of that key. See [`crate::commands::token`]
//! for the minting mechanics.
//!
//! Identity resolution order:
//! 1. URL userinfo — `http://bob@alice.localhost:8080/...` → identity "bob"
//! 2. Subdomain — `http://alice.localhost:8080/...` → identity "alice"

use atomic_identity::IdentityStore;
use atomic_remote::HttpRemoteConfig;
use url::Url;

/// Attach a Bearer JWT auth header to the remote config by resolving the
/// identity from the URL and logging in for a token.
///
/// If the identity cannot be resolved (no userinfo, no subdomain, or identity
/// not found in the store) or login fails, the config is returned unmodified
/// and a debug log is emitted. This keeps push/pull/clone working against
/// servers that don't require auth (e.g. public reads).
pub async fn attach_identity(config: HttpRemoteConfig, remote_url: &str) -> HttpRemoteConfig {
    let identity_name = match resolve_identity_name(remote_url) {
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

    let identity = match store.load_by_name(&identity_name) {
        Ok(id) => id,
        Err(e) => {
            log::debug!("Identity '{}' not found in store: {}", identity_name, e);
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

/// Extract the identity name from a remote URL.
///
/// Tries URL userinfo first (`bob@host`), then the subdomain (`bob.host`).
fn resolve_identity_name(remote_url: &str) -> Option<String> {
    let url = Url::parse(remote_url).ok()?;

    // 1. Explicit: http://bob@alice.localhost:8080/...
    let username = url.username();
    if !username.is_empty() {
        return Some(username.to_string());
    }

    // 2. Implicit: http://alice.localhost:8080/... → "alice"
    extract_subdomain(url.host_str()?)
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
}

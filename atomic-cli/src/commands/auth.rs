//! Shared authentication helpers for remote commands.
//!
//! Resolves the caller's identity from the remote URL and builds
//! the `Authorization: Bearer` header for authenticated requests.
//!
//! Identity resolution order:
//! 1. URL userinfo — `http://bob@alice.localhost:8080/...` → identity "bob"
//! 2. Subdomain — `http://alice.localhost:8080/...` → identity "alice"

use atomic_identity::IdentityStore;
use atomic_remote::HttpRemoteConfig;
use url::Url;

/// Attach a Bearer auth header to the remote config by resolving the
/// identity from the URL.
///
/// If the identity cannot be resolved (no userinfo, no subdomain, or
/// identity not found in the store), the config is returned unmodified
/// and a debug log is emitted. This keeps push/pull/clone working
/// against servers that don't require auth.
pub fn attach_identity(config: HttpRemoteConfig, remote_url: &str) -> HttpRemoteConfig {
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

    let public_key_b32 = identity.public_key_base32();
    log::debug!("Attaching Bearer auth for identity '{}'", identity_name);

    config.with_header("Authorization", format!("Bearer {}", public_key_b32))
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

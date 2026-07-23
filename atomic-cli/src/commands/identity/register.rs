//! The `identity register` command for registering with an atomic-storage server.
//!
//! This module implements the `atomic identity register` command, which pushes
//! the user's local identity to a remote atomic-storage server to create a
//! tenant. The server verifies the Ed25519 signature and creates a tenant
//! whose slug is derived from the identity's username.
//!
//! # Usage
//!
//! ```text
//! atomic identity register <SERVER_URL> [OPTIONS]
//!
//! Arguments:
//!   <SERVER_URL>  URL of the atomic-storage server (e.g. https://atomic.storage)
//!
//! Options:
//!   -i, --identity <NAME>  Identity to register (defaults to current default)
//!   -h, --help             Print help information
//! ```
//!
//! # Examples
//!
//! ```text
//! # Register with the default identity
//! $ atomic identity register https://atomic.storage
//! ✓ Registered as alice
//!   Tenant:   alice
//!   URL:      https://alice.atomic.storage
//!
//! # Register a specific identity
//! $ atomic identity register https://atomic.storage --identity alice-work
//! ```
//!
//! # Protocol
//!
//! The command constructs a signed registration payload:
//!
//! 1. Loads the identity and its Ed25519 keypair from local storage
//! 2. Builds a canonical signing payload:
//!    `atomic-storage:register\n{username}\n{public_key_base32}\n{timestamp}`
//! 3. Signs the payload with the identity's secret key
//! 4. POSTs the request to `{server_url}/register`
//! 5. Displays the server's response (tenant slug, base URL)

use chrono::Utc;
use clap::Parser;
use data_encoding::BASE32_NOPAD;

use atomic_config::GlobalConfig;
use atomic_identity::IdentityStore;

use crate::commands::Command;
use crate::error::{CliError, CliResult};
use crate::output::{print_error, print_success};

/// Signing domain prefix — must match the server's `registration.rs`.
const SIGNING_DOMAIN: &str = "atomic-storage:register";

/// Register an identity with a remote atomic-storage server.
///
/// Pushes the local identity to the server, which creates a tenant
/// whose slug is derived from the identity's username. Authentication
/// is Ed25519 signature-based — no passwords required.
///
/// When `--identity` is specified, a named server profile is automatically
/// created in `~/.atomic/config.toml` so that commands targeting this
/// server use that identity automatically. The profile name is derived
/// from the server hostname (e.g. `staging.atomic.storage` → `staging`).
/// Use `--server-name <name>` to choose a custom profile name.
#[derive(Debug, Parser)]
pub struct Register {
    /// URL of the atomic-storage server.
    ///
    /// The server must be running and reachable. The `/register` endpoint
    /// will be called automatically.
    ///
    /// Examples:
    ///   <https://atomic.storage>
    ///   <http://localhost:8080>
    #[arg(required = true)]
    pub server_url: String,

    /// Name of the identity to register.
    ///
    /// If not specified, the current default identity is used. When
    /// specified, the server URL and identity are automatically saved as
    /// a named server profile (see `--server-name`).
    #[arg(short, long)]
    pub identity: Option<String>,

    /// Custom name for the auto-created server profile.
    ///
    /// Only used when `--identity` is given. Defaults to the first
    /// hostname label of the server URL (e.g. `staging` from
    /// `staging.atomic.storage`).
    #[arg(long)]
    pub server_name: Option<String>,
}

impl Command for Register {
    fn run(&self) -> CliResult<()> {
        // We need async for the HTTP request. Build a one-shot runtime
        // rather than depending on the caller being inside tokio.
        let rt = tokio::runtime::Runtime::new().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to create async runtime: {e}"))
        })?;

        rt.block_on(self.execute())
    }
}

impl Register {
    async fn execute(&self) -> CliResult<()> {
        // 1. Open the identity store.
        let store = IdentityStore::open_default().map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to open identity store: {e}"))
        })?;

        // 2. Load the identity.
        let identity = if let Some(name) = &self.identity {
            store
                .load_by_name(name)
                .map_err(|_| CliError::IdentityNotFound(name.clone()))?
        } else {
            store
                .get_default()
                .map_err(|e| {
                    CliError::Internal(anyhow::anyhow!("Failed to load default identity: {e}"))
                })?
                .ok_or_else(|| {
                    CliError::Internal(anyhow::anyhow!(
                        "No default identity set. Create one first:\n  \
                         atomic identity new <name> --email <email> --set-default"
                    ))
                })?
        };

        // 3. Load the keypair (needs the secret key for signing).
        let keypair = store.load_keypair(&identity.id, None).map_err(|e| {
            CliError::Internal(anyhow::anyhow!(
                "Failed to load keypair for '{}': {e}",
                identity.name
            ))
        })?;

        // 4. Build the registration payload.
        let username = &identity.name;
        let public_key_b32 = identity.public_key_base32();
        let timestamp = Utc::now();
        let timestamp_str = timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        // 5. Build and sign the canonical payload.
        let payload = format!("{SIGNING_DOMAIN}\n{username}\n{public_key_b32}\n{timestamp_str}");
        let signature_bytes = keypair.sign(payload.as_bytes());
        let signature_b32 = BASE32_NOPAD.encode(&signature_bytes);

        // 6. Construct the JSON request body.
        let body = serde_json::json!({
            "username": username,
            "email": identity.email,
            "public_key": public_key_b32,
            "timestamp": timestamp_str,
            "signature": signature_b32,
        });

        // 7. POST to the server.
        let url = format!("{}/register", self.server_url.trim_end_matches('/'));

        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .json(&body)
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| CliError::RemoteError {
                message: format!("Failed to connect to server: {e}"),
                url: Some(url.clone()),
            })?;

        let status = response.status();
        let response_text = response.text().await.map_err(|e| {
            CliError::Internal(anyhow::anyhow!("Failed to read response body: {e}"))
        })?;

        // 8. Handle the response.
        if status.is_success() {
            let result: serde_json::Value = serde_json::from_str(&response_text)
                .map_err(|e| CliError::Internal(anyhow::anyhow!("Invalid server response: {e}")))?;

            // The server wraps the response in ApiResponse { success, data, ... }
            let data = result.get("data").unwrap_or(&result);

            let tenant_id = data
                .get("tenant_id")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let slug = data
                .get("slug")
                .and_then(|v| v.as_str())
                .unwrap_or(username);
            let base_url = data
                .get("base_url")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            // Single-tenant deployments report `mode: "single"` (field added
            // server-side in v1.4.0; absent on older servers = multi-tenant).
            // On a single-tenant server the typed URL IS the tenant URL —
            // no `{org}.` prefixing — so the profile must keep it verbatim
            // and mark the server single-tenant.
            let (single_tenant, role) = parse_tenancy_mode(data);

            let clean_server_url = self.server_url.trim_end_matches('/').to_string();
            let mut config = GlobalConfig::load().map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to load global config: {e}"))
            })?;

            // When --identity is specified, register as a named server profile
            // so the URL+identity combo is remembered automatically.
            if self.identity.is_some() {
                let profile_name = self
                    .server_name
                    .clone()
                    .or_else(|| derive_profile_name(&clean_server_url))
                    .unwrap_or_else(|| slug.to_string());

                let profile = atomic_config::ServerConfig {
                    url: Some(clean_server_url.clone()),
                    default_org: Some(slug.to_string()),
                    default_workspaces: std::collections::BTreeMap::new(),
                    identity: Some(identity.name.clone()),
                    single_tenant,
                };

                config.servers.insert(profile_name.clone(), profile);

                // If there's no active named server yet, make this one active.
                if config.default_server.is_none() && config.server.is_configured() {
                    // Legacy [server] is already set — don't override it silently.
                    // Just register the profile; user can activate with `atomic server set`.
                    println!(
                        "  Profile:   '{}' added (activate with: atomic server set {})",
                        profile_name, profile_name
                    );
                } else if config.default_server.is_none() {
                    config.default_server = Some(profile_name.clone());
                    println!("  Profile:   '{}' (now active)", profile_name);
                } else {
                    println!(
                        "  Profile:   '{}' added (activate with: atomic server set {})",
                        profile_name, profile_name
                    );
                }
            } else {
                // Default path: update the legacy [server] block.
                config.server.url = Some(clean_server_url.clone());
                config.server.default_org = Some(slug.to_string());
                config.server.single_tenant = single_tenant;
            }

            config.save().map_err(|e| {
                CliError::Internal(anyhow::anyhow!("Failed to save global config: {e}"))
            })?;

            print_success(&format!("Registered as {slug}"));
            println!();
            println!("  Tenant ID: {tenant_id}");
            println!("  Slug:      {slug}");
            println!("  URL:       {base_url}");
            println!("  Identity:  {} ({})", identity.name, identity.id.short());
            if single_tenant {
                println!(
                    "  Mode:      single-tenant (role: {})",
                    role.unwrap_or("member")
                );
            }

            if slug != username {
                println!();
                println!(
                    "  {}",
                    crate::output::hint(format!(
                        "Note: slug '{slug}' differs from username '{username}' \
                         (normalized or collision-resolved)"
                    ))
                );
            }

            crate::output::print_next_steps(&[
                (
                    "atomic workspace create <name>",
                    "Create your first workspace",
                ),
                (
                    "atomic workspace set <name>",
                    "Set as your default workspace",
                ),
                (
                    "atomic project create <project-name>",
                    "Create a project (prints the clone/push URL)",
                ),
            ]);
        } else {
            // Parse error response.
            let error_msg = serde_json::from_str::<serde_json::Value>(&response_text)
                .ok()
                .and_then(|v| {
                    // Try ApiError shape: { code, message }
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .map(String::from)
                        .or_else(|| {
                            // Try ApiResponse shape: { error: { message } }
                            v.get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(|m| m.as_str())
                                .map(String::from)
                        })
                })
                .unwrap_or_else(|| format!("Server returned {status}"));

            print_error(&format!("Registration failed: {error_msg}"));

            if status.as_u16() == 409 {
                println!();
                println!(
                    "  {}",
                    crate::output::hint(
                        "This public key is already registered. \
                         Use a different identity or contact support."
                    )
                );
            }

            return Err(CliError::RemoteError {
                message: error_msg,
                url: Some(url),
            });
        }

        Ok(())
    }
}

/// Derive a short profile name from a server URL.
///
/// `https://staging.atomic.storage` → `"staging"`
/// `https://atomic.storage`         → `"atomic-storage"` (no subdomain)
/// `http://localhost:8080`           → `"localhost"`
fn derive_profile_name(server_url: &str) -> Option<String> {
    let url = url::Url::parse(server_url).ok()?;
    let host = url.host_str()?;

    // If the host has a subdomain, use that as the profile name.
    if let Some(dot) = host.find('.') {
        let label = &host[..dot];
        if !label.is_empty() && label != "www" {
            return Some(label.to_string());
        }
        // No useful subdomain — fall through to full host slug.
    }

    // Slug the host: replace dots and colons with hyphens, strip port.
    let host_no_port = host.split(':').next().unwrap_or(host);
    let slug = host_no_port.replace('.', "-");
    if slug.is_empty() {
        None
    } else {
        Some(slug)
    }
}

/// Interpret the registration response's tenancy signal.
///
/// `mode: "single"` (server v1.4.0+) marks a single-tenant deployment and
/// carries the org `role` the identity received; anything else — `"multi"`
/// or a missing field (older servers) — keeps multi-tenant URL semantics.
/// Returns `(single_tenant, role)`.
fn parse_tenancy_mode(data: &serde_json::Value) -> (bool, Option<&str>) {
    let single = data.get("mode").and_then(|v| v.as_str()) == Some("single");
    let role = data.get("role").and_then(|v| v.as_str());
    (single, role)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_payload_format() {
        let username = "alice";
        let public_key = "ABCDEF1234567890";
        let timestamp = "2025-03-26T17:00:00Z";

        let payload = format!("{SIGNING_DOMAIN}\n{username}\n{public_key}\n{timestamp}");
        assert_eq!(
            payload,
            "atomic-storage:register\nalice\nABCDEF1234567890\n2025-03-26T17:00:00Z"
        );
    }

    #[test]
    fn server_url_trailing_slash_stripped() {
        let url_with_slash = "https://atomic.storage/";
        let url_without_slash = "https://atomic.storage";

        let endpoint_a = format!("{}/register", url_with_slash.trim_end_matches('/'));
        let endpoint_b = format!("{}/register", url_without_slash.trim_end_matches('/'));

        assert_eq!(endpoint_a, "https://atomic.storage/register");
        assert_eq!(endpoint_b, "https://atomic.storage/register");
    }

    #[test]
    fn tenancy_mode_single_with_role() {
        let data = serde_json::json!({"mode": "single", "role": "owner"});
        let (single, role) = parse_tenancy_mode(&data);
        assert!(single);
        assert_eq!(role, Some("owner"));
    }

    #[test]
    fn tenancy_mode_multi() {
        let data = serde_json::json!({"mode": "multi"});
        let (single, role) = parse_tenancy_mode(&data);
        assert!(!single);
        assert_eq!(role, None);
    }

    #[test]
    fn tenancy_mode_missing_field_defaults_multi() {
        // Legacy server responses (pre-mode field) must not flip profile
        // semantics — they keep org-prefixed URLs.
        let data = serde_json::json!({"slug": "alice", "base_url": "https://alice.atomic.storage"});
        let (single, role) = parse_tenancy_mode(&data);
        assert!(!single);
        assert_eq!(role, None);
    }

    #[test]
    fn command_fields() {
        let cmd = Register {
            server_url: "http://localhost:8080".to_string(),
            identity: Some("alice".to_string()),
            server_name: None,
        };

        assert_eq!(cmd.server_url, "http://localhost:8080");
        assert_eq!(cmd.identity, Some("alice".to_string()));
    }

    #[test]
    fn command_default_identity() {
        let cmd = Register {
            server_url: "http://localhost:8080".to_string(),
            identity: None,
            server_name: None,
        };

        assert!(cmd.identity.is_none());
    }

    #[test]
    fn derive_profile_name_subdomain() {
        assert_eq!(
            derive_profile_name("https://staging.atomic.storage"),
            Some("staging".to_string())
        );
    }

    #[test]
    fn derive_profile_name_no_subdomain() {
        // "atomic.storage" — "atomic" is the first label (subdomain of "storage")
        // so it's used as the profile name.
        assert_eq!(
            derive_profile_name("https://atomic.storage"),
            Some("atomic".to_string())
        );
    }

    #[test]
    fn derive_profile_name_localhost() {
        assert_eq!(
            derive_profile_name("http://localhost:8080"),
            Some("localhost".to_string())
        );
    }
}

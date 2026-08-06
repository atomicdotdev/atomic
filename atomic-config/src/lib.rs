//! Configuration management for Atomic VCS
//!
//! This crate handles loading, saving, and managing configuration for Atomic
//! repositories at three levels:
//!
//! 1. **System** - `/etc/atomic/config.toml` (Unix) or system-wide location
//! 2. **User** - `~/.atomic/config.toml` in the user's home directory
//! 3. **Repository** - `.atomic/config.toml` in the repository root
//!
//! Configuration is merged with later levels overriding earlier ones.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

/// Configuration errors
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read configuration file: {path}")]
    ReadError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to parse configuration: {0}")]
    ParseError(#[from] toml::de::Error),

    #[error("Failed to serialize configuration: {0}")]
    SerializeError(#[from] toml::ser::Error),

    #[error("Failed to write configuration file: {path}")]
    WriteError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Could not determine configuration directory")]
    NoConfigDir,
}

/// Author information for changes
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Author {
    /// Display name
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,

    /// Email address
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// Identity key reference
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
}

/// Configuration for remote atomic-storage server connection.
///
/// Set during `atomic identity register` and used by management commands
/// (workspace, project, org, team) to connect to the server.
///
/// ```toml
/// [server]
/// url = "https://atomic.storage"
/// default_org = "alice"
///
/// [server.default_workspaces]
/// alice = "personal"
/// acme = "backend"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerConfig {
    /// Base URL of the atomic-storage server.
    ///
    /// The management API constructs org-scoped URLs as
    /// `https://{default_org}.{domain}` from this base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Default organization slug for management commands.
    ///
    /// Set automatically during registration (personal org = identity name).
    /// Can be switched with `atomic org set`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_org: Option<String>,

    /// Default workspace slug per organization.
    ///
    /// Workspaces are org-scoped, so the default is stored per org. When
    /// a management command needs a workspace and none is given on the
    /// CLI, the resolver looks up the current org in this map.
    ///
    /// Set with `atomic workspace set <slug> [--org <slug>]`.
    /// Uses `BTreeMap` so the serialized TOML is alphabetically ordered
    /// and produces stable diffs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub default_workspaces: BTreeMap<String, String>,

    /// Identity name to use when authenticating to this server.
    ///
    /// If set, management commands targeting this server use this identity
    /// instead of the global default. Set automatically during
    /// `atomic identity register` when `--identity` is specified.
    ///
    /// Example: `identity = "alice-staging"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,

    /// Whether the server is a single-tenant deployment.
    ///
    /// Single-tenant servers (reported by the registration response's
    /// `mode = "single"`) serve exactly one organization at their bare host:
    /// org-scoped URLs are NOT prefixed with the org slug — every path is
    /// already scoped to the single tenant server-side.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub single_tenant: bool,
}

impl ServerConfig {
    /// Whether the server has been configured (registration completed).
    pub fn is_configured(&self) -> bool {
        self.url.is_some() && self.default_org.is_some()
    }

    /// Build the org-scoped base URL.
    ///
    /// Given `url = "https://atomic.storage"` and `org = "alice"`,
    /// returns `"https://alice.atomic.storage"`.
    ///
    /// Given `url = "http://localhost:8080"` and `org = "alice"`,
    /// returns `"http://alice.localhost:8080"`.
    ///
    /// For single-tenant servers the org is already implied by the server
    /// itself, so the URL is returned unchanged (no `{org}.` prefix).
    pub fn org_base_url(&self, org_slug: &str) -> Option<String> {
        let url = self.url.as_ref()?;

        // Single-tenant: the bare host is already tenant-scoped server-side;
        // prefixing would produce bogus hosts like `org.org.org.example.com`.
        if self.single_tenant {
            return Some(url.trim_end_matches('/').to_string());
        }

        // Parse the URL to extract scheme, host, port
        let url_parsed = url::Url::parse(url).ok()?;
        let scheme = url_parsed.scheme();
        let host = url_parsed.host_str()?;
        let port = url_parsed.port();

        let base = if let Some(port) = port {
            format!("{}://{}.{}:{}", scheme, org_slug, host, port)
        } else {
            format!("{}://{}.{}", scheme, org_slug, host)
        };

        Some(base)
    }

    /// Get the org-scoped base URL using the default org.
    pub fn default_org_base_url(&self) -> Option<String> {
        let org = self.default_org.as_ref()?;
        self.org_base_url(org)
    }
}

/// Global configuration settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalConfig {
    /// Default author information
    #[serde(default)]
    pub author: Author,

    /// Default channel name for new repositories
    #[serde(default = "default_channel_name")]
    pub default_channel: String,

    /// Enable colored output
    #[serde(default)]
    pub colors: Option<ColorChoice>,

    /// Use pager for long output
    #[serde(default)]
    pub pager: Option<bool>,

    /// Global workspace configuration.
    ///
    /// Expose patterns defined here apply to ALL repositories.
    /// Repo-local `[workspace] expose` patterns are merged on top.
    #[serde(default)]
    pub workspace: WorkspaceConfig,

    /// Default remote server configuration for atomic-storage.
    ///
    /// This is the server used when no `--server` flag is given and no
    /// `default_server` points to a named server in `servers`.
    #[serde(default)]
    pub server: ServerConfig,

    /// Named server profiles (e.g. "staging", "prod").
    ///
    /// Each entry is a full `ServerConfig` with its own URL, default org,
    /// workspaces, and optional identity override.
    ///
    /// ```toml
    /// [servers.staging]
    /// url = "https://staging.atomic.storage"
    /// default_org = "alice"
    /// identity = "alice-staging"
    ///
    /// [servers.prod]
    /// url = "https://atomic.storage"
    /// default_org = "alice"
    /// identity = "alice-prod"
    /// ```
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<String, ServerConfig>,

    /// Name of the active server profile from `servers`.
    ///
    /// When set, management commands use `servers[default_server]` instead
    /// of `server`. Switch with `atomic server set <name>` or pass
    /// `--server <name>` per-command.
    ///
    /// When `None`, the legacy `[server]` block is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_server: Option<String>,
}

fn default_channel_name() -> String {
    "main".to_string()
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            author: Author::default(),
            default_channel: default_channel_name(),
            colors: None,
            pager: None,
            workspace: WorkspaceConfig::default(),
            server: ServerConfig::default(),
            servers: BTreeMap::new(),
            default_server: None,
        }
    }
}

impl GlobalConfig {
    /// Resolve the effective server config, honouring the optional name override.
    ///
    /// Resolution order:
    /// 1. `server_override` (from `--server <name>` CLI flag)
    /// 2. `default_server` (from `~/.atomic/config.toml`)
    /// 3. Legacy `server` block
    ///
    /// Returns `(server_config, Option<name>)` — the name is `Some` when a
    /// named profile was resolved, `None` for the legacy block.
    pub fn resolve_server<'a>(
        &'a self,
        server_override: Option<&'a str>,
    ) -> Result<(&'a ServerConfig, Option<&'a str>), String> {
        // 1. Explicit --server flag
        if let Some(name) = server_override {
            return self
                .servers
                .get(name)
                .map(|s| (s, Some(name)))
                .ok_or_else(|| {
                    format!(
                        "Server profile '{}' not found. \
                         Use 'atomic server list' to see available profiles, \
                         or 'atomic server add {0} <url>' to create it.",
                        name
                    )
                });
        }

        // 2. Configured default server name
        if let Some(ref name) = self.default_server {
            return self
                .servers
                .get(name.as_str())
                .map(|s| (s, Some(name.as_str())))
                .ok_or_else(|| {
                    format!(
                        "Default server profile '{}' not found in servers map. \
                         Run 'atomic server set <name>' to fix.",
                        name
                    )
                });
        }

        // 3. Legacy [server] block
        Ok((&self.server, None))
    }

    /// Resolve the effective server config mutably, honouring the optional
    /// name override.
    ///
    /// Mirrors the resolution order of [`resolve_server`](Self::resolve_server)
    /// so that writes (e.g. `atomic org set`, `atomic workspace set`) land on
    /// the same profile that reads resolve to:
    ///
    /// 1. `server_override` (from `--server <name>` CLI flag)
    /// 2. `default_server` (from `~/.atomic/config.toml`)
    /// 3. Legacy `server` block
    ///
    /// Returns the mutable profile plus its name (`Some` for a named profile,
    /// `None` for the legacy block).
    pub fn resolve_server_mut(
        &mut self,
        server_override: Option<&str>,
    ) -> Result<(&mut ServerConfig, Option<String>), String> {
        // Determine which profile name to target, if any. We resolve the name
        // first (immutable borrow) so the mutable borrow below is unambiguous.
        let name = if let Some(name) = server_override {
            Some(name.to_string())
        } else {
            self.default_server.clone()
        };

        match name {
            Some(name) => {
                let profile = self.servers.get_mut(&name).ok_or_else(|| {
                    format!(
                        "Server profile '{}' not found. \
                         Use 'atomic server list' to see available profiles, \
                         or 'atomic server add {0} <url>' to create it.",
                        name
                    )
                })?;
                Ok((profile, Some(name)))
            }
            None => Ok((&mut self.server, None)),
        }
    }
}

/// Color output preference
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColorChoice {
    /// Automatically detect based on terminal
    #[default]
    Auto,
    /// Always use colors
    Always,
    /// Never use colors
    Never,
}

/// Repository-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoConfig {
    /// Override author for this repository
    #[serde(default)]
    pub author: Option<Author>,

    /// Remote repositories
    #[serde(default)]
    pub remotes: Vec<RemoteConfig>,

    /// Workspace configuration for view switching behavior.
    #[serde(default)]
    pub workspace: WorkspaceConfig,
}

/// Controls how ignored files are handled during view switches.
///
/// By default, all ignored files (`.atomicignore`) are shelved per-view
/// when switching — build artifacts get isolated so each view has its own
/// `node_modules/`, `target/`, etc. Paths listed in `expose` are the
/// exception: they persist across all views and are never shelved.
///
/// This keeps tool configs (`.opencode/`, `.vscode/`, `.idea/`) stable
/// while build artifacts are managed per-view automatically.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Paths that persist across all views (never shelved).
    ///
    /// Everything in `.atomicignore` is shelved per-view on switch,
    /// EXCEPT paths matching these patterns — those are left alone.
    ///
    /// Example:
    /// ```toml
    /// [workspace]
    /// expose = [".opencode", ".vscode", ".idea"]
    /// ```
    #[serde(default)]
    pub expose: Vec<String>,
}

/// Remote repository configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Name of the remote (e.g., "origin")
    pub name: String,

    /// URL of the remote repository
    pub url: String,

    /// Default channel to push/pull
    #[serde(default)]
    pub default_channel: Option<String>,
}

/// Get the global configuration directory.
///
/// Resolution order:
/// 1. The `ATOMIC_CONFIG_DIR` environment variable, if set — used verbatim as
///    the config directory (the one containing `config.toml`). This lets tests
///    isolate the global config on every platform, and lets users relocate it.
/// 2. `dirs::home_dir()/.atomic` (the default).
///
/// The variable is read at call time so it can be set and restored per test
/// (`dirs::home_dir()` on Windows resolves the profile known-folder, not
/// `HOME`, so an env override is the only reliable cross-platform isolation).
pub fn global_config_dir() -> Option<PathBuf> {
    global_config_dir_from(std::env::var_os("ATOMIC_CONFIG_DIR"))
}

/// Resolve the global config directory from an explicit override value.
///
/// Split out so the override precedence is unit-testable without mutating
/// process environment (which is racy and platform-sensitive).
fn global_config_dir_from(env_override: Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(dir) = env_override {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|p| p.join(".atomic"))
}

/// Get the global configuration file path
pub fn global_config_path() -> Option<PathBuf> {
    global_config_dir().map(|p| p.join("config.toml"))
}

impl GlobalConfig {
    /// Load global configuration from the default location
    pub fn load() -> Result<Self, ConfigError> {
        let path = global_config_path().ok_or(ConfigError::NoConfigDir)?;

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(&path).map_err(|e| ConfigError::ReadError {
            path: path.clone(),
            source: e,
        })?;

        Ok(toml::from_str(&content)?)
    }

    /// Save global configuration to the default location
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = global_config_path().ok_or(ConfigError::NoConfigDir)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::WriteError {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content).map_err(|e| ConfigError::WriteError { path, source: e })?;

        Ok(())
    }
}

impl RepoConfig {
    /// Load repository configuration from a specific path
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::ReadError {
            path: path.to_path_buf(),
            source: e,
        })?;

        Ok(toml::from_str(&content)?)
    }

    /// Save repository configuration to a specific path
    pub fn save(&self, path: &std::path::Path) -> Result<(), ConfigError> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content).map_err(|e| ConfigError::WriteError {
            path: path.to_path_buf(),
            source: e,
        })?;

        Ok(())
    }

    /// Get a remote by name
    pub fn get_remote(&self, name: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|r| r.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_author_serialization() {
        let author = Author {
            name: "Test User".to_string(),
            email: Some("test@example.com".to_string()),
            identity: None,
        };

        let toml_str = toml::to_string(&author).unwrap();
        let parsed: Author = toml::from_str(&toml_str).unwrap();
        assert_eq!(author, parsed);
    }

    #[test]
    fn test_global_config_default() {
        let config = GlobalConfig::default();
        assert_eq!(config.default_channel, "main");
    }

    #[test]
    fn test_server_config_default() {
        let config = ServerConfig::default();
        assert!(!config.is_configured());
        assert!(config.url.is_none());
        assert!(config.default_org.is_none());
        assert!(config.org_base_url("alice").is_none());
        assert!(config.default_org_base_url().is_none());
    }

    #[test]
    fn test_server_config_is_configured() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: false,
        };
        assert!(config.is_configured());

        // Missing org → not configured
        let partial = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: None,
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: false,
        };
        assert!(!partial.is_configured());

        // Missing url → not configured
        let partial = ServerConfig {
            url: None,
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: false,
        };
        assert!(!partial.is_configured());
    }

    #[test]
    fn test_server_config_org_base_url() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: false,
        };
        assert_eq!(
            config.org_base_url("alice"),
            Some("https://alice.atomic.storage".to_string())
        );
        assert_eq!(
            config.org_base_url("acme-corp"),
            Some("https://acme-corp.atomic.storage".to_string())
        );
    }

    #[test]
    fn test_server_config_org_base_url_with_port() {
        let config = ServerConfig {
            url: Some("http://localhost:8080".to_string()),
            default_org: None,
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: false,
        };
        assert_eq!(
            config.org_base_url("alice"),
            Some("http://alice.localhost:8080".to_string())
        );
    }

    #[test]
    fn test_server_config_default_org_base_url() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: false,
        };
        assert_eq!(
            config.default_org_base_url(),
            Some("https://alice.atomic.storage".to_string())
        );

        // No default org → None
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: None,
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: false,
        };
        assert!(config.default_org_base_url().is_none());
    }

    #[test]
    fn test_server_config_serialization_roundtrip() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: false,
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("url = \"https://atomic.storage\""));
        assert!(toml_str.contains("default_org = \"alice\""));

        let parsed: ServerConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.url, config.url);
        assert_eq!(parsed.default_org, config.default_org);
    }

    #[test]
    fn test_global_config_with_server() {
        let config = GlobalConfig {
            author: Author {
                name: "Test User".to_string(),
                email: Some("test@example.com".to_string()),
                identity: None,
            },
            server: ServerConfig {
                url: Some("https://atomic.storage".to_string()),
                default_org: Some("alice".to_string()),
                default_workspaces: BTreeMap::new(),
                identity: None,
                single_tenant: false,
            },
            ..GlobalConfig::default()
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("[server]"));
        assert!(toml_str.contains("url = \"https://atomic.storage\""));

        let parsed: GlobalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed.server.url,
            Some("https://atomic.storage".to_string())
        );
        assert_eq!(parsed.server.default_org, Some("alice".to_string()));
        assert!(parsed.server.is_configured());
    }

    #[test]
    fn test_default_workspaces_skipped_when_empty() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: false,
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(!toml_str.contains("default_workspaces"));
    }

    #[test]
    fn test_default_workspaces_roundtrip() {
        let mut workspaces = BTreeMap::new();
        workspaces.insert("alice".to_string(), "personal".to_string());
        workspaces.insert("acme".to_string(), "backend".to_string());

        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
            default_workspaces: workspaces,
            identity: None,
            single_tenant: false,
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("[default_workspaces]"));

        // BTreeMap → alphabetical, so "acme" appears before "alice"
        let acme_pos = toml_str.find("acme = \"backend\"").unwrap();
        let alice_pos = toml_str.find("alice = \"personal\"").unwrap();
        assert!(acme_pos < alice_pos);

        let parsed: ServerConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            parsed.default_workspaces.get("alice"),
            Some(&"personal".to_string())
        );
        assert_eq!(
            parsed.default_workspaces.get("acme"),
            Some(&"backend".to_string())
        );
    }

    #[test]
    fn test_resolve_server_mut_targets_named_profile() {
        // default_server points at a named profile → mutation must land there,
        // not on the legacy [server] block.
        let mut config = GlobalConfig {
            default_server: Some("prod".to_string()),
            ..GlobalConfig::default()
        };
        config.servers.insert(
            "prod".to_string(),
            ServerConfig {
                url: Some("https://atomic.storage".to_string()),
                default_org: None,
                default_workspaces: BTreeMap::new(),
                identity: Some("continuouslee".to_string()),
                single_tenant: false,
            },
        );

        let (server, name) = config.resolve_server_mut(None).unwrap();
        assert_eq!(name.as_deref(), Some("prod"));
        server.default_org = Some("atomic".to_string());

        assert_eq!(
            config.servers["prod"].default_org.as_deref(),
            Some("atomic")
        );
        assert!(config.server.default_org.is_none());
    }

    #[test]
    fn test_resolve_server_mut_falls_back_to_legacy_block() {
        // No default_server and no override → legacy [server] block.
        let mut config = GlobalConfig::default();

        let (server, name) = config.resolve_server_mut(None).unwrap();
        assert!(name.is_none());
        server.default_org = Some("alice".to_string());

        assert_eq!(config.server.default_org.as_deref(), Some("alice"));
    }

    #[test]
    fn test_resolve_server_mut_override_wins() {
        let mut config = GlobalConfig {
            default_server: Some("prod".to_string()),
            ..GlobalConfig::default()
        };
        config
            .servers
            .insert("prod".to_string(), ServerConfig::default());
        config
            .servers
            .insert("staging".to_string(), ServerConfig::default());

        let (server, name) = config.resolve_server_mut(Some("staging")).unwrap();
        assert_eq!(name.as_deref(), Some("staging"));
        server.default_org = Some("staging-org".to_string());

        assert_eq!(
            config.servers["staging"].default_org.as_deref(),
            Some("staging-org")
        );
        assert!(config.servers["prod"].default_org.is_none());
    }

    #[test]
    fn test_resolve_server_mut_missing_profile_errors() {
        let mut config = GlobalConfig {
            default_server: Some("ghost".to_string()),
            ..GlobalConfig::default()
        };
        assert!(config.resolve_server_mut(None).is_err());
    }

    #[test]
    fn test_default_workspaces_backward_compatibility() {
        // Old configs without default_workspaces should still parse.
        let toml_str = r#"
url = "https://atomic.storage"
default_org = "alice"
"#;
        let parsed: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.default_org.as_deref(), Some("alice"));
        assert!(parsed.default_workspaces.is_empty());
    }

    #[test]
    fn test_global_config_backward_compatibility_without_server() {
        // A TOML string without [server] should still parse correctly
        let toml_str = r#"
default_channel = "main"

[author]
name = "Test User"
email = "test@example.com"
"#;

        let parsed: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.default_channel, "main");
        assert_eq!(parsed.author.name, "Test User");
        // server should be default (not configured)
        assert!(!parsed.server.is_configured());
        assert!(parsed.server.url.is_none());
        assert!(parsed.server.default_org.is_none());
    }

    #[test]
    fn test_org_base_url_single_tenant_not_prefixed() {
        let config = ServerConfig {
            url: Some("https://storage.acme.com".to_string()),
            default_org: Some("acme".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: true,
        };
        // Single-tenant: the bare host is already tenant-scoped — no org prefix.
        assert_eq!(
            config.org_base_url("acme").as_deref(),
            Some("https://storage.acme.com")
        );
        // Any org slug returns the bare host unchanged.
        assert_eq!(
            config.org_base_url("anything-else").as_deref(),
            Some("https://storage.acme.com")
        );
        assert_eq!(
            config.default_org_base_url().as_deref(),
            Some("https://storage.acme.com")
        );
    }

    #[test]
    fn test_org_base_url_single_tenant_strips_trailing_slash() {
        let config = ServerConfig {
            url: Some("https://storage.acme.com/".to_string()),
            default_org: Some("acme".to_string()),
            default_workspaces: BTreeMap::new(),
            identity: None,
            single_tenant: true,
        };
        assert_eq!(
            config.org_base_url("acme").as_deref(),
            Some("https://storage.acme.com")
        );
    }

    #[test]
    fn test_single_tenant_defaults_false_for_legacy_configs() {
        // Configs written before the field existed must deserialize as
        // multi-tenant (org-prefixed) — no migration needed.
        let legacy = r#"url = "https://atomic.storage"
default_org = "alice"
"#;
        let config: ServerConfig = toml::from_str(legacy).unwrap();
        assert!(!config.single_tenant);
        assert_eq!(
            config.org_base_url("alice").as_deref(),
            Some("https://alice.atomic.storage")
        );
    }

    #[test]
    fn global_config_dir_uses_env_override_verbatim() {
        // With the override set, the directory is used exactly as given.
        let override_dir = std::ffi::OsString::from("/some/test/config/dir");
        assert_eq!(
            global_config_dir_from(Some(override_dir)),
            Some(PathBuf::from("/some/test/config/dir"))
        );
    }

    #[test]
    fn global_config_dir_falls_back_to_home_when_unset() {
        // Without the override, it falls back to <home>/.atomic (matching the
        // pre-existing behavior). We only assert the `.atomic` suffix so the
        // test is host-independent.
        if let Some(dir) = global_config_dir_from(None) {
            assert!(
                dir.ends_with(".atomic"),
                "expected <home>/.atomic, got {dir:?}"
            );
        }
    }
}

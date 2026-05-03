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
    /// Can be switched with `atomic org switch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_org: Option<String>,
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
    pub fn org_base_url(&self, org_slug: &str) -> Option<String> {
        let url = self.url.as_ref()?;

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

    /// Remote server configuration for atomic-storage.
    #[serde(default)]
    pub server: ServerConfig,
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

/// Get the global configuration directory
pub fn global_config_dir() -> Option<PathBuf> {
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
        };
        assert!(config.is_configured());

        // Missing org → not configured
        let partial = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: None,
        };
        assert!(!partial.is_configured());

        // Missing url → not configured
        let partial = ServerConfig {
            url: None,
            default_org: Some("alice".to_string()),
        };
        assert!(!partial.is_configured());
    }

    #[test]
    fn test_server_config_org_base_url() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
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
        };
        assert_eq!(
            config.default_org_base_url(),
            Some("https://alice.atomic.storage".to_string())
        );

        // No default org → None
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: None,
        };
        assert!(config.default_org_base_url().is_none());
    }

    #[test]
    fn test_server_config_serialization_roundtrip() {
        let config = ServerConfig {
            url: Some("https://atomic.storage".to_string()),
            default_org: Some("alice".to_string()),
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
}

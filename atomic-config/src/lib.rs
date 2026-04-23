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
}

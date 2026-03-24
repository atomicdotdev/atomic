//! Remote repository configuration management.
//!
//! This module provides types and functions for managing remote repository
//! configurations, allowing users to define named remotes that can be used
//! with push, pull, and other network operations.
//!
//! # Overview
//!
//! Remotes are named references to remote repository URLs. They are stored
//! in the repository's configuration file (`.atomic/config.toml`) and can
//! be managed using the `atomic remote` command.
//!
//! # Example Configuration
//!
//! ```toml
//! [remotes]
//! origin = { url = "https://api.example.com/tenant/portfolio/project/code" }
//! upstream = { url = "https://api.example.com/other/repo/code", default = false }
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use atomic_repository::remote::{RemoteConfig, RemoteEntry};
//!
//! // Load configuration
//! let config = RemoteConfig::load(&repo.config_path())?;
//!
//! // Get a remote by name
//! if let Some(remote) = config.get("origin") {
//!     println!("Origin URL: {}", remote.url);
//! }
//!
//! // Add a new remote
//! let mut config = RemoteConfig::load(&repo.config_path())?;
//! config.add("upstream", RemoteEntry::new("https://example.com/repo"));
//! config.save(&repo.config_path())?;
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

// Error Types

/// Errors that can occur during remote configuration operations.
#[derive(Debug)]
pub enum RemoteError {
    /// The remote already exists.
    AlreadyExists { name: String },

    /// The remote was not found.
    NotFound { name: String },

    /// Invalid remote name.
    InvalidName { name: String, reason: String },

    /// Invalid remote URL.
    InvalidUrl { url: String, reason: String },

    /// Failed to read configuration file.
    ReadError {
        path: String,
        source: std::io::Error,
    },

    /// Failed to write configuration file.
    WriteError {
        path: String,
        source: std::io::Error,
    },

    /// Failed to parse configuration file.
    ParseError { path: String, message: String },

    /// Failed to serialize configuration.
    SerializeError { message: String },

    /// Cannot remove the default remote without setting another default.
    CannotRemoveDefault { name: String },
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists { name } => {
                write!(f, "remote '{}' already exists", name)
            }
            Self::NotFound { name } => {
                write!(f, "remote '{}' not found", name)
            }
            Self::InvalidName { name, reason } => {
                write!(f, "invalid remote name '{}': {}", name, reason)
            }
            Self::InvalidUrl { url, reason } => {
                write!(f, "invalid remote URL '{}': {}", url, reason)
            }
            Self::ReadError { path, source } => {
                write!(f, "failed to read config '{}': {}", path, source)
            }
            Self::WriteError { path, source } => {
                write!(f, "failed to write config '{}': {}", path, source)
            }
            Self::ParseError { path, message } => {
                write!(f, "failed to parse config '{}': {}", path, message)
            }
            Self::SerializeError { message } => {
                write!(f, "failed to serialize config: {}", message)
            }
            Self::CannotRemoveDefault { name } => {
                write!(
                    f,
                    "cannot remove default remote '{}'; set another remote as default first",
                    name
                )
            }
        }
    }
}

impl std::error::Error for RemoteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadError { source, .. } => Some(source),
            Self::WriteError { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Result type for remote operations.
pub type RemoteResult<T> = Result<T, RemoteError>;

// RemoteEntry

/// A single remote repository entry.
///
/// Each remote has a URL and optional metadata like whether it's the default
/// remote for push/pull operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteEntry {
    /// The URL of the remote repository.
    pub url: String,

    /// Whether this is the default remote for push/pull operations.
    ///
    /// Only one remote should be marked as default. If multiple remotes
    /// have `default = true`, the behavior is undefined.
    #[serde(default)]
    pub default: bool,
}

impl RemoteEntry {
    /// Create a new remote entry with the given URL.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the remote repository
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::remote::RemoteEntry;
    ///
    /// let remote = RemoteEntry::new("https://api.example.com/repo");
    /// assert_eq!(remote.url, "https://api.example.com/repo");
    /// assert!(!remote.default);
    /// ```
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            default: false,
        }
    }

    /// Create a new remote entry marked as the default.
    ///
    /// # Arguments
    ///
    /// * `url` - The URL of the remote repository
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::remote::RemoteEntry;
    ///
    /// let remote = RemoteEntry::new_default("https://api.example.com/repo");
    /// assert!(remote.default);
    /// ```
    pub fn new_default(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            default: true,
        }
    }

    /// Set whether this remote is the default.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::remote::RemoteEntry;
    ///
    /// let remote = RemoteEntry::new("https://example.com/repo")
    ///     .with_default(true);
    /// assert!(remote.default);
    /// ```
    pub fn with_default(mut self, default: bool) -> Self {
        self.default = default;
        self
    }

    /// Check if the URL appears valid.
    ///
    /// This performs basic validation that the URL has a scheme.
    pub fn is_valid_url(&self) -> bool {
        self.url.contains("://")
    }
}

impl fmt::Display for RemoteEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.default {
            write!(f, "{} (default)", self.url)
        } else {
            write!(f, "{}", self.url)
        }
    }
}

// RemoteConfig

/// Configuration for all remotes in a repository.
///
/// This struct manages the collection of named remotes and provides
/// methods for adding, removing, and querying remotes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Map of remote names to their configurations.
    ///
    /// Using BTreeMap for deterministic ordering in config file.
    #[serde(default)]
    pub remotes: BTreeMap<String, RemoteEntry>,
}

impl RemoteConfig {
    /// Create a new empty remote configuration.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::remote::RemoteConfig;
    ///
    /// let config = RemoteConfig::new();
    /// assert!(config.is_empty());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Load remote configuration from a config file.
    ///
    /// This reads the TOML configuration file and extracts the `[remotes]`
    /// section. If the file doesn't exist or has no remotes section,
    /// returns an empty configuration.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the config file (typically `.atomic/config.toml`)
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load<P: AsRef<Path>>(path: P) -> RemoteResult<Self> {
        let path = path.as_ref();

        // If file doesn't exist, return empty config
        if !path.exists() {
            return Ok(Self::new());
        }

        let content = std::fs::read_to_string(path).map_err(|e| RemoteError::ReadError {
            path: path.display().to_string(),
            source: e,
        })?;

        Self::parse(&content, path)
    }

    /// Parse remote configuration from TOML content.
    fn parse(content: &str, path: &Path) -> RemoteResult<Self> {
        // Parse the full config file as a table
        let full_config: toml::map::Map<String, toml::Value> =
            toml::from_str(content).map_err(|e| RemoteError::ParseError {
                path: path.display().to_string(),
                message: e.to_string(),
            })?;

        // Extract the remotes section
        let remotes = if let Some(remotes_value) = full_config.get("remotes") {
            // Convert the Value directly to the expected type
            remotes_value
                .clone()
                .try_into::<BTreeMap<String, RemoteEntry>>()
                .map_err(|e| RemoteError::ParseError {
                    path: path.display().to_string(),
                    message: format!("invalid remotes section: {}", e),
                })?
        } else {
            BTreeMap::new()
        };

        Ok(Self { remotes })
    }

    /// Save remote configuration to a config file.
    ///
    /// This preserves other sections in the config file and only updates
    /// the `[remotes]` section.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the config file (typically `.atomic/config.toml`)
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> RemoteResult<()> {
        let path = path.as_ref();

        // Load existing config or create empty
        let mut full_config: toml::map::Map<String, toml::Value> = if path.exists() {
            let content = std::fs::read_to_string(path).map_err(|e| RemoteError::ReadError {
                path: path.display().to_string(),
                source: e,
            })?;
            toml::from_str(&content).map_err(|e| RemoteError::ParseError {
                path: path.display().to_string(),
                message: e.to_string(),
            })?
        } else {
            toml::map::Map::new()
        };

        // Update or remove the remotes section
        if self.remotes.is_empty() {
            full_config.remove("remotes");
        } else {
            let remotes_value =
                toml::Value::try_from(&self.remotes).map_err(|e| RemoteError::SerializeError {
                    message: e.to_string(),
                })?;
            full_config.insert("remotes".to_string(), remotes_value);
        }

        // Serialize and write
        let content =
            toml::to_string_pretty(&full_config).map_err(|e| RemoteError::SerializeError {
                message: e.to_string(),
            })?;

        std::fs::write(path, content).map_err(|e| RemoteError::WriteError {
            path: path.display().to_string(),
            source: e,
        })?;

        Ok(())
    }

    /// Check if there are no remotes configured.
    pub fn is_empty(&self) -> bool {
        self.remotes.is_empty()
    }

    /// Get the number of configured remotes.
    pub fn len(&self) -> usize {
        self.remotes.len()
    }

    /// Get a remote by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to look up
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::remote::{RemoteConfig, RemoteEntry};
    ///
    /// let mut config = RemoteConfig::new();
    /// config.add("origin", RemoteEntry::new("https://example.com/repo")).unwrap();
    ///
    /// assert!(config.get("origin").is_some());
    /// assert!(config.get("nonexistent").is_none());
    /// ```
    pub fn get(&self, name: &str) -> Option<&RemoteEntry> {
        self.remotes.get(name)
    }

    /// Get a mutable reference to a remote by name.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut RemoteEntry> {
        self.remotes.get_mut(name)
    }

    /// Check if a remote with the given name exists.
    pub fn contains(&self, name: &str) -> bool {
        self.remotes.contains_key(name)
    }

    /// Get the default remote, if one is configured.
    ///
    /// Returns the first remote marked as default, or "origin" if it exists
    /// and no other default is set.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::remote::{RemoteConfig, RemoteEntry};
    ///
    /// let mut config = RemoteConfig::new();
    /// config.add("upstream", RemoteEntry::new_default("https://upstream.com/repo")).unwrap();
    /// config.add("origin", RemoteEntry::new("https://origin.com/repo")).unwrap();
    ///
    /// let (name, remote) = config.get_default().unwrap();
    /// assert_eq!(name, "upstream");
    /// ```
    pub fn get_default(&self) -> Option<(&str, &RemoteEntry)> {
        // First, look for an explicitly marked default
        for (name, entry) in &self.remotes {
            if entry.default {
                return Some((name, entry));
            }
        }

        // Fall back to "origin" if it exists
        if let Some(origin) = self.remotes.get("origin") {
            return Some(("origin", origin));
        }

        // Return the first remote if there's only one
        if self.remotes.len() == 1 {
            if let Some((name, entry)) = self.remotes.iter().next() {
                return Some((name, entry));
            }
        }

        None
    }

    /// Add a new remote.
    ///
    /// # Arguments
    ///
    /// * `name` - The name for the new remote
    /// * `entry` - The remote configuration
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A remote with the same name already exists
    /// - The name is invalid
    /// - The URL is invalid
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::remote::{RemoteConfig, RemoteEntry};
    ///
    /// let mut config = RemoteConfig::new();
    /// config.add("origin", RemoteEntry::new("https://example.com/repo")).unwrap();
    /// assert!(config.contains("origin"));
    /// ```
    pub fn add(&mut self, name: &str, entry: RemoteEntry) -> RemoteResult<()> {
        // Validate name
        validate_remote_name(name)?;

        // Validate URL
        if !entry.is_valid_url() {
            return Err(RemoteError::InvalidUrl {
                url: entry.url.clone(),
                reason: "URL must include a scheme (e.g., https://)".to_string(),
            });
        }

        // Check for duplicates
        if self.remotes.contains_key(name) {
            return Err(RemoteError::AlreadyExists {
                name: name.to_string(),
            });
        }

        // If this is marked as default, unmark any existing default
        if entry.default {
            for existing in self.remotes.values_mut() {
                existing.default = false;
            }
        }

        self.remotes.insert(name.to_string(), entry);
        Ok(())
    }

    /// Remove a remote by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to remove
    ///
    /// # Errors
    ///
    /// Returns an error if the remote doesn't exist.
    ///
    /// # Example
    ///
    /// ```rust
    /// use atomic_repository::remote::{RemoteConfig, RemoteEntry};
    ///
    /// let mut config = RemoteConfig::new();
    /// config.add("origin", RemoteEntry::new("https://example.com/repo")).unwrap();
    /// config.remove("origin").unwrap();
    /// assert!(!config.contains("origin"));
    /// ```
    pub fn remove(&mut self, name: &str) -> RemoteResult<RemoteEntry> {
        self.remotes
            .remove(name)
            .ok_or_else(|| RemoteError::NotFound {
                name: name.to_string(),
            })
    }

    /// Update the URL of an existing remote.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to update
    /// * `url` - The new URL
    ///
    /// # Errors
    ///
    /// Returns an error if the remote doesn't exist or the URL is invalid.
    pub fn set_url(&mut self, name: &str, url: impl Into<String>) -> RemoteResult<()> {
        let url = url.into();

        // Validate URL
        if !url.contains("://") {
            return Err(RemoteError::InvalidUrl {
                url,
                reason: "URL must include a scheme (e.g., https://)".to_string(),
            });
        }

        let entry = self
            .remotes
            .get_mut(name)
            .ok_or_else(|| RemoteError::NotFound {
                name: name.to_string(),
            })?;

        entry.url = url;
        Ok(())
    }

    /// Set a remote as the default.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the remote to set as default
    ///
    /// # Errors
    ///
    /// Returns an error if the remote doesn't exist.
    pub fn set_default(&mut self, name: &str) -> RemoteResult<()> {
        // Verify the remote exists
        if !self.remotes.contains_key(name) {
            return Err(RemoteError::NotFound {
                name: name.to_string(),
            });
        }

        // Unmark all remotes as default
        for entry in self.remotes.values_mut() {
            entry.default = false;
        }

        // Mark the specified remote as default
        if let Some(entry) = self.remotes.get_mut(name) {
            entry.default = true;
        }

        Ok(())
    }

    /// Rename a remote.
    ///
    /// # Arguments
    ///
    /// * `old_name` - The current name of the remote
    /// * `new_name` - The new name for the remote
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The old remote doesn't exist
    /// - The new name is invalid
    /// - A remote with the new name already exists
    pub fn rename(&mut self, old_name: &str, new_name: &str) -> RemoteResult<()> {
        // Validate new name
        validate_remote_name(new_name)?;

        // Check old exists
        if !self.remotes.contains_key(old_name) {
            return Err(RemoteError::NotFound {
                name: old_name.to_string(),
            });
        }

        // Check new doesn't exist
        if self.remotes.contains_key(new_name) {
            return Err(RemoteError::AlreadyExists {
                name: new_name.to_string(),
            });
        }

        // Perform the rename
        if let Some(entry) = self.remotes.remove(old_name) {
            self.remotes.insert(new_name.to_string(), entry);
        }

        Ok(())
    }

    /// Iterate over all remotes.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &RemoteEntry)> {
        self.remotes.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Get all remote names.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.remotes.keys().map(|s| s.as_str())
    }
}

// Helper Functions

/// Validate a remote name.
///
/// Remote names must:
/// - Not be empty
/// - Contain only alphanumeric characters, hyphens, and underscores
/// - Not start with a hyphen
/// - Not be a reserved name (like "." or "..")
fn validate_remote_name(name: &str) -> RemoteResult<()> {
    if name.is_empty() {
        return Err(RemoteError::InvalidName {
            name: name.to_string(),
            reason: "name cannot be empty".to_string(),
        });
    }

    if name == "." || name == ".." {
        return Err(RemoteError::InvalidName {
            name: name.to_string(),
            reason: "name cannot be '.' or '..'".to_string(),
        });
    }

    if name.starts_with('-') {
        return Err(RemoteError::InvalidName {
            name: name.to_string(),
            reason: "name cannot start with a hyphen".to_string(),
        });
    }

    for ch in name.chars() {
        if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' {
            return Err(RemoteError::InvalidName {
                name: name.to_string(),
                reason: format!(
                    "name contains invalid character '{}'; only alphanumeric, hyphen, and underscore are allowed",
                    ch
                ),
            });
        }
    }

    Ok(())
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // RemoteEntry Tests

    #[test]
    fn test_remote_entry_new() {
        let entry = RemoteEntry::new("https://example.com/repo");
        assert_eq!(entry.url, "https://example.com/repo");
        assert!(!entry.default);
    }

    #[test]
    fn test_remote_entry_new_default() {
        let entry = RemoteEntry::new_default("https://example.com/repo");
        assert_eq!(entry.url, "https://example.com/repo");
        assert!(entry.default);
    }

    #[test]
    fn test_remote_entry_with_default() {
        let entry = RemoteEntry::new("https://example.com/repo").with_default(true);
        assert!(entry.default);

        let entry = entry.with_default(false);
        assert!(!entry.default);
    }

    #[test]
    fn test_remote_entry_is_valid_url() {
        assert!(RemoteEntry::new("https://example.com/repo").is_valid_url());
        assert!(RemoteEntry::new("http://localhost:3000/repo").is_valid_url());
        assert!(!RemoteEntry::new("example.com/repo").is_valid_url());
        assert!(!RemoteEntry::new("origin").is_valid_url());
    }

    #[test]
    fn test_remote_entry_display() {
        let entry = RemoteEntry::new("https://example.com/repo");
        assert_eq!(format!("{}", entry), "https://example.com/repo");

        let entry = RemoteEntry::new_default("https://example.com/repo");
        assert_eq!(format!("{}", entry), "https://example.com/repo (default)");
    }

    #[test]
    fn test_remote_entry_serialize() {
        let entry = RemoteEntry::new_default("https://example.com/repo");
        let toml_str = toml::to_string(&entry).unwrap();
        assert!(toml_str.contains("url = \"https://example.com/repo\""));
        assert!(toml_str.contains("default = true"));
    }

    #[test]
    fn test_remote_entry_deserialize() {
        let toml_str = r#"url = "https://example.com/repo"
default = true"#;
        let entry: RemoteEntry = toml::from_str(toml_str).unwrap();
        assert_eq!(entry.url, "https://example.com/repo");
        assert!(entry.default);
    }

    #[test]
    fn test_remote_entry_deserialize_minimal() {
        let toml_str = r#"url = "https://example.com/repo""#;
        let entry: RemoteEntry = toml::from_str(toml_str).unwrap();
        assert_eq!(entry.url, "https://example.com/repo");
        assert!(!entry.default); // default is false when not specified
    }

    // RemoteConfig Tests

    #[test]
    fn test_remote_config_new() {
        let config = RemoteConfig::new();
        assert!(config.is_empty());
        assert_eq!(config.len(), 0);
    }

    #[test]
    fn test_remote_config_add() {
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://example.com/repo"))
            .unwrap();

        assert!(!config.is_empty());
        assert_eq!(config.len(), 1);
        assert!(config.contains("origin"));
    }

    #[test]
    fn test_remote_config_add_duplicate() {
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://example.com/repo"))
            .unwrap();

        let result = config.add("origin", RemoteEntry::new("https://other.com/repo"));
        assert!(matches!(result, Err(RemoteError::AlreadyExists { .. })));
    }

    #[test]
    fn test_remote_config_add_invalid_name() {
        let mut config = RemoteConfig::new();

        assert!(matches!(
            config.add("", RemoteEntry::new("https://example.com/repo")),
            Err(RemoteError::InvalidName { .. })
        ));

        assert!(matches!(
            config.add("-origin", RemoteEntry::new("https://example.com/repo")),
            Err(RemoteError::InvalidName { .. })
        ));

        assert!(matches!(
            config.add("ori gin", RemoteEntry::new("https://example.com/repo")),
            Err(RemoteError::InvalidName { .. })
        ));
    }

    #[test]
    fn test_remote_config_add_invalid_url() {
        let mut config = RemoteConfig::new();

        let result = config.add("origin", RemoteEntry::new("not-a-url"));
        assert!(matches!(result, Err(RemoteError::InvalidUrl { .. })));
    }

    #[test]
    fn test_remote_config_get() {
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://example.com/repo"))
            .unwrap();

        let entry = config.get("origin").unwrap();
        assert_eq!(entry.url, "https://example.com/repo");

        assert!(config.get("nonexistent").is_none());
    }

    #[test]
    fn test_remote_config_remove() {
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://example.com/repo"))
            .unwrap();

        let removed = config.remove("origin").unwrap();
        assert_eq!(removed.url, "https://example.com/repo");
        assert!(!config.contains("origin"));
    }

    #[test]
    fn test_remote_config_remove_nonexistent() {
        let mut config = RemoteConfig::new();
        let result = config.remove("nonexistent");
        assert!(matches!(result, Err(RemoteError::NotFound { .. })));
    }

    #[test]
    fn test_remote_config_set_url() {
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://example.com/repo"))
            .unwrap();

        config.set_url("origin", "https://new.com/repo").unwrap();
        assert_eq!(config.get("origin").unwrap().url, "https://new.com/repo");
    }

    #[test]
    fn test_remote_config_set_default() {
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://origin.com/repo"))
            .unwrap();
        config
            .add("upstream", RemoteEntry::new("https://upstream.com/repo"))
            .unwrap();

        config.set_default("upstream").unwrap();

        assert!(!config.get("origin").unwrap().default);
        assert!(config.get("upstream").unwrap().default);
    }

    #[test]
    fn test_remote_config_get_default_explicit() {
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://origin.com/repo"))
            .unwrap();
        config
            .add(
                "upstream",
                RemoteEntry::new_default("https://upstream.com/repo"),
            )
            .unwrap();

        let (name, _) = config.get_default().unwrap();
        assert_eq!(name, "upstream");
    }

    #[test]
    fn test_remote_config_get_default_origin_fallback() {
        let mut config = RemoteConfig::new();
        config
            .add("upstream", RemoteEntry::new("https://upstream.com/repo"))
            .unwrap();
        config
            .add("origin", RemoteEntry::new("https://origin.com/repo"))
            .unwrap();

        let (name, _) = config.get_default().unwrap();
        assert_eq!(name, "origin");
    }

    #[test]
    fn test_remote_config_get_default_single() {
        let mut config = RemoteConfig::new();
        config
            .add("my-remote", RemoteEntry::new("https://example.com/repo"))
            .unwrap();

        let (name, _) = config.get_default().unwrap();
        assert_eq!(name, "my-remote");
    }

    #[test]
    fn test_remote_config_get_default_none() {
        let config = RemoteConfig::new();
        assert!(config.get_default().is_none());
    }

    #[test]
    fn test_remote_config_rename() {
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://example.com/repo"))
            .unwrap();

        config.rename("origin", "upstream").unwrap();

        assert!(!config.contains("origin"));
        assert!(config.contains("upstream"));
        assert_eq!(
            config.get("upstream").unwrap().url,
            "https://example.com/repo"
        );
    }

    #[test]
    fn test_remote_config_rename_nonexistent() {
        let mut config = RemoteConfig::new();
        let result = config.rename("nonexistent", "new");
        assert!(matches!(result, Err(RemoteError::NotFound { .. })));
    }

    #[test]
    fn test_remote_config_rename_to_existing() {
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://origin.com/repo"))
            .unwrap();
        config
            .add("upstream", RemoteEntry::new("https://upstream.com/repo"))
            .unwrap();

        let result = config.rename("origin", "upstream");
        assert!(matches!(result, Err(RemoteError::AlreadyExists { .. })));
    }

    #[test]
    fn test_remote_config_iter() {
        let mut config = RemoteConfig::new();
        config
            .add("a", RemoteEntry::new("https://a.com/repo"))
            .unwrap();
        config
            .add("b", RemoteEntry::new("https://b.com/repo"))
            .unwrap();

        let names: Vec<&str> = config.iter().map(|(n, _)| n).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn test_remote_config_names() {
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://origin.com/repo"))
            .unwrap();
        config
            .add("upstream", RemoteEntry::new("https://upstream.com/repo"))
            .unwrap();

        let names: Vec<&str> = config.names().collect();
        assert_eq!(names.len(), 2);
    }

    // File I/O Tests

    #[test]
    fn test_remote_config_load_nonexistent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = RemoteConfig::load(&path).unwrap();
        assert!(config.is_empty());
    }

    #[test]
    fn test_remote_config_save_and_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut config = RemoteConfig::new();
        config
            .add(
                "origin",
                RemoteEntry::new_default("https://origin.com/repo"),
            )
            .unwrap();
        config
            .add("upstream", RemoteEntry::new("https://upstream.com/repo"))
            .unwrap();

        config.save(&path).unwrap();

        let loaded = RemoteConfig::load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.get("origin").unwrap().default);
        assert!(!loaded.get("upstream").unwrap().default);
    }

    #[test]
    fn test_remote_config_save_preserves_other_sections() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // Create initial config with other sections
        let initial = r#"# Atomic repository configuration

[stack]
default = "dev"

[other]
key = "value"
"#;
        std::fs::write(&path, initial).unwrap();

        // Add remotes
        let mut config = RemoteConfig::load(&path).unwrap();
        config
            .add("origin", RemoteEntry::new("https://example.com/repo"))
            .unwrap();
        config.save(&path).unwrap();

        // Verify other sections are preserved
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("[stack]"));
        assert!(content.contains("[other]"));
        assert!(content.contains("[remotes.origin]") || content.contains("[remotes]"));
    }

    #[test]
    fn test_remote_config_save_removes_empty_remotes_section() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // Create config with remotes
        let mut config = RemoteConfig::new();
        config
            .add("origin", RemoteEntry::new("https://example.com/repo"))
            .unwrap();
        config.save(&path).unwrap();

        // Remove all remotes
        config.remove("origin").unwrap();
        config.save(&path).unwrap();

        // Verify remotes section is removed
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("[remotes]"));
    }

    // Validation Tests

    #[test]
    fn test_validate_remote_name_valid() {
        assert!(validate_remote_name("origin").is_ok());
        assert!(validate_remote_name("my-remote").is_ok());
        assert!(validate_remote_name("my_remote").is_ok());
        assert!(validate_remote_name("remote123").is_ok());
        assert!(validate_remote_name("a").is_ok());
    }

    #[test]
    fn test_validate_remote_name_invalid() {
        assert!(validate_remote_name("").is_err());
        assert!(validate_remote_name(".").is_err());
        assert!(validate_remote_name("..").is_err());
        assert!(validate_remote_name("-origin").is_err());
        assert!(validate_remote_name("origin/test").is_err());
        assert!(validate_remote_name("origin test").is_err());
        assert!(validate_remote_name("origin@test").is_err());
    }

    // Error Display Tests

    #[test]
    fn test_remote_error_display() {
        let err = RemoteError::AlreadyExists {
            name: "origin".to_string(),
        };
        assert!(err.to_string().contains("origin"));
        assert!(err.to_string().contains("already exists"));

        let err = RemoteError::NotFound {
            name: "origin".to_string(),
        };
        assert!(err.to_string().contains("origin"));
        assert!(err.to_string().contains("not found"));

        let err = RemoteError::InvalidName {
            name: "bad name".to_string(),
            reason: "contains space".to_string(),
        };
        assert!(err.to_string().contains("bad name"));
        assert!(err.to_string().contains("contains space"));
    }

    #[test]
    fn test_add_default_unmarks_existing() {
        let mut config = RemoteConfig::new();
        config
            .add(
                "origin",
                RemoteEntry::new_default("https://origin.com/repo"),
            )
            .unwrap();
        config
            .add(
                "upstream",
                RemoteEntry::new_default("https://upstream.com/repo"),
            )
            .unwrap();

        // Adding a new default should unmark the old one
        assert!(!config.get("origin").unwrap().default);
        assert!(config.get("upstream").unwrap().default);
    }
}

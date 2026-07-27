//! Installation receipts: what was installed, where from, and which files
//! are ours. Receipts drive uninstall and protect user-modified files on
//! reinstall.
//!
//! Layout under the user's home:
//!
//! ```text
//! ~/.atomic/integrations/
//! └── <agent>/
//!     ├── repo/          ← CLI-managed clone of the package (sync cache)
//!     └── receipt.json   ← written by install.rs
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AgentError, AgentResult};

const RECEIPT_FILE: &str = "receipt.json";
const RECEIPT_SCHEMA: u32 = 1;

/// Root directory for all integration state: `~/.atomic/integrations`.
///
/// `ATOMIC_INTEGRATIONS_HOME` overrides the root — used by tests and by
/// sandboxes that must not touch the real home directory.
pub fn integrations_root() -> AgentResult<PathBuf> {
    if let Some(root) = std::env::var_os("ATOMIC_INTEGRATIONS_HOME") {
        return Ok(PathBuf::from(root));
    }
    dirs::home_dir()
        .map(|h| h.join(".atomic").join("integrations"))
        .ok_or_else(|| AgentError::Integration {
            agent: "<any>".to_string(),
            reason: "cannot resolve home directory".to_string(),
        })
}

/// Per-agent integration directory: `~/.atomic/integrations/<agent>`.
pub fn agent_dir(agent: &str) -> AgentResult<PathBuf> {
    Ok(integrations_root()?.join(agent))
}

/// Where the CLI clones/syncs the package repository for an agent.
pub fn cache_repo_dir(agent: &str) -> AgentResult<PathBuf> {
    Ok(agent_dir(agent)?.join("repo"))
}

/// Path to an agent's receipt file.
pub fn receipt_path(agent: &str) -> AgentResult<PathBuf> {
    Ok(agent_dir(agent)?.join(RECEIPT_FILE))
}

/// One installed file and its content hash at install time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptFile {
    /// Absolute destination the file was copied to.
    pub dst: PathBuf,
    /// Blake3 hex digest of the content we installed. Used to distinguish
    /// "our file, safe to refresh/remove" from "user modified, hands off".
    pub blake3: String,
}

/// A settings file that was merged via a hooks manifest, recorded so
/// uninstall can remove our entries without the original manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiptSettings {
    /// Resolved settings file that was merged into.
    pub target: PathBuf,
    /// JSON key under which hooks live in the target.
    pub hooks_key: String,
    /// Substring identifying this integration's own hook commands.
    pub command_prefix: String,
}

/// Record of one integration install.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    /// Receipt schema version.
    pub schema: u32,
    /// Adapter name that was installed.
    pub agent: String,
    /// Package version from the manifest.
    pub version: String,
    /// CLI version that performed the install.
    pub cli_version: String,
    /// RFC3339 install timestamp.
    pub installed_at: String,
    /// Where the package came from (storage URL or local path).
    pub source: String,
    /// Files copied into place.
    pub files: Vec<ReceiptFile>,
    /// Settings files merged into.
    #[serde(default)]
    pub settings: Vec<ReceiptSettings>,
}

impl Receipt {
    /// Create a receipt for a just-completed install.
    pub fn new(
        agent: &str,
        version: &str,
        cli_version: &str,
        source: &str,
        files: Vec<ReceiptFile>,
        settings: Vec<ReceiptSettings>,
    ) -> Self {
        Self {
            schema: RECEIPT_SCHEMA,
            agent: agent.to_string(),
            version: version.to_string(),
            cli_version: cli_version.to_string(),
            installed_at: chrono::Utc::now().to_rfc3339(),
            source: source.to_string(),
            files,
            settings,
        }
    }

    /// Persist the receipt under `~/.atomic/integrations/<agent>/`.
    pub fn save(&self) -> AgentResult<PathBuf> {
        let path = receipt_path(&self.agent)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, format!("{json}\n"))?;
        Ok(path)
    }

    /// Load an agent's receipt, if one exists.
    pub fn load(agent: &str) -> AgentResult<Option<Self>> {
        let path = receipt_path(agent)?;
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)?;
        let receipt: Receipt =
            serde_json::from_str(&text).map_err(|e| AgentError::Integration {
                agent: agent.to_string(),
                reason: format!("corrupt receipt at {}: {}", path.display(), e),
            })?;
        Ok(Some(receipt))
    }

    /// Delete the receipt file. Returns `true` if one existed.
    pub fn remove(agent: &str) -> AgentResult<bool> {
        let path = receipt_path(agent)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
            // Best-effort cleanup of the per-agent dir if now empty.
            if let Ok(dir) = agent_dir(agent) {
                let _ = std::fs::remove_dir(&dir);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Look up the hash we recorded for a destination, if we installed it.
    pub fn recorded_hash(&self, dst: &Path) -> Option<&str> {
        self.files
            .iter()
            .find(|f| f.dst == dst)
            .map(|f| f.blake3.as_str())
    }
}

/// Blake3 hex digest of a file's contents.
pub fn hash_file(path: &Path) -> AgentResult<String> {
    let bytes = std::fs::read(path)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_roundtrip() {
        let receipt = Receipt::new(
            "opencode",
            "1.0.0",
            "0.11.1",
            "/tmp/pkg",
            vec![ReceiptFile {
                dst: PathBuf::from("/home/u/.config/opencode/agents/atomic.md"),
                blake3: "abc123".to_string(),
            }],
            vec![ReceiptSettings {
                target: PathBuf::from("/home/u/.config/opencode/opencode.json"),
                hooks_key: "hooks".to_string(),
                command_prefix: "atomic agent hooks opencode".to_string(),
            }],
        );
        let json = serde_json::to_string_pretty(&receipt).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent, "opencode");
        assert_eq!(back.schema, RECEIPT_SCHEMA);
        assert_eq!(back.files.len(), 1);
        assert_eq!(back.settings.len(), 1);
        assert_eq!(
            back.recorded_hash(Path::new("/home/u/.config/opencode/agents/atomic.md")),
            Some("abc123")
        );
        assert_eq!(back.recorded_hash(Path::new("/elsewhere")), None);
    }

    #[test]
    fn receipt_without_settings_deserializes() {
        // Older receipts may lack the settings field.
        let json = r#"{
            "schema": 1, "agent": "x", "version": "1.0.0",
            "cli_version": "0.11.0", "installed_at": "2026-01-01T00:00:00Z",
            "source": "test", "files": []
        }"#;
        let receipt: Receipt = serde_json::from_str(json).unwrap();
        assert!(receipt.settings.is_empty());
    }
}

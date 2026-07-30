//! The `atomic-integration.toml` package manifest.
//!
//! Each integration package ships this manifest at its root. It is the
//! producer-side contract that lets the CLI install the package without
//! executing anything from it: what files go where, which settings manifests
//! to merge, and which CLI versions the package works with.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};

/// Filename of the manifest at the package root.
pub const MANIFEST_FILE: &str = "atomic-integration.toml";

/// The only schema version this CLI understands.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// Parsed `atomic-integration.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct IntegrationManifest {
    /// Manifest schema version.
    pub schema: u32,
    /// Adapter name this package integrates (e.g. `"opencode"`).
    pub agent: String,
    /// Package version (display / receipt purposes).
    pub version: String,
    /// Version gates.
    #[serde(default)]
    pub requires: Requires,
    /// Files to copy into place.
    #[serde(default, rename = "file")]
    pub files: Vec<FileEntry>,
    /// Hooks-manifest files to merge via the hooks::manifest engine.
    #[serde(default, rename = "settings")]
    pub settings: Vec<SettingsEntry>,
}

/// Version requirements the package imposes on its host.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Requires {
    /// Semver requirement on the atomic CLI, e.g. `">=0.11.0"`.
    pub atomic: Option<String>,
}

/// One file to copy from the package into the agent's config location.
#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    /// Path relative to the package root.
    pub src: String,
    /// Absolute destination (`~` expands to the user's home).
    pub dst: String,
}

/// One hooks manifest to merge into the agent's settings file.
#[derive(Debug, Clone, Deserialize)]
pub struct SettingsEntry {
    /// Path to a hooks manifest (hooks::manifest format), relative to the
    /// package root.
    pub manifest: String,
}

impl IntegrationManifest {
    /// Load and validate the manifest from a package directory.
    pub fn load(pkg_dir: &Path) -> AgentResult<Self> {
        let path = pkg_dir.join(MANIFEST_FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| AgentError::Integration {
            agent: agent_name_guess(pkg_dir),
            reason: format!("cannot read {}: {}", path.display(), e),
        })?;
        let manifest: IntegrationManifest =
            toml::from_str(&text).map_err(|e| AgentError::Integration {
                agent: agent_name_guess(pkg_dir),
                reason: format!("invalid {}: {}", MANIFEST_FILE, e),
            })?;
        manifest.check_schema()?;
        Ok(manifest)
    }

    /// Reject manifests from a newer schema than this CLI understands.
    pub fn check_schema(&self) -> AgentResult<()> {
        if self.schema != SUPPORTED_SCHEMA {
            return Err(AgentError::Integration {
                agent: self.agent.clone(),
                reason: format!(
                    "unsupported manifest schema {} (this CLI understands schema {})",
                    self.schema, SUPPORTED_SCHEMA
                ),
            });
        }
        Ok(())
    }

    /// Enforce the package's `requires.atomic` gate against the running CLI.
    pub fn check_cli_version(&self, cli_version: &str) -> AgentResult<()> {
        let Some(req_str) = self.requires.atomic.as_deref() else {
            return Ok(());
        };
        let req = semver::VersionReq::parse(req_str).map_err(|e| AgentError::Integration {
            agent: self.agent.clone(),
            reason: format!("invalid requires.atomic '{req_str}': {e}"),
        })?;
        let current = semver::Version::parse(cli_version).map_err(|e| AgentError::Integration {
            agent: self.agent.clone(),
            reason: format!("cannot parse CLI version '{cli_version}': {e}"),
        })?;
        if !req.matches(&current) {
            return Err(AgentError::IntegrationVersionMismatch {
                agent: self.agent.clone(),
                requires: req_str.to_string(),
                current: cli_version.to_string(),
            });
        }
        Ok(())
    }

    /// Absolute paths of the settings manifests, resolved against the package.
    pub fn settings_manifest_paths(&self, pkg_dir: &Path) -> Vec<PathBuf> {
        self.settings
            .iter()
            .map(|s| pkg_dir.join(&s.manifest))
            .collect()
    }
}

fn agent_name_guess(pkg_dir: &Path) -> String {
    pkg_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &Path, body: &str) {
        std::fs::write(dir.join(MANIFEST_FILE), body).unwrap();
    }

    #[test]
    fn loads_minimal_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(
            tmp.path(),
            r#"
schema = 1
agent = "opencode"
version = "1.0.0"
"#,
        );
        let m = IntegrationManifest::load(tmp.path()).unwrap();
        assert_eq!(m.agent, "opencode");
        assert_eq!(m.version, "1.0.0");
        assert!(m.files.is_empty());
        assert!(m.settings.is_empty());
        assert!(m.requires.atomic.is_none());
    }

    #[test]
    fn loads_full_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(
            tmp.path(),
            r#"
schema = 1
agent = "opencode"
version = "1.2.0"

[requires]
atomic = ">=0.11.0"

[[file]]
src = "plugins/atomic-hooks.ts"
dst = "~/.config/opencode/plugins/atomic-hooks.ts"

[[file]]
src = "agents/atomic.md"
dst = "~/.config/opencode/agents/atomic.md"

[[settings]]
manifest = "hooks/opencode.atomic-hooks.json"
"#,
        );
        let m = IntegrationManifest::load(tmp.path()).unwrap();
        assert_eq!(m.files.len(), 2);
        assert_eq!(m.files[0].src, "plugins/atomic-hooks.ts");
        assert_eq!(m.settings.len(), 1);
        let paths = m.settings_manifest_paths(tmp.path());
        assert_eq!(
            paths,
            vec![tmp.path().join("hooks/opencode.atomic-hooks.json")]
        );
    }

    #[test]
    fn rejects_newer_schema() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(
            tmp.path(),
            "schema = 2\nagent = \"x\"\nversion = \"1.0.0\"\n",
        );
        let err = IntegrationManifest::load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("unsupported manifest schema 2"));
    }

    #[test]
    fn cli_version_gate_accepts_matching() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(
            tmp.path(),
            "schema = 1\nagent = \"x\"\nversion = \"1.0.0\"\n\n[requires]\natomic = \">=0.11.0\"\n",
        );
        let m = IntegrationManifest::load(tmp.path()).unwrap();
        assert!(m.check_cli_version("0.11.1").is_ok());
        assert!(m.check_cli_version("0.12.0").is_ok());
    }

    #[test]
    fn cli_version_gate_rejects_old_cli() {
        let tmp = tempfile::tempdir().unwrap();
        write_manifest(
            tmp.path(),
            "schema = 1\nagent = \"x\"\nversion = \"1.0.0\"\n\n[requires]\natomic = \">=0.11.0\"\n",
        );
        let m = IntegrationManifest::load(tmp.path()).unwrap();
        let err = m.check_cli_version("0.10.0").unwrap_err();
        assert!(matches!(err, AgentError::IntegrationVersionMismatch { .. }));
        assert!(err.to_string().contains("requires atomic >=0.11.0"));
    }

    #[test]
    fn missing_manifest_is_an_integration_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = IntegrationManifest::load(tmp.path()).unwrap_err();
        assert!(matches!(err, AgentError::Integration { .. }));
        assert!(err.to_string().contains(MANIFEST_FILE));
    }
}

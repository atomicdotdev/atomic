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
    /// Files to copy into place (from the package itself).
    #[serde(default, rename = "file")]
    pub files: Vec<FileEntry>,
    /// Hooks-manifest files to merge via the hooks::manifest engine.
    #[serde(default, rename = "settings")]
    pub settings: Vec<SettingsEntry>,
    /// Shared skills source — names another registry package (typically
    /// `"atomic-skills"`) whose cache provides the `[[skill]]` src paths and
    /// the `[[agent-definition]]` body. When set, the installer expects
    /// `opts.skills_cache_dir` to point at that package's cloned cache.
    #[serde(default, rename = "skills-source")]
    pub skills_source: Option<SkillsSource>,
    /// Auto-install all skills from the cache using a dst pattern. When
    /// present, the installer globs `skills/*/SKILL.md` in the cache and
    /// copies each to `dst_pattern` with `{name}` replaced by the skill
    /// directory name. This means adding a new skill to `atomic-skills`
    /// requires zero plugin manifest changes.
    #[serde(default, rename = "skills")]
    pub skills_config: Option<SkillsConfig>,
    /// Explicit skill entries for non-skill cache files (e.g. AGENTS.md
    /// copied to a vendor-specific instruction-file path). For actual skills,
    /// prefer `[skills]` with `install = "all"` — it's automatic.
    #[serde(default, rename = "skill")]
    pub skills: Vec<SkillEntry>,
    /// Skills declared by the atomic-skills package itself. Each entry is a
    /// skill name (e.g. `"atomic-vault"`). When a plugin's `[skills]` block
    /// has `install = "all"`, the installer loads the atomic-skills manifest
    /// from the cache and iterates this list. The skill file is always at
    /// `skills/{name}/SKILL.md` — convention over configuration.
    #[serde(default, rename = "declared-skill")]
    pub declared_skills: Vec<DeclaredSkill>,
    /// Files to install into the *repository* root (not the user's home).
    /// Used for the canonical AGENTS.md so the workflow is always-on without
    /// picking a bundled agent. `dst` is relative to the repo root. Only
    /// installed when the caller passes `opts.repo_root` (gated by the
    /// `--repo-agents` prompt in the CLI).
    #[serde(default, rename = "repo-file")]
    pub repo_files: Vec<RepoFileEntry>,
    /// Declares that the plugin bundles a custom agent definition (e.g.
    /// opencode's `agents/atomic.md`). The agent body is stitched at install
    /// time from `src` (frontmatter, in the plugin package) + the canonical
    /// AGENTS.md body (from the skills cache), and written to `slot`.
    #[serde(default, rename = "agent-definition")]
    pub agent_definition: Option<AgentDefinition>,
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

/// Names the shared skills package that provides skill content + AGENTS.md.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillsSource {
    /// Registry agent name (e.g. `"atomic-skills"`). Resolved via the
    /// embedded registry.toml to a storage URL + view, cloned into the
    /// shared cache by the CLI, and passed as `opts.skills_cache_dir`.
    pub package: String,
}

/// Auto-install all skills from the cache using a dst pattern.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillsConfig {
    /// Which skills to install. `"all"` globs `skills/*/SKILL.md` in the
    /// cache and installs every one. Alternatively, a comma-separated list
    /// of skill directory names (e.g. `"atomic-vault,atomic-vcs"`).
    pub install: String,
    /// Destination pattern with `{name}` placeholder. The installer replaces
    /// `{name}` with each skill's directory name. Examples:
    /// - Nested: `"~/.config/opencode/skills/{name}/SKILL.md"`
    /// - Flat: `"~/Documents/Cline/Workflows/{name}.md"`
    pub dst_pattern: String,
}

/// One skill to copy from the skills-source cache.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillEntry {
    /// Path relative to the skills-source cache root (e.g.
    /// `"skills/atomic-vault/SKILL.md"`).
    pub src: String,
    /// Destination (`~` expands to home). Vendors with flat skill layouts
    /// (Cline, agy) use a flat filename here.
    pub dst: String,
}

/// A skill declared by the atomic-skills package. The installer copies
/// `skills/{name}/SKILL.md` from the cache to the plugin's formatted
/// `dst_pattern`.
#[derive(Debug, Clone, Deserialize)]
pub struct DeclaredSkill {
    /// Skill directory name (e.g. `"atomic-vault"`). The file is at
    /// `skills/{name}/SKILL.md` in the atomic-skills cache.
    pub name: String,
}

/// One file to install into the repository root.
#[derive(Debug, Clone, Deserialize)]
pub struct RepoFileEntry {
    /// Path relative to the skills-source cache root (e.g. `"AGENTS.md"`).
    pub src: String,
    /// Destination relative to the repo root (e.g. `"AGENTS.md"`).
    pub dst: String,
}

/// A bundled custom agent definition, stitched at install time.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentDefinition {
    /// Frontmatter template file, relative to the plugin package root (e.g.
    /// `"agents/atomic.md.frontmatter"`). Contains the YAML frontmatter block
    /// and any vendor-specific preamble.
    pub src: String,
    /// Reference to the canonical body, in the form
    /// `"atomic-skills:AGENTS.md"` (package-name-relative path). The
    /// installer reads this from the skills cache.
    pub body_from: String,
    /// Destination in the vendor's agent slot (`~` expands to home).
    pub slot: String,
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

    /// Resolve a `body_from` reference (`"atomic-skills:AGENTS.md"`) into an
    /// absolute path inside the skills cache. Returns `None` if the format is
    /// unrecognized.
    pub fn resolve_body_from(&self, body_from: &str, skills_cache: &Path) -> Option<PathBuf> {
        let (pkg, rel) = body_from.split_once(':')?;
        // The package name in body_from must match the skills-source package.
        let expected = self.skills_source.as_ref()?.package.as_str();
        if pkg != expected {
            return None;
        }
        Some(skills_cache.join(rel))
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

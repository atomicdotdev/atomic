//! The integration installer: copy files, merge settings, write a receipt.
//!
//! Two rules keep this safe to run against a user's real config:
//!
//! 1. **Never execute the package.** Files are copied and JSON is merged via
//!    the `hooks::manifest` engine. No shell scripts, no postinstall.
//! 2. **Never clobber user files.** A destination we did not install (per the
//!    receipt), or one the user modified since we installed it, is skipped —
//!    unless `force` is set.

use std::path::{Path, PathBuf};

use crate::error::{AgentError, AgentResult};
use crate::hooks::manifest as hooks_manifest;
use crate::hooks::manifest::ManifestOutcome;

use super::manifest::IntegrationManifest;
use super::receipt::{hash_file, Receipt, ReceiptFile, ReceiptSettings};

/// Options controlling an install run.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Overwrite files even when they appear user-owned or user-modified.
    pub force: bool,
    /// Running CLI version, checked against the package's `requires.atomic`.
    pub cli_version: String,
    /// Where the package came from (storage URL or local path), recorded in
    /// the receipt.
    pub source: String,
    /// When set, the package manifest's `agent` field must match — guards
    /// against installing the wrong package with `--from`.
    pub expect_agent: Option<String>,
}

/// Why a destination file was left untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// File exists and was not installed by us (not in the receipt).
    UserFile,
    /// File was installed by us but the user modified it since.
    UserModified,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::UserFile => write!(f, "user file, not overwriting"),
            SkipReason::UserModified => write!(f, "modified since install, not overwriting"),
        }
    }
}

/// A destination that was left in place.
#[derive(Debug, Clone)]
pub struct SkippedFile {
    /// The destination left untouched.
    pub dst: PathBuf,
    /// Why it was skipped.
    pub reason: SkipReason,
}

/// What an install did.
#[derive(Debug)]
pub struct InstallOutcome {
    /// Adapter name from the package manifest.
    pub agent: String,
    /// Package version from the manifest.
    pub version: String,
    /// Files newly created.
    pub installed: Vec<PathBuf>,
    /// Files we had installed before and refreshed with new content.
    pub refreshed: Vec<PathBuf>,
    /// Files left untouched (user-owned or user-modified, without `force`).
    pub skipped: Vec<SkippedFile>,
    /// Settings merges performed.
    pub settings: Vec<ManifestOutcome>,
    /// Where the receipt was written.
    pub receipt_path: PathBuf,
}

/// What an uninstall did.
#[derive(Debug)]
pub struct UninstallOutcome {
    /// Adapter name that was removed.
    pub agent: String,
    /// Files deleted (content matched the receipt).
    pub removed: Vec<PathBuf>,
    /// Files kept because the user modified them after install.
    pub kept_modified: Vec<PathBuf>,
    /// Hook commands removed from settings files.
    pub settings_hooks_removed: usize,
}

/// Install an integration package from a local directory (a synced cache
/// clone or a `--from` checkout).
pub fn install_from_dir(pkg_dir: &Path, opts: &InstallOptions) -> AgentResult<InstallOutcome> {
    let manifest = IntegrationManifest::load(pkg_dir)?;

    if let Some(expect) = &opts.expect_agent {
        if &manifest.agent != expect {
            return Err(AgentError::Integration {
                agent: expect.clone(),
                reason: format!(
                    "package at {} is for agent '{}'",
                    pkg_dir.display(),
                    manifest.agent
                ),
            });
        }
    }

    manifest.check_cli_version(&opts.cli_version)?;

    let previous = Receipt::load(&manifest.agent)?;

    let mut installed = Vec::new();
    let mut refreshed = Vec::new();
    let mut skipped = Vec::new();
    let mut receipt_files = Vec::new();

    for entry in &manifest.files {
        reject_parent_traversal(&manifest.agent, &entry.src)?;
        let src = pkg_dir.join(&entry.src);
        if !src.is_file() {
            return Err(AgentError::Integration {
                agent: manifest.agent.clone(),
                reason: format!("manifest file source missing: {}", src.display()),
            });
        }
        let dst = expand_dst(&entry.dst)?;

        match install_decision(&dst, previous.as_ref(), opts.force)? {
            Decision::Install => installed.push(dst.clone()),
            Decision::Refresh => refreshed.push(dst.clone()),
            Decision::Skip(reason) => {
                skipped.push(SkippedFile {
                    dst: dst.clone(),
                    reason,
                });
                // Keep protecting this file: preserve any prior receipt entry
                // so a later --force still knows whether it was ours.
                if let Some(old) = previous
                    .as_ref()
                    .and_then(|r| r.files.iter().find(|f| f.dst == dst))
                {
                    receipt_files.push(old.clone());
                }
                continue;
            }
        }

        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)?;
        receipt_files.push(ReceiptFile {
            blake3: hash_file(&dst)?,
            dst,
        });
    }

    let mut settings_outcomes = Vec::new();
    let mut receipt_settings = Vec::new();
    for settings_path in manifest.settings_manifest_paths(pkg_dir) {
        // Record (target, hooks_key, command_prefix) so uninstall works even
        // after the package is gone.
        let hook_manifest: hooks_manifest::HookManifest = {
            let text =
                std::fs::read_to_string(&settings_path).map_err(|e| AgentError::Integration {
                    agent: manifest.agent.clone(),
                    reason: format!(
                        "cannot read settings manifest {}: {}",
                        settings_path.display(),
                        e
                    ),
                })?;
            serde_json::from_str(&text).map_err(|e| AgentError::Integration {
                agent: manifest.agent.clone(),
                reason: format!(
                    "invalid settings manifest {}: {}",
                    settings_path.display(),
                    e
                ),
            })?
        };
        receipt_settings.push(ReceiptSettings {
            target: expand_dst(&hook_manifest.target)?,
            hooks_key: hook_manifest.hooks_key.clone(),
            command_prefix: hook_manifest.command_prefix.clone(),
        });
        settings_outcomes.push(hooks_manifest::install_from_manifest(&settings_path)?);
    }

    let receipt = Receipt::new(
        &manifest.agent,
        &manifest.version,
        &opts.cli_version,
        &opts.source,
        receipt_files,
        receipt_settings,
    );
    let receipt_path = receipt.save()?;

    Ok(InstallOutcome {
        agent: manifest.agent,
        version: manifest.version,
        installed,
        refreshed,
        skipped,
        settings: settings_outcomes,
        receipt_path,
    })
}

/// Remove a previously installed integration, guided by its receipt.
///
/// Files the user modified since install are kept (and reported). Settings
/// entries are removed by `(target, hooks_key, command_prefix)` without
/// needing the original package.
pub fn uninstall(agent: &str) -> AgentResult<UninstallOutcome> {
    let receipt = Receipt::load(agent)?.ok_or_else(|| AgentError::Integration {
        agent: agent.to_string(),
        reason: "not installed (no receipt found)".to_string(),
    })?;

    let mut removed = Vec::new();
    let mut kept_modified = Vec::new();

    for file in &receipt.files {
        if !file.dst.exists() {
            continue;
        }
        let current = hash_file(&file.dst)?;
        if current == file.blake3 {
            std::fs::remove_file(&file.dst)?;
            // Best-effort: drop the parent dir if we just emptied it.
            if let Some(parent) = file.dst.parent() {
                let _ = std::fs::remove_dir(parent);
            }
            removed.push(file.dst.clone());
        } else {
            kept_modified.push(file.dst.clone());
        }
    }

    let mut settings_hooks_removed = 0;
    for settings in &receipt.settings {
        let outcome = hooks_manifest::uninstall_prefixed(
            &settings.target,
            &settings.hooks_key,
            &settings.command_prefix,
        )?;
        settings_hooks_removed += outcome.removed;
    }

    Receipt::remove(agent)?;

    Ok(UninstallOutcome {
        agent: agent.to_string(),
        removed,
        kept_modified,
        settings_hooks_removed,
    })
}

enum Decision {
    Install,
    Refresh,
    Skip(SkipReason),
}

/// Decide what to do with a destination that may already exist.
fn install_decision(dst: &Path, previous: Option<&Receipt>, force: bool) -> AgentResult<Decision> {
    if !dst.exists() {
        return Ok(Decision::Install);
    }
    let recorded = previous.and_then(|r| r.recorded_hash(dst));
    match recorded {
        // Ours and untouched since install → safe to refresh.
        Some(hash) if hash == hash_file(dst)? => Ok(Decision::Refresh),
        // Ours but the user changed it → hands off unless forced.
        Some(_) if !force => Ok(Decision::Skip(SkipReason::UserModified)),
        // Not ours → hands off unless forced.
        None if !force => Ok(Decision::Skip(SkipReason::UserFile)),
        _ => Ok(Decision::Refresh),
    }
}

/// Expand a leading `~` in a destination path.
fn expand_dst(dst: &str) -> AgentResult<PathBuf> {
    if dst == "~" {
        return dirs::home_dir().ok_or_else(|| AgentError::Integration {
            agent: "<any>".to_string(),
            reason: "cannot resolve home directory".to_string(),
        });
    }
    if let Some(rest) = dst.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|h| h.join(rest))
            .ok_or_else(|| AgentError::Integration {
                agent: "<any>".to_string(),
                reason: "cannot resolve home directory".to_string(),
            });
    }
    Ok(PathBuf::from(dst))
}

/// Reject `src` paths that would escape the package directory.
fn reject_parent_traversal(agent: &str, src: &str) -> AgentResult<()> {
    if src.split('/').any(|c| c == "..") || src.starts_with('/') {
        return Err(AgentError::Integration {
            agent: agent.to_string(),
            reason: format!("invalid file src '{src}': must be a relative path inside the package"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::MANIFEST_FILE;
    use serial_test::serial;

    /// Render a filesystem path for embedding in a TOML basic (double-quoted)
    /// string. On Windows, paths contain backslashes, which TOML treats as
    /// escape-sequence introducers (`\Users` → invalid unicode escape), so
    /// they must be doubled.
    fn toml_path(path: &Path) -> String {
        path.display().to_string().replace('\\', "\\\\")
    }

    /// Build a fake package in a tempdir: manifest + two files + a settings
    /// manifest. Returns (pkg_dir, dst_dir) tempdirs.
    fn fake_package(agent: &str) -> (tempfile::TempDir, tempfile::TempDir) {
        let pkg = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(pkg.path().join("agents")).unwrap();
        std::fs::create_dir_all(pkg.path().join("hooks")).unwrap();
        std::fs::write(pkg.path().join("agents/atomic.md"), "# Atomic Agent\n").unwrap();
        std::fs::write(pkg.path().join("skills.md"), "skill content\n").unwrap();
        std::fs::write(
            pkg.path().join("hooks/hooks.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "target": dst.path().join("settings.json").to_string_lossy(),
                "command_prefix": "atomic agent hooks testagent",
                "hooks": {
                    "Stop": [ { "matcher": "", "hooks": [
                        { "type": "command", "command": "test -d .atomic && atomic agent hooks testagent stop || true" }
                    ] } ]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            format!(
                r#"
schema = 1
agent = "{agent}"
version = "1.0.0"

[requires]
atomic = ">=0.1.0"

[[file]]
src = "agents/atomic.md"
dst = "{}/agents/atomic.md"

[[file]]
src = "skills.md"
dst = "{}/skills.md"

[[settings]]
manifest = "hooks/hooks.json"
"#,
                toml_path(dst.path()),
                toml_path(dst.path())
            ),
        )
        .unwrap();
        (pkg, dst)
    }

    fn opts(force: bool) -> InstallOptions {
        InstallOptions {
            force,
            cli_version: "0.11.1".to_string(),
            source: "test".to_string(),
            expect_agent: None,
        }
    }

    #[test]
    #[serial]
    fn installs_files_and_settings_and_receipt() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let (pkg, dst) = fake_package("testagent");

        let outcome = install_from_dir(pkg.path(), &opts(false)).unwrap();

        assert_eq!(outcome.agent, "testagent");
        assert_eq!(outcome.version, "1.0.0");
        assert_eq!(outcome.installed.len(), 2);
        assert!(outcome.skipped.is_empty());
        assert!(dst.path().join("agents/atomic.md").exists());
        assert!(dst.path().join("skills.md").exists());

        // Settings merged with our hook.
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dst.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        assert!(settings["hooks"]["Stop"].as_array().unwrap()[0]
            .to_string()
            .contains("atomic agent hooks testagent"));

        // Receipt written with file hashes + settings coordinates.
        let receipt = Receipt::load("testagent").unwrap().unwrap();
        assert_eq!(receipt.files.len(), 2);
        assert_eq!(receipt.settings.len(), 1);
        assert_eq!(
            receipt.settings[0].command_prefix,
            "atomic agent hooks testagent"
        );

        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn reinstall_refreshes_ours_but_skips_foreign_and_modified() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let (pkg, dst) = fake_package("testagent2");

        install_from_dir(pkg.path(), &opts(false)).unwrap();

        // User creates a foreign file at a would-be destination of a *new*
        // manifest entry, and modifies one of our installed files.
        std::fs::write(dst.path().join("skills.md"), "user edits\n").unwrap();
        std::fs::write(dst.path().join("foreign.md"), "not atomic's\n").unwrap();

        // New package version adds a file whose dst is the foreign file.
        std::fs::write(pkg.path().join("new.md"), "new content\n").unwrap();
        let manifest_text = std::fs::read_to_string(pkg.path().join(MANIFEST_FILE))
            .unwrap()
            .replace(
                "[[settings]]",
                "[[file]]\nsrc = \"new.md\"\ndst = \"FOREIGN\"\n\n[[settings]]",
            )
            .replace("FOREIGN", &toml_path(&dst.path().join("foreign.md")));
        std::fs::write(pkg.path().join(MANIFEST_FILE), manifest_text).unwrap();

        let outcome = install_from_dir(pkg.path(), &opts(false)).unwrap();

        assert_eq!(outcome.skipped.len(), 2);
        assert!(outcome
            .skipped
            .iter()
            .any(|s| s.reason == SkipReason::UserModified));
        assert!(outcome
            .skipped
            .iter()
            .any(|s| s.reason == SkipReason::UserFile));
        // Untouched on disk.
        assert_eq!(
            std::fs::read_to_string(dst.path().join("skills.md")).unwrap(),
            "user edits\n"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("foreign.md")).unwrap(),
            "not atomic's\n"
        );

        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn force_overwrites_foreign_files() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let (pkg, dst) = fake_package("testagent3");

        std::fs::create_dir_all(dst.path().join("agents")).unwrap();
        std::fs::write(dst.path().join("agents/atomic.md"), "user's own\n").unwrap();

        let outcome = install_from_dir(pkg.path(), &opts(true)).unwrap();
        assert!(outcome.skipped.is_empty());
        assert_eq!(
            std::fs::read_to_string(dst.path().join("agents/atomic.md")).unwrap(),
            "# Atomic Agent\n"
        );

        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn uninstall_removes_ours_keeps_modified_and_strips_hooks() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let (pkg, dst) = fake_package("testagent4");

        install_from_dir(pkg.path(), &opts(false)).unwrap();
        std::fs::write(dst.path().join("skills.md"), "user edits\n").unwrap();
        // Package deleted before uninstall — receipt must be enough.
        drop(pkg);

        let outcome = uninstall("testagent4").unwrap();

        assert_eq!(outcome.removed.len(), 1); // agents/atomic.md only
        assert_eq!(outcome.kept_modified.len(), 1); // skills.md
        assert!(outcome.settings_hooks_removed >= 1);
        assert!(!dst.path().join("agents/atomic.md").exists());
        assert!(dst.path().join("skills.md").exists());

        // Hooks stripped from settings.
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dst.path().join("settings.json")).unwrap(),
        )
        .unwrap();
        let stop_entries = settings["hooks"]["Stop"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(!stop_entries
            .iter()
            .any(|e| e.to_string().contains("atomic agent hooks testagent")));

        // Receipt gone; second uninstall errors cleanly.
        assert!(Receipt::load("testagent4").unwrap().is_none());
        assert!(uninstall("testagent4").is_err());

        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn agent_mismatch_rejected() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let (pkg, _dst) = fake_package("real-agent");
        let mut o = opts(false);
        o.expect_agent = Some("other-agent".to_string());
        let err = install_from_dir(pkg.path(), &o).unwrap_err();
        assert!(err.to_string().contains("is for agent 'real-agent'"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn parent_traversal_src_rejected() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let pkg = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            format!(
                "schema = 1\nagent = \"evil\"\nversion = \"1.0.0\"\n\n[[file]]\nsrc = \"../../etc/passwd\"\ndst = \"{}/x\"\n",
                toml_path(dst.path())
            ),
        )
        .unwrap();
        let err = install_from_dir(pkg.path(), &opts(false)).unwrap_err();
        assert!(err.to_string().contains("must be a relative path"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }
}

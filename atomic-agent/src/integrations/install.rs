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
    /// Path to the cloned `atomic-skills` cache. Required when the manifest
    /// declares `[skills-source]` or `[agent-definition]`. The CLI syncs
    /// the cache before calling `install_from_dir`.
    pub skills_cache_dir: Option<PathBuf>,
    /// Repository root for `[[repo-file]]` entries (AGENTS.md-into-repo).
    /// When `None`, `[[repo-file]]` entries are skipped silently — the caller
    /// gates this via the `--repo-agents` prompt.
    pub repo_root: Option<PathBuf>,
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
        copy_one(
            &manifest.agent,
            &src,
            &dst,
            previous.as_ref(),
            opts.force,
            &mut installed,
            &mut refreshed,
            &mut skipped,
            &mut receipt_files,
        )?;
    }

    // Auto-install all skills from the shared cache via [skills] block.
    // The installer globs skills/*/SKILL.md and formats dst_pattern with
    // {name}. This means adding a skill to atomic-skills requires zero
    // plugin manifest changes.
    if let Some(ref config) = manifest.skills_config {
        let cache = opts
            .skills_cache_dir
            .as_deref()
            .ok_or_else(|| AgentError::Integration {
                agent: manifest.agent.clone(),
                reason: "manifest declares [skills] but no skills cache was \
                         provided (expected [skills-source] to be synced by the CLI)"
                    .to_string(),
            })?;

        if !config.dst_pattern.contains("{name}") {
            return Err(AgentError::Integration {
                agent: manifest.agent.clone(),
                reason: format!(
                    "[skills] dst_pattern must contain '{{name}}' placeholder, got '{}'",
                    config.dst_pattern
                ),
            });
        }

        // Glob skills/*/SKILL.md in the cache.
        let skills_dir = cache.join("skills");
        let skill_names: Vec<String> = std::fs::read_dir(&skills_dir)
            .map_err(|e| AgentError::Integration {
                agent: manifest.agent.clone(),
                reason: format!("cannot read skills dir {}: {}", skills_dir.display(), e),
            })?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().join("SKILL.md").is_file() {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();

        // Filter by install list if not "all".
        let wanted: Vec<String> = if config.install.trim() == "all" {
            skill_names
        } else {
            let allowed: Vec<&str> = config.install.split(',').map(|s| s.trim()).collect();
            skill_names
                .into_iter()
                .filter(|n| allowed.contains(&n.as_str()))
                .collect()
        };

        for name in &wanted {
            let src = cache.join("skills").join(name).join("SKILL.md");
            let dst_str = config.dst_pattern.replace("{name}", name);
            let dst = expand_dst(&dst_str)?;
            copy_one(
                &manifest.agent,
                &src,
                &dst,
                previous.as_ref(),
                opts.force,
                &mut installed,
                &mut refreshed,
                &mut skipped,
                &mut receipt_files,
            )?;
        }
    }

    // Explicit [[skill]] entries — for non-skill cache files (AGENTS.md to
    // vendor instruction paths). For actual skills, prefer [skills] with
    // install = "all".
    if !manifest.skills.is_empty() {
        let cache = opts
            .skills_cache_dir
            .as_deref()
            .ok_or_else(|| AgentError::Integration {
                agent: manifest.agent.clone(),
                reason: "manifest declares [[skill]] entries but no skills cache was \
                         provided (expected [skills-source] to be synced by the CLI)"
                    .to_string(),
            })?;
        for entry in &manifest.skills {
            reject_parent_traversal(&manifest.agent, &entry.src)?;
            let src = cache.join(&entry.src);
            if !src.is_file() {
                return Err(AgentError::Integration {
                    agent: manifest.agent.clone(),
                    reason: format!("skill source missing: {}", src.display()),
                });
            }
            let dst = expand_dst(&entry.dst)?;
            copy_one(
                &manifest.agent,
                &src,
                &dst,
                previous.as_ref(),
                opts.force,
                &mut installed,
                &mut refreshed,
                &mut skipped,
                &mut receipt_files,
            )?;
        }
    }

    // Repo-level files (AGENTS.md into the repo root). Only when the caller
    // passed a repo_root — gated by the --agents-md prompt in the CLI.
    // Unlike [[file]] and [[skill]], a repo-file that exists but isn't ours
    // is *merged* (canonical content appended with a separator) rather than
    // skipped — the user's project-specific AGENTS.md content is preserved.
    if !manifest.repo_files.is_empty() {
        if let Some(repo) = opts.repo_root.as_deref() {
            for entry in &manifest.repo_files {
                reject_parent_traversal(&manifest.agent, &entry.src)?;
                // src comes from the skills cache (or the package if no
                // skills-source is set — uncommon but valid).
                let src_base = opts.skills_cache_dir.as_deref().unwrap_or(pkg_dir);
                let src = src_base.join(&entry.src);
                if !src.is_file() {
                    return Err(AgentError::Integration {
                        agent: manifest.agent.clone(),
                        reason: format!("repo-file source missing: {}", src.display()),
                    });
                }
                // dst is relative to the repo root.
                let dst = expand_repo_dst(&entry.dst, repo)?;
                copy_repo_file(
                    &src,
                    &dst,
                    previous.as_ref(),
                    opts.force,
                    &mut installed,
                    &mut refreshed,
                    &mut skipped,
                    &mut receipt_files,
                )?;
            }
        }
    }

    // Bundled agent definition: stitch frontmatter + canonical body at install
    // time. Pure string concat — no exec, same safety category as fs::copy.
    if let Some(def) = &manifest.agent_definition {
        let cache = opts
            .skills_cache_dir
            .as_deref()
            .ok_or_else(|| AgentError::Integration {
                agent: manifest.agent.clone(),
                reason: "manifest declares [agent-definition] but no skills cache \
                         was provided (needed for body_from)"
                    .to_string(),
            })?;
        reject_parent_traversal(&manifest.agent, &def.src)?;
        let frontmatter_path = pkg_dir.join(&def.src);
        let frontmatter =
            std::fs::read_to_string(&frontmatter_path).map_err(|e| AgentError::Integration {
                agent: manifest.agent.clone(),
                reason: format!(
                    "agent-definition frontmatter missing: {}: {}",
                    frontmatter_path.display(),
                    e
                ),
            })?;
        let body_path = manifest
            .resolve_body_from(&def.body_from, cache)
            .ok_or_else(|| AgentError::Integration {
                agent: manifest.agent.clone(),
                reason: format!(
                    "agent-definition body_from '{}' is malformed or does not \
                         match the skills-source package",
                    def.body_from
                ),
            })?;
        let body = std::fs::read_to_string(&body_path).map_err(|e| AgentError::Integration {
            agent: manifest.agent.clone(),
            reason: format!(
                "agent-definition body missing: {}: {}",
                body_path.display(),
                e
            ),
        })?;
        let stitched = format!("{frontmatter}\n\n{body}");
        let slot = expand_dst(&def.slot)?;

        match install_decision(&slot, previous.as_ref(), opts.force)? {
            Decision::Install => installed.push(slot.clone()),
            Decision::Refresh => refreshed.push(slot.clone()),
            // Merge only applies to [[repo-file]] (via repo_file_decision).
            // install_decision never returns Merge for an agent-definition
            // slot — treat it as a refresh if it somehow occurs.
            Decision::Merge => refreshed.push(slot.clone()),
            Decision::Skip(reason) => {
                skipped.push(SkippedFile {
                    dst: slot.clone(),
                    reason,
                });
                if let Some(old) = previous
                    .as_ref()
                    .and_then(|r| r.files.iter().find(|f| f.dst == slot))
                {
                    receipt_files.push(old.clone());
                }
                // We don't write the stitched content, but we still proceed.
            }
        }

        if !skipped.iter().any(|s| s.dst == slot) {
            if let Some(parent) = slot.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&slot, &stitched)?;
            receipt_files.push(ReceiptFile {
                blake3: hash_file(&slot)?,
                dst: slot,
            });
        }
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
    Merge,
    Skip(SkipReason),
}

/// The Atomic marker that identifies a file as already containing the
/// canonical Atomic workflow instructions. Used to distinguish "user's own
/// AGENTS.md" from "has Atomic content already (maybe from a prior install
/// or copy)".
const ATOMIC_MARKER: &str = "# Atomic VCS Agent";

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

/// Decide what to do with a [[repo-file]] destination. Unlike [[file]] and
/// [[skill]], a repo-file that exists but isn't ours can be *merged* — the
/// canonical content is appended with a separator, preserving the user's
/// existing content. If the file already contains the Atomic marker, it's
/// treated as a refresh (replace the whole file).
fn repo_file_decision(
    dst: &Path,
    previous: Option<&Receipt>,
    force: bool,
) -> AgentResult<Decision> {
    if !dst.exists() {
        return Ok(Decision::Install);
    }
    let recorded = previous.and_then(|r| r.recorded_hash(dst));
    match recorded {
        // Ours and untouched since install → safe to refresh.
        Some(hash) if hash == hash_file(dst)? => Ok(Decision::Refresh),
        // Ours but the user changed it → hands off unless forced.
        Some(_) if !force => Ok(Decision::Skip(SkipReason::UserModified)),
        // Not ours. If it already has the Atomic marker, treat as refresh
        // (replace the whole file — it's Atomic content, just not from this
        // receipt). Otherwise merge (append with separator).
        None if !force => {
            let existing = std::fs::read_to_string(dst).unwrap_or_default();
            if existing.contains(ATOMIC_MARKER) {
                Ok(Decision::Refresh)
            } else {
                Ok(Decision::Merge)
            }
        }
        // --force: clobber everything.
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

/// Resolve a `[[repo-file]]` destination against the repository root.
/// `dst` must be a relative path (no `~`, no absolute) — it lands inside the
/// repo. Rejects parent traversal so a manifest can't write outside the repo.
fn expand_repo_dst(dst: &str, repo_root: &Path) -> AgentResult<PathBuf> {
    if dst.starts_with('/') || dst.starts_with('~') {
        return Err(AgentError::Integration {
            agent: "<any>".to_string(),
            reason: format!(
                "repo-file dst '{dst}' must be relative to the repo root, not absolute or home"
            ),
        });
    }
    if dst.split('/').any(|c| c == "..") {
        return Err(AgentError::Integration {
            agent: "<any>".to_string(),
            reason: format!("repo-file dst '{dst}' must not escape the repo root"),
        });
    }
    Ok(repo_root.join(dst))
}

/// Copy one file into place with the standard install-decision logic.
/// Shared by [[file]], [[skill]], and [[repo-file]] entries.
#[allow(clippy::too_many_arguments)]
fn copy_one(
    agent: &str,
    src: &Path,
    dst: &Path,
    previous: Option<&Receipt>,
    force: bool,
    installed: &mut Vec<PathBuf>,
    refreshed: &mut Vec<PathBuf>,
    skipped: &mut Vec<SkippedFile>,
    receipt_files: &mut Vec<ReceiptFile>,
) -> AgentResult<()> {
    let _ = agent; // used only in error context by callers
    match install_decision(dst, previous, force)? {
        Decision::Install => installed.push(dst.to_path_buf()),
        Decision::Refresh => refreshed.push(dst.to_path_buf()),
        // Merge only applies to [[repo-file]] (via copy_repo_file). For
        // [[file]] and [[skill]], install_decision never returns Merge —
        // treat as a refresh if it somehow occurs.
        Decision::Merge => refreshed.push(dst.to_path_buf()),
        Decision::Skip(reason) => {
            skipped.push(SkippedFile {
                dst: dst.to_path_buf(),
                reason,
            });
            // Keep protecting this file: preserve any prior receipt entry
            // so a later --force still knows whether it was ours.
            if let Some(old) = previous.and_then(|r| r.files.iter().find(|f| f.dst == dst)) {
                receipt_files.push(old.clone());
            }
            return Ok(());
        }
    }

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    receipt_files.push(ReceiptFile {
        blake3: hash_file(dst)?,
        dst: dst.to_path_buf(),
    });
    Ok(())
}

/// Copy a [[repo-file]] into the repo root, with merge support.
///
/// Unlike [[file]] and [[skill]] (which use `copy_one`), a repo-file that
/// exists but isn't ours is *merged* — the canonical content is appended
/// with a markdown horizontal rule separator, preserving the user's
/// existing project-specific content above. If the file already contains
/// the Atomic marker (`# Atomic VCS Agent`), it's treated as a refresh
/// (replace the whole file). `--force` always clobbers.
#[allow(clippy::too_many_arguments)]
fn copy_repo_file(
    src: &Path,
    dst: &Path,
    previous: Option<&Receipt>,
    force: bool,
    installed: &mut Vec<PathBuf>,
    refreshed: &mut Vec<PathBuf>,
    skipped: &mut Vec<SkippedFile>,
    receipt_files: &mut Vec<ReceiptFile>,
) -> AgentResult<()> {
    match repo_file_decision(dst, previous, force)? {
        Decision::Install => {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(src, dst)?;
            installed.push(dst.to_path_buf());
            receipt_files.push(ReceiptFile {
                blake3: hash_file(dst)?,
                dst: dst.to_path_buf(),
            });
        }
        Decision::Refresh => {
            std::fs::copy(src, dst)?;
            refreshed.push(dst.to_path_buf());
            receipt_files.push(ReceiptFile {
                blake3: hash_file(dst)?,
                dst: dst.to_path_buf(),
            });
        }
        Decision::Merge => {
            // Append the canonical content to the user's existing file with
            // a markdown horizontal rule separator. The user's content is
            // preserved above; the Atomic workflow content goes below.
            let existing = std::fs::read_to_string(dst)?;
            let canonical = std::fs::read_to_string(src)?;
            let merged = format!("{existing}\n\n---\n\n{canonical}");
            std::fs::write(dst, merged)?;
            installed.push(dst.to_path_buf());
            receipt_files.push(ReceiptFile {
                blake3: hash_file(dst)?,
                dst: dst.to_path_buf(),
            });
        }
        Decision::Skip(reason) => {
            skipped.push(SkippedFile {
                dst: dst.to_path_buf(),
                reason,
            });
            if let Some(old) = previous.and_then(|r| r.files.iter().find(|f| f.dst == dst)) {
                receipt_files.push(old.clone());
            }
        }
    }
    Ok(())
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
            skills_cache_dir: None,
            repo_root: None,
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

    // ── New install paths: skills, repo-file, agent-definition ──────────

    /// Build a fake skills cache (mimics atomic-skills/) with AGENTS.md and
    /// one skill.
    fn fake_skills_cache() -> tempfile::TempDir {
        let cache = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cache.path().join("skills/atomic-vault")).unwrap();
        std::fs::write(
            cache.path().join("AGENTS.md"),
            "# Atomic VCS Agent\n\ncanonical body\n",
        )
        .unwrap();
        std::fs::write(
            cache.path().join("skills/atomic-vault/SKILL.md"),
            "# atomic-vault skill\n",
        )
        .unwrap();
        cache
    }

    /// Build a fake plugin that uses [skills-source] + [[skill]].
    fn fake_plugin_with_skills(agent: &str, dst_dir: &Path) -> tempfile::TempDir {
        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            format!(
                r#"
schema = 1
agent = "{agent}"
version = "1.0.0"

[requires]
atomic = ">=0.1.0"

[skills-source]
package = "atomic-skills"

[[skill]]
src = "skills/atomic-vault/SKILL.md"
dst = "{0}/skills/atomic-vault/SKILL.md"
"#,
                toml_path(dst_dir),
            ),
        )
        .unwrap();
        pkg
    }

    #[test]
    #[serial]
    fn installs_skills_from_cache() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let dst = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();
        let pkg = fake_plugin_with_skills("skill-test", dst.path());

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        let outcome = install_from_dir(pkg.path(), &o).unwrap();

        assert_eq!(outcome.installed.len(), 1);
        let skill_dst = dst.path().join("skills/atomic-vault/SKILL.md");
        assert!(skill_dst.exists());
        assert_eq!(
            std::fs::read_to_string(&skill_dst).unwrap(),
            "# atomic-vault skill\n"
        );
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn skills_without_cache_errors() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let dst = tempfile::tempdir().unwrap();
        let pkg = fake_plugin_with_skills("skill-test-err", dst.path());

        let err = install_from_dir(pkg.path(), &opts(false)).unwrap_err();
        assert!(err.to_string().contains("no skills cache was provided"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn installs_repo_file_into_repo_root() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let repo = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            format!(
                r#"
schema = 1
agent = "repo-file-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[[repo-file]]
src = "AGENTS.md"
dst = "AGENTS.md"
"#,
            ),
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        o.repo_root = Some(repo.path().to_path_buf());
        let outcome = install_from_dir(pkg.path(), &o).unwrap();

        assert_eq!(outcome.installed.len(), 1);
        let agents_dst = repo.path().join("AGENTS.md");
        assert!(agents_dst.exists());
        assert!(std::fs::read_to_string(&agents_dst)
            .unwrap()
            .contains("canonical body"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn repo_file_skipped_without_repo_root() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let repo = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            r#"
schema = 1
agent = "repo-file-skip-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[[repo-file]]
src = "AGENTS.md"
dst = "AGENTS.md"
"#,
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        // repo_root intentionally None — simulates user said "no" to the prompt.
        let outcome = install_from_dir(pkg.path(), &o).unwrap();

        assert!(outcome.installed.is_empty());
        assert!(!repo.path().join("AGENTS.md").exists());
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn repo_file_rejects_absolute_dst() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let repo = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            r#"
schema = 1
agent = "repo-file-abs-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[[repo-file]]
src = "AGENTS.md"
dst = "/etc/passwd"
"#,
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        o.repo_root = Some(repo.path().to_path_buf());
        let err = install_from_dir(pkg.path(), &o).unwrap_err();
        assert!(err
            .to_string()
            .contains("must be relative to the repo root"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn repo_file_rejects_traversal_dst() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let repo = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            r#"
schema = 1
agent = "repo-file-trav-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[[repo-file]]
src = "AGENTS.md"
dst = "../escape.md"
"#,
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        o.repo_root = Some(repo.path().to_path_buf());
        let err = install_from_dir(pkg.path(), &o).unwrap_err();
        assert!(err.to_string().contains("must not escape the repo root"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn stitches_agent_definition_at_install() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let dst = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(pkg.path().join("agents")).unwrap();
        std::fs::write(
            pkg.path().join("agents/atomic.md.frontmatter"),
            "---\ndescription: Test agent\nmode: primary\n---\n",
        )
        .unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            format!(
                r#"
schema = 1
agent = "stitch-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[agent-definition]
src = "agents/atomic.md.frontmatter"
body_from = "atomic-skills:AGENTS.md"
slot = "{0}/agents/atomic.md"
"#,
                toml_path(dst.path()),
            ),
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        let outcome = install_from_dir(pkg.path(), &o).unwrap();

        assert_eq!(outcome.installed.len(), 1);
        let agent_file = dst.path().join("agents/atomic.md");
        assert!(agent_file.exists());
        let content = std::fs::read_to_string(&agent_file).unwrap();
        // Frontmatter present.
        assert!(content.starts_with("---\ndescription: Test agent\nmode: primary\n---\n"));
        // Canonical body present.
        assert!(content.contains("canonical body"));
        // The receipt recorded the stitched file.
        let receipt = Receipt::load("stitch-test").unwrap().unwrap();
        assert_eq!(receipt.files.len(), 1);
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn agent_definition_without_cache_errors() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let dst = tempfile::tempdir().unwrap();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(pkg.path().join("agents")).unwrap();
        std::fs::write(
            pkg.path().join("agents/atomic.md.frontmatter"),
            "---\ndescription: Test\n---\n",
        )
        .unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            format!(
                r#"
schema = 1
agent = "stitch-no-cache"
version = "1.0.0"

[agent-definition]
src = "agents/atomic.md.frontmatter"
body_from = "atomic-skills:AGENTS.md"
slot = "{0}/agents/atomic.md"
"#,
                toml_path(dst.path()),
            ),
        )
        .unwrap();

        let err = install_from_dir(pkg.path(), &opts(false)).unwrap_err();
        assert!(err.to_string().contains("no skills cache was provided"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn repo_file_uninstall_removes_from_repo() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let repo = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            r#"
schema = 1
agent = "repo-uninstall-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[[repo-file]]
src = "AGENTS.md"
dst = "AGENTS.md"
"#,
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        o.repo_root = Some(repo.path().to_path_buf());
        install_from_dir(pkg.path(), &o).unwrap();
        assert!(repo.path().join("AGENTS.md").exists());

        // Uninstall must remove it.
        let outcome = uninstall("repo-uninstall-test").unwrap();
        assert_eq!(outcome.removed.len(), 1);
        assert!(!repo.path().join("AGENTS.md").exists());
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn repo_file_merges_with_existing_user_file() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let repo = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();

        // User already has a project-specific AGENTS.md.
        let user_content = "# My Project\n\nThis is my custom project guide.\n";
        std::fs::write(repo.path().join("AGENTS.md"), user_content).unwrap();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            r#"
schema = 1
agent = "merge-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[[repo-file]]
src = "AGENTS.md"
dst = "AGENTS.md"
"#,
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        o.repo_root = Some(repo.path().to_path_buf());
        let outcome = install_from_dir(pkg.path(), &o).unwrap();

        // Should be "installed" (merged), not skipped.
        assert_eq!(outcome.installed.len(), 1);
        assert!(outcome.skipped.is_empty());

        let result = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        // User's content is preserved above the separator.
        assert!(result.starts_with("# My Project"));
        assert!(result.contains("This is my custom project guide."));
        // Separator is present.
        assert!(result.contains("\n\n---\n\n"));
        // Canonical Atomic content is appended below.
        assert!(result.contains("# Atomic VCS Agent"));
        assert!(result.contains("canonical body"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn repo_file_refreshes_when_atomic_marker_present() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let repo = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();

        // File already has the Atomic marker (maybe from a manual copy).
        let stale = "# Atomic VCS Agent\n\nOld stale content\n";
        std::fs::write(repo.path().join("AGENTS.md"), stale).unwrap();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            r#"
schema = 1
agent = "marker-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[[repo-file]]
src = "AGENTS.md"
dst = "AGENTS.md"
"#,
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        o.repo_root = Some(repo.path().to_path_buf());
        let outcome = install_from_dir(pkg.path(), &o).unwrap();

        // Should be refreshed, not merged (marker present → replace).
        assert_eq!(outcome.refreshed.len(), 1);
        assert!(outcome.installed.is_empty());

        let result = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        // Stale content is gone, canonical is in place.
        assert!(result.contains("canonical body"));
        assert!(!result.contains("Old stale content"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn repo_file_force_clobbers_user_file() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let repo = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();

        // User has their own AGENTS.md (no Atomic marker).
        let user_content = "# My Project\n\nDon't clobber me\n";
        std::fs::write(repo.path().join("AGENTS.md"), user_content).unwrap();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            r#"
schema = 1
agent = "force-clobber-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[[repo-file]]
src = "AGENTS.md"
dst = "AGENTS.md"
"#,
        )
        .unwrap();

        let mut o = opts(true); // force = clobber
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        o.repo_root = Some(repo.path().to_path_buf());
        let outcome = install_from_dir(pkg.path(), &o).unwrap();

        // Force clobbers — user's content is gone.
        assert!(outcome.skipped.is_empty());
        let result = std::fs::read_to_string(repo.path().join("AGENTS.md")).unwrap();
        assert!(!result.contains("Don't clobber me"));
        assert!(result.contains("canonical body"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn skills_glob_installs_all_from_cache() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let dst = tempfile::tempdir().unwrap();

        // Build a skills cache with 3 skills.
        let cache = tempfile::tempdir().unwrap();
        for skill in &["atomic-vault", "atomic-vcs", "code-intelligence"] {
            let dir = cache.path().join("skills").join(skill);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("# {skill}\n")).unwrap();
        }

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            format!(
                r#"
schema = 1
agent = "glob-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[skills]
install = "all"
dst_pattern = "{0}/skills/{{name}}/SKILL.md"
"#,
                toml_path(dst.path()),
            ),
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        let outcome = install_from_dir(pkg.path(), &o).unwrap();

        // All 3 skills installed.
        assert_eq!(outcome.installed.len(), 3);
        for skill in &["atomic-vault", "atomic-vcs", "code-intelligence"] {
            let path = dst.path().join("skills").join(skill).join("SKILL.md");
            assert!(path.exists(), "skill {skill} not installed");
        }
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn skills_glob_flat_pattern() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let dst = tempfile::tempdir().unwrap();

        // Cache with 2 skills.
        let cache = tempfile::tempdir().unwrap();
        for skill in &["atomic-vault", "code-intelligence"] {
            let dir = cache.path().join("skills").join(skill);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("# {skill}\n")).unwrap();
        }

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            format!(
                r#"
schema = 1
agent = "flat-glob-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[skills]
install = "all"
dst_pattern = "{0}/workflows/{{name}}.md"
"#,
                toml_path(dst.path()),
            ),
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        let outcome = install_from_dir(pkg.path(), &o).unwrap();

        // 2 skills installed as flat .md files.
        assert_eq!(outcome.installed.len(), 2);
        assert!(dst.path().join("workflows/atomic-vault.md").exists());
        assert!(dst.path().join("workflows/code-intelligence.md").exists());
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn skills_glob_picks_up_new_skill_automatically() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let dst = tempfile::tempdir().unwrap();

        // Cache with 2 skills initially.
        let cache = tempfile::tempdir().unwrap();
        for skill in &["atomic-vault", "atomic-vcs"] {
            let dir = cache.path().join("skills").join(skill);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("# {skill}\n")).unwrap();
        }

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            format!(
                r#"
schema = 1
agent = "auto-new-skill-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[skills]
install = "all"
dst_pattern = "{0}/skills/{{name}}/SKILL.md"
"#,
                toml_path(dst.path()),
            ),
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        let outcome1 = install_from_dir(pkg.path(), &o).unwrap();
        assert_eq!(outcome1.installed.len(), 2);

        // A new skill is added to the cache — no manifest change needed.
        let new_skill_dir = cache.path().join("skills/triage-review");
        std::fs::create_dir_all(&new_skill_dir).unwrap();
        std::fs::write(new_skill_dir.join("SKILL.md"), "# triage-review\n").unwrap();

        // Re-install with force — picks up the new skill automatically.
        o.force = true;
        let outcome2 = install_from_dir(pkg.path(), &o).unwrap();
        // 2 refreshed + 1 new = 3 installed/refreshed total.
        assert_eq!(outcome2.installed.len() + outcome2.refreshed.len(), 3);
        assert!(dst.path().join("skills/triage-review/SKILL.md").exists());
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }

    #[test]
    #[serial]
    fn skills_glob_errors_without_name_placeholder() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMIC_INTEGRATIONS_HOME", home.path());
        let dst = tempfile::tempdir().unwrap();
        let cache = fake_skills_cache();

        let pkg = tempfile::tempdir().unwrap();
        std::fs::write(
            pkg.path().join(MANIFEST_FILE),
            format!(
                r#"
schema = 1
agent = "no-placeholder-test"
version = "1.0.0"

[skills-source]
package = "atomic-skills"

[skills]
install = "all"
dst_pattern = "{0}/skills/all.md"
"#,
                toml_path(dst.path()),
            ),
        )
        .unwrap();

        let mut o = opts(false);
        o.skills_cache_dir = Some(cache.path().to_path_buf());
        let err = install_from_dir(pkg.path(), &o).unwrap_err();
        assert!(err
            .to_string()
            .contains("must contain '{name}' placeholder"));
        std::env::remove_var("ATOMIC_INTEGRATIONS_HOME");
    }
}

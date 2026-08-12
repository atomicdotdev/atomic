//! `atomic agent enable` command implementation.
//!
//! Installs agent hooks into the AI coding agent's configuration file so that
//! each agent turn is automatically recorded as an Atomic change.
//!
//! # What It Does
//!
//! 1. Finds the repository root (`.atomic/` must exist)
//! 2. Detects which agent is present (or uses `--agent` flag)
//! 3. Calls `AgentHook::install()` to write hooks into the agent's config
//! 4. Creates `.atomic/sessions/` directory for session state persistence
//! 5. Prints a success message with the number of hooks installed
//!
//! # Examples
//!
//! ```text
//! # Auto-detect which agent is present
//! atomic agent enable
//!
//! # Specify the agent explicitly
//! atomic agent enable --agent claude-code
//!
//! # Force reinstall (removes existing Atomic hooks first)
//! atomic agent enable --force
//!
//! # Install for all detected agents
//! atomic agent enable --all
//! ```

use std::path::PathBuf;

use clap::Args;

use atomic_agent::hooks::{codex::CodexHook, AgentRegistry};
use atomic_agent::integrations::Receipt;

use crate::commands::{find_repository_root, Command};
use crate::error::CliResult;
use crate::output::{print_error, print_success, print_warning};

// Enable Command

/// Install agent hooks for turn-level recording.
#[derive(Debug, Args)]
pub struct Enable {
    /// Which agent to install hooks for.
    ///
    /// If not specified, auto-detects which agent is present in the
    /// repository (looks for `.claude/`, `.gemini/`, etc.).
    #[arg(long, value_name = "NAME")]
    agent: Option<String>,

    /// Force reinstall hooks even if already installed.
    ///
    /// Removes existing Atomic hooks before installing new ones.
    /// Non-Atomic hooks in the agent's config are preserved.
    #[arg(short, long)]
    force: bool,

    /// Install hooks for all detected agents.
    ///
    /// If multiple agents are detected (e.g., both Claude Code and
    /// Gemini CLI are configured), install hooks for all of them.
    #[arg(long)]
    all: bool,

    /// Install hooks globally (~/.claude/settings.json).
    ///
    /// Global hooks fire for every Claude Code session regardless of
    /// project. This is the recommended way to enable Atomic tracking —
    /// install once, works everywhere that has a `.atomic/` directory.
    #[arg(short, long)]
    global: bool,

    /// Install hooks from an integration-supplied manifest file.
    ///
    /// The manifest (shipped by an integration package such as atomic-codex)
    /// names its own target settings file and the hook commands to register,
    /// and is merged in idempotently — preserving non-Atomic hooks. Because the
    /// definitions live in the integration repo, updating an agent's hook
    /// wiring never requires rebuilding `atomic`. When set, `--agent`/`--global`
    /// are not needed; the manifest is self-describing.
    #[arg(long, value_name = "FILE")]
    hooks: Option<std::path::PathBuf>,

    /// Install the integration package from a local checkout instead of
    /// syncing it from Atomic storage.
    ///
    /// For development of integration packages (e.g. a local atomic-opencode
    /// clone): installs exactly what the storage path would, per the
    /// package's atomic-integration.toml, without any network access.
    #[arg(long, value_name = "PATH")]
    from: Option<std::path::PathBuf>,

    /// Also install AGENTS.md into the repository root so the Atomic
    /// workflow is always-on without picking a bundled agent.
    ///
    /// When set, `[[repo-file]]` entries from the manifest are installed
    /// into the repo. When neither `--agents-md` nor `--no-agents-md`
    /// is set, the user is prompted interactively (default: no).
    #[arg(long)]
    agents_md: bool,

    /// Skip the AGENTS.md-into-repo prompt entirely.
    ///
    /// Suppresses the interactive prompt and skips `[[repo-file]]` entries.
    /// Useful in non-interactive contexts or when you only want the global
    /// agent install.
    #[arg(long)]
    no_agents_md: bool,
}

impl Enable {
    /// Create a default instance for testing.
    #[cfg(test)]
    pub(crate) fn default_for_test() -> Self {
        Self {
            agent: None,
            force: false,
            all: false,
            global: false,
            hooks: None,
            from: None,
            agents_md: false,
            no_agents_md: false,
        }
    }
}

/// Return the hook files managed by a Codex integration receipt.
///
/// Current receipts record the exact manifest target. The global default is
/// retained for older receipts that predate the `settings` field.
fn receipt_codex_hook_targets(receipt: &Receipt) -> Vec<PathBuf> {
    let mut targets = Vec::new();
    for settings in &receipt.settings {
        if settings.command_prefix.contains("atomic agent hooks codex")
            && !targets.contains(&settings.target)
        {
            targets.push(settings.target.clone());
        }
    }

    if targets.is_empty() {
        if let Some(path) = CodexHook::global_hooks_path() {
            targets.push(path);
        }
    }
    targets
}

/// Migrate stale Codex hooks in the settings files owned by an integration
/// receipt. `None` means every receipt-managed target was already current.
fn repair_stale_codex_receipt_hooks(
    receipt: &Receipt,
) -> atomic_agent::error::AgentResult<Option<usize>> {
    let hook = CodexHook::new();
    let stale_targets: Vec<PathBuf> = receipt_codex_hook_targets(receipt)
        .into_iter()
        .filter(|path| !hook.is_installed_at(path))
        .collect();
    if stale_targets.is_empty() {
        return Ok(None);
    }

    let mut installed = 0;
    for target in stale_targets {
        installed += hook.install_at(&target, false)?;
    }
    Ok(Some(installed))
}

impl Command for Enable {
    fn run(&self) -> CliResult<()> {
        // Data-driven install — definitions come from the integration's manifest.
        if let Some(ref manifest) = self.hooks {
            return self.run_manifest(manifest);
        }

        // Global install — writes to ~/.claude/settings.json
        if self.global {
            return self.run_global();
        }

        // Find the repository root
        let repo_root = find_repository_root()?;

        // Ensure .atomic/sessions directory exists
        let sessions_dir = repo_root.join(".atomic").join("sessions");
        if !sessions_dir.exists() {
            std::fs::create_dir_all(&sessions_dir).map_err(|e| {
                crate::error::CliError::Io(std::io::Error::new(
                    e.kind(),
                    format!("Failed to create sessions directory: {}", e),
                ))
            })?;
        }

        let registry = AgentRegistry::with_defaults();

        // Determine which agents to install for
        let agents_to_install: Vec<&str> = if self.all {
            // Install for all detected agents
            let detected = registry.detect(&repo_root);
            if detected.is_empty() {
                // Do not install hooks when no agent is detected.
                print_error(
                    "No agents detected in this repository — nothing to enable with --all.",
                );
                print_warning(
                    "Create a .claude/, .gemini/, or .agents/ directory first, or use --agent <name>.",
                );
                return Err(crate::error::CliError::InvalidArgument {
                    message: "no agents detected for --all".to_string(),
                });
            }
            detected
        } else if let Some(ref name) = self.agent {
            // Specific agent requested — validate it exists
            registry
                .require(name)
                .map_err(|e| crate::error::CliError::InvalidArgument {
                    message: format!("Unknown agent '{}': {}", name, e),
                })?;
            vec![name.as_str()]
        } else {
            // Auto-detect
            let detected = registry.detect(&repo_root);
            if detected.is_empty() {
                // No agent detected — try to install for all, with a hint
                let available = registry.list();
                if available.is_empty() {
                    print_error("No agents available. This is a bug — the registry should have built-in agents.");
                    return Ok(());
                }

                // Default to Claude Code when nothing is detected (fall back
                // to the first registered agent if it is ever unavailable).
                let default_agent = if available.contains(&"claude-code") {
                    "claude-code"
                } else {
                    available[0]
                };
                print_warning(&format!(
                    "No agent detected in this repository. Defaulting to '{}'.",
                    default_agent,
                ));
                print_warning(
                    "Create a .claude/, .gemini/, or .agents/ directory first, or use --agent <name>.",
                );
                vec![default_agent]
            } else if detected.len() == 1 {
                detected
            } else {
                // Multiple agents detected — ask user to be specific or use --all
                println!("Multiple agents detected: {}", detected.to_vec().join(", "));
                println!("Use --agent <name> to choose one, or --all to install for all.");
                return Ok(());
            }
        };

        // Install hooks for each selected agent
        let mut total_installed = 0;

        for agent_name in &agents_to_install {
            let agent = match registry.get(agent_name) {
                Some(a) => a,
                None => {
                    print_warning(&format!(
                        "Agent '{}' not found in registry — skipping.",
                        agent_name
                    ));
                    continue;
                }
            };

            // Externally-packaged agents: install the integration package
            // itself — synced from Atomic storage, or from a local checkout
            // with --from. This supersedes the adapter's built-in install,
            // which remains as a fallback if the package can't be fetched.
            if let Some(spec) = atomic_agent::integrations::resolve(agent_name) {
                let receipt = Receipt::load(agent_name).ok().flatten();

                if let Some(ref receipt) = receipt {
                    if !self.force {
                        if *agent_name == "codex" {
                            match repair_stale_codex_receipt_hooks(receipt) {
                                Ok(Some(count)) => {
                                    print_warning(
                                        "Codex integration receipt exists, but its hook set is outdated; repaired the receipt-managed hooks from the current Atomic binary.",
                                    );
                                    print_success(&format!(
                                        "Repaired {} hook{} for {}",
                                        count,
                                        if count == 1 { "" } else { "s" },
                                        agent.display_name(),
                                    ));
                                    total_installed += count;
                                    continue;
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    return Err(crate::error::CliError::Internal(anyhow::anyhow!(
                                        "Failed to repair receipt-managed Codex hooks: {}",
                                        error
                                    )));
                                }
                            }
                        }

                        println!(
                            "  ✓ integration already installed for {}. Use --force to refresh.",
                            agent.display_name(),
                        );
                        continue;
                    }
                }

                match self.install_integration(agent_name, &spec) {
                    Ok(outcome) => {
                        print_success(&format!(
                            "Installed {} integration v{} ({} new, {} refreshed, {} skipped)",
                            agent.display_name(),
                            outcome.version,
                            outcome.installed.len(),
                            outcome.refreshed.len(),
                            outcome.skipped.len(),
                        ));
                        for skipped in &outcome.skipped {
                            println!("    keep: {} ({})", skipped.dst.display(), skipped.reason);
                        }
                        for merged in &outcome.settings {
                            println!(
                                "    settings: {} hook command(s) → {}",
                                merged.added,
                                merged.target.display()
                            );
                        }
                        total_installed += 1;
                        continue;
                    }
                    Err(e) => {
                        print_warning(&format!(
                            "Integration install for {} failed: {} — falling back to built-in hooks.",
                            agent.display_name(),
                            e
                        ));
                    }
                }
            }

            // Check if already installed (and not forcing)
            if !self.force && agent.is_installed(&repo_root) {
                println!(
                    "  ✓ hooks already installed for {}. Use --force to reinstall.",
                    agent.display_name(),
                );
                continue;
            }

            // If forcing, uninstall first
            if self.force && agent.is_installed(&repo_root) {
                if let Err(e) = agent.uninstall(&repo_root) {
                    print_warning(&format!(
                        "Failed to remove existing hooks for {}: {}",
                        agent.display_name(),
                        e
                    ));
                }
            }

            // Install
            match agent.install(&repo_root) {
                Ok(count) => {
                    if count > 0 {
                        print_success(&format!(
                            "Installed {} hook{} for {}",
                            count,
                            if count == 1 { "" } else { "s" },
                            agent.display_name(),
                        ));
                        total_installed += count;
                    } else {
                        println!(
                            "  ✓ already up to date for {}. Use --force to reinstall.",
                            agent.display_name(),
                        );
                    }
                }
                Err(e) => {
                    print_error(&format!(
                        "Failed to install hooks for {}: {}",
                        agent.display_name(),
                        e
                    ));
                }
            }
        }

        // Summary
        if total_installed > 0 {
            println!();
            println!("Each agent turn will be recorded as an Atomic change with:");
            println!("  • AI provenance (vendor, model, tokens, cost)");
            println!("  • Session metadata (turn number, timing, files)");
            println!("  • Optional transcript (full conversation)");
            println!();
            println!("Use 'atomic agent status' to check integration status.");
            println!("Use 'atomic log' to view recorded turns.");
        }

        Ok(())
    }
}

/// Sync the shared atomic-skills package into the CLI-managed cache
/// (`~/.atomic/integrations/atomic-skills/repo`). Skills and the canonical
/// AGENTS.md are sourced from this cache, so every plugin install reuses it.
///
/// First run clones the package; later runs reuse the cache. `--force`
/// discards and re-clones.
fn sync_skills_cache(force: bool) -> CliResult<std::path::PathBuf> {
    const SKILLS_AGENT: &str = "atomic-skills";

    let spec = atomic_agent::integrations::resolve(SKILLS_AGENT).ok_or_else(|| {
        crate::error::CliError::Internal(anyhow::anyhow!(
            "atomic-skills not found in the integration registry — this is a bug"
        ))
    })?;

    sync_integration_package(SKILLS_AGENT, &spec, force)
}

/// Sync an agent's integration package into the CLI-managed cache
/// (`~/.atomic/integrations/<agent>/repo`) using Atomic's own remote
/// protocol, and return the package directory (the cache's working copy).
///
/// First run clones the package; later runs reuse the cache so enable works
/// offline. `--force` discards the cache and re-clones.
fn sync_integration_package(
    agent_name: &str,
    spec: &atomic_agent::integrations::IntegrationSpec,
    force: bool,
) -> CliResult<std::path::PathBuf> {
    let cache = atomic_agent::integrations::cache_repo_dir(agent_name)
        .map_err(|e| crate::error::CliError::Internal(anyhow::anyhow!(e.to_string())))?;

    if force && cache.exists() {
        std::fs::remove_dir_all(&cache).map_err(crate::error::CliError::Io)?;
    }

    if cache.exists() {
        println!(
            "Using cached package at {} (use --force to refresh).",
            cache.display()
        );
    } else {
        if let Some(ref tag) = spec.tag {
            print_warning(&format!(
                "Tag pinning ({tag}) is not yet implemented — installing the head of view '{}' instead.",
                spec.view
            ));
        }
        crate::commands::clone::Clone::new(spec.url.clone())
            .with_path(cache.display().to_string())
            .with_view(spec.view.clone())
            .run()?;
    }

    Ok(cache)
}

impl Enable {
    /// Install an externally-packaged integration for an agent.
    ///
    /// The package directory comes from `--from` (a local checkout) or from
    /// syncing the registry's Atomic storage project into the CLI-managed
    /// cache. Installation itself is done by the integrations engine per the
    /// package's atomic-integration.toml.
    ///
    /// When the manifest declares `[skills-source]`, the shared atomic-skills
    /// cache is synced (or reused) and passed as `skills_cache_dir`. When the
    /// user opts in via `--agents-md` or the prompt, the repo root is passed
    /// so `[[repo-file]]` entries land in the repo.
    fn install_integration(
        &self,
        agent_name: &str,
        spec: &atomic_agent::integrations::IntegrationSpec,
    ) -> CliResult<atomic_agent::integrations::InstallOutcome> {
        let source;
        let pkg_dir = if let Some(ref from) = self.from {
            source = from.display().to_string();
            from.clone()
        } else {
            source = spec.url.clone();
            sync_integration_package(agent_name, spec, self.force)?
        };

        // Peek at the manifest to see if we need the skills cache.
        let manifest = atomic_agent::integrations::IntegrationManifest::load(&pkg_dir)
            .map_err(|e| crate::error::CliError::Internal(anyhow::anyhow!(e.to_string())))?;

        let skills_cache_dir = if manifest.skills_source.is_some()
            || manifest.agent_definition.is_some()
        {
            Some(sync_skills_cache(self.force)?)
        } else {
            None
        };

        // Determine repo_root from the --agents-md / --no-agents-md flags
        // or the interactive prompt. Only relevant when the manifest has
        // [[repo-file]] entries.
        let repo_root = if !manifest.repo_files.is_empty() {
            self.resolve_repo_root()?
        } else {
            None
        };

        let opts = atomic_agent::integrations::InstallOptions {
            force: self.force,
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            source,
            expect_agent: Some(agent_name.to_string()),
            skills_cache_dir,
            repo_root,
        };

        atomic_agent::integrations::install_from_dir(&pkg_dir, &opts)
            .map_err(|e| crate::error::CliError::Internal(anyhow::anyhow!(e.to_string())))
    }

    /// Sync the shared atomic-skills package into the CLI-managed cache.
    ///
    /// First run clones it; later runs reuse the cache so installs work
    /// offline. `--force` discards the cache and re-clones.
    fn resolve_repo_root(&self) -> CliResult<Option<std::path::PathBuf>> {
        // If the user explicitly said no, skip.
        if self.no_agents_md {
            return Ok(None);
        }
        // If the user explicitly said yes, find the repo root and proceed.
        if self.agents_md {
            let repo_root = find_repository_root()?;
            return Ok(Some(repo_root));
        }
        // Otherwise prompt (interactive TTY only; non-interactive defaults to no).
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            println!("Skipping repo AGENTS.md (non-interactive). Use --agents-md to install.");
            return Ok(None);
        }
        print!(
            "Also install/merge AGENTS.md into this repo so the workflow is always-on\n\
             without picking the Atomic agent? [y/N] "
        );
        use std::io::Write;
        std::io::stdout().flush().ok();

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| crate::error::CliError::Internal(anyhow::anyhow!(e.to_string())))?;
        let input = input.trim().to_lowercase();
        if input == "y" || input == "yes" {
            let repo_root = find_repository_root()?;
            Ok(Some(repo_root))
        } else {
            Ok(None)
        }
    }

    /// Install hooks from an integration-supplied manifest file.
    ///
    /// The manifest names its own target settings file and the hook commands to
    /// register; this merges them in idempotently, preserving any non-Atomic
    /// hooks. The hook definitions live in the integration repo, so this path
    /// never needs the per-agent definitions baked into this binary.
    fn run_manifest(&self, manifest: &std::path::Path) -> CliResult<()> {
        use atomic_agent::hooks::manifest::install_from_manifest;

        match install_from_manifest(manifest) {
            Ok(outcome) => {
                print_success(&format!(
                    "Installed hooks from {} → {}",
                    manifest.display(),
                    outcome.target.display(),
                ));
                let mut summary = format!("{} hook command(s) registered", outcome.added);
                if outcome.removed > 0 {
                    summary.push_str(&format!(", {} stale entr(y/ies) replaced", outcome.removed));
                }
                println!("  {summary}");
                println!();
                println!("Each agent turn in a project with .atomic/ will be recorded");
                println!("as an Atomic change with full AI provenance.");
            }
            Err(e) => {
                print_error(&format!(
                    "Failed to install hooks from manifest {}: {}",
                    manifest.display(),
                    e,
                ));
            }
        }
        Ok(())
    }

    /// Install hooks globally to the agent's user-level settings.
    ///
    /// Supports:
    /// - `claude-code` → `~/.claude/settings.json`
    /// - `gemini-cli` → `~/.gemini/settings.json`
    /// - `codex` → `~/.codex/hooks.json`
    ///
    /// agy is intentionally absent: its plugin is inherently global and is
    /// installed by the integrations engine via the standard `enable` path.
    fn run_global(&self) -> CliResult<()> {
        use atomic_agent::hooks::claude_code::ClaudeCodeHook;
        use atomic_agent::hooks::codex::CodexHook;
        use atomic_agent::hooks::gemini_cli::GeminiCliHook;

        let agent_name = self.agent.as_deref().unwrap_or("claude-code");

        match agent_name {
            "claude-code" => {
                let hook = ClaudeCodeHook::new();

                if !self.force && hook.is_installed_global() {
                    print_success("Global hooks already installed in ~/.claude/settings.json.");
                    println!("  Use --force to reinstall.");
                    return Ok(());
                }

                match hook.install_global(self.force) {
                    Ok(count) if count > 0 => {
                        print_success(&format!(
                            "Installed {} global hook{} for Claude Code",
                            count,
                            if count == 1 { "" } else { "s" },
                        ));
                        println!();
                        println!("Hooks written to: ~/.claude/settings.json");
                        println!();
                        println!("Every Claude Code session in a project with .atomic/ will now:");
                        println!("  • Record each turn as an Atomic change with full provenance");
                        println!("  • Track session metadata (turn number, timing, files)");
                        println!("  • Create attestations at session end");
                    }
                    Ok(_) => {
                        print_success("Global hooks already up to date.");
                    }
                    Err(e) => {
                        print_error(&format!("Failed to install global hooks: {}", e));
                    }
                }
            }

            "gemini-cli" => {
                let hook = GeminiCliHook::new();

                if !self.force && hook.is_installed_global() {
                    print_success("Global hooks already installed in ~/.gemini/settings.json.");
                    println!("  Use --force to reinstall.");
                    return Ok(());
                }

                match hook.install_global(self.force) {
                    Ok(count) if count > 0 => {
                        print_success(&format!(
                            "Installed {} global hook{} for Gemini CLI",
                            count,
                            if count == 1 { "" } else { "s" },
                        ));
                        println!();
                        println!("Hooks written to: ~/.gemini/settings.json");
                        println!();
                        println!("Every Gemini CLI session in a project with .atomic/ will now:");
                        println!("  • Record each turn as an Atomic change with full provenance");
                        println!("  • Track session metadata (turn number, timing, files)");
                        println!("  • Create attestations at session end");
                    }
                    Ok(_) => {
                        print_success("Global hooks already up to date.");
                    }
                    Err(e) => {
                        print_error(&format!("Failed to install global hooks: {}", e));
                    }
                }
            }

            "agy" => {
                // agy's plugin is inherently global — the integration package
                // install (the standard repo-scoped `enable` path) *is* the
                // global install.
                println!("The agy plugin is global by nature — install it with:");
                println!("  atomic agent enable --agent agy");
                println!();
                println!("(No --global needed; the integrations engine stages the plugin");
                println!(" at ~/.gemini/config/plugins/atomic/ from the atomic-agy package.)");
            }

            "codex" => {
                let hook = CodexHook::new();

                if !self.force && hook.is_installed_global() {
                    print_success("Global hooks already installed in ~/.codex/hooks.json.");
                    println!("  Use --force to reinstall.");
                    return Ok(());
                }

                match hook.install_global(self.force) {
                    Ok(count) if count > 0 => {
                        print_success(&format!(
                            "Installed {} global hook{} for Codex",
                            count,
                            if count == 1 { "" } else { "s" },
                        ));
                        println!();
                        println!("Hooks written to: ~/.codex/hooks.json");
                        println!();
                        println!("Every Codex session in a project with .atomic/ will now:");
                        println!(
                            "  \u{2022} Record each turn as an Atomic change with full provenance"
                        );
                        println!("  \u{2022} Track session metadata (turn number, timing, files)");
                        println!("  \u{2022} Capture tool calls through pre/post tool hooks");
                    }
                    Ok(_) => {
                        print_success("Global hooks already up to date.");
                    }
                    Err(e) => {
                        print_error(&format!("Failed to install global hooks: {}", e));
                    }
                }
            }

            "kiro" => {
                print_success("Kiro hooks are configured through the IDE panel.");
                println!();
                println!("Install the atomic-kiro integration (skills, steering, hook scripts):");
                println!("  atomic agent enable --agent kiro");
                println!();
                println!(
                    "Then configure hooks in Kiro IDE \u{2192} Agent Steering & Skills panel."
                );
            }

            other => {
                print_warning(&format!(
                    "Global install is not supported for '{}'. Supported agents: claude-code, gemini-cli, codex, kiro (agy via plain enable)",
                    other
                ));
            }
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enable_default_for_test() {
        let cmd = Enable::default_for_test();
        assert!(cmd.agent.is_none());
        assert!(!cmd.force);
        assert!(!cmd.all);
    }

    #[test]
    fn test_enable_no_repo_fails() {
        // Running enable outside a repo should fail in find_repository_root
        // We can't easily test this without changing the working directory,
        // so we just verify the command struct is constructible.
        let _cmd = Enable {
            agent: Some("claude-code".to_string()),
            force: false,
            all: false,
            global: false,
            hooks: None,
            from: None,
            agents_md: false,
            no_agents_md: false,
        };
    }

    #[test]
    fn test_enable_with_force_flag() {
        let cmd = Enable {
            agent: Some("claude-code".to_string()),
            force: true,
            all: false,
            global: false,
            hooks: None,
            from: None,
            agents_md: false,
            no_agents_md: false,
        };
        assert!(cmd.force);
        assert_eq!(cmd.agent.as_deref(), Some("claude-code"));
    }

    #[test]
    fn test_enable_with_all_flag() {
        let cmd = Enable {
            agent: None,
            force: false,
            all: true,
            global: false,
            hooks: None,
            from: None,
            agents_md: false,
            no_agents_md: false,
        };
        assert!(cmd.all);
        assert!(cmd.agent.is_none());
    }

    #[test]
    fn test_enable_with_global_flag() {
        let cmd = Enable {
            agent: Some("claude-code".to_string()),
            force: false,
            all: false,
            global: true,
            hooks: None,
            from: None,
            agents_md: false,
            no_agents_md: false,
        };
        assert!(cmd.global);
        assert_eq!(cmd.agent.as_deref(), Some("claude-code"));
    }

    #[test]
    fn test_enable_install_to_temp_repo() {
        let dir = tempfile::TempDir::new().unwrap();

        // Create a minimal .atomic directory so find_repository_root would work
        std::fs::create_dir_all(dir.path().join(".atomic")).unwrap();

        // Create .claude directory so the agent is detected
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();

        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("claude-code").unwrap();

        // Install hooks
        let count = agent.install(dir.path()).unwrap();
        assert_eq!(count, 8);
        assert!(agent.is_installed(dir.path()));

        // Verify .claude/settings.json was created
        let settings_path = dir.path().join(".claude").join("settings.json");
        assert!(settings_path.exists());

        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(content.contains("atomic agent hooks claude-code"));
    }

    #[test]
    fn test_enable_force_reinstall() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomic")).unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();

        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("claude-code").unwrap();

        // First install
        let count1 = agent.install(dir.path()).unwrap();
        assert_eq!(count1, 8);

        // Second install without force — should be 0
        let count2 = agent.install(dir.path()).unwrap();
        assert_eq!(count2, 0);

        // Uninstall and reinstall (simulating --force)
        agent.uninstall(dir.path()).unwrap();
        assert!(!agent.is_installed(dir.path()));

        let count3 = agent.install(dir.path()).unwrap();
        assert_eq!(count3, 8);
        assert!(agent.is_installed(dir.path()));
    }

    #[test]
    fn test_receipt_managed_codex_repair_uses_recorded_target() {
        let dir = tempfile::TempDir::new().unwrap();
        let global_target = dir.path().join("home/.codex/hooks.json");
        std::fs::create_dir_all(global_target.parent().unwrap()).unwrap();
        std::fs::write(
            &global_target,
            serde_json::to_vec_pretty(&serde_json::json!({
                "hooks": {
                    "Stop": [{
                        "hooks": [{
                            "type": "command",
                            "command": "test -d .atomic || test -f .atomic-sandbox && atomic agent hooks codex stop || true"
                        }]
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let receipt: Receipt = serde_json::from_value(serde_json::json!({
            "schema": 1,
            "agent": "codex",
            "version": "0.1.0",
            "cli_version": "0.12.1",
            "installed_at": "2026-08-04T00:00:00Z",
            "source": "test",
            "files": [],
            "settings": [{
                "target": global_target,
                "hooks_key": "hooks",
                "command_prefix": "atomic agent hooks codex"
            }]
        }))
        .unwrap();

        let repaired = repair_stale_codex_receipt_hooks(&receipt).unwrap();
        assert!(repaired.is_some_and(|count| count > 0));
        assert!(CodexHook::new().is_installed_at(&global_target));
        assert!(!dir.path().join("repo/.codex/hooks.json").exists());
    }
}

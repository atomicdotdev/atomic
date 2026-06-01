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

use clap::Args;

use atomic_agent::hooks::AgentRegistry;

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
        }
    }
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
                // If none detected, try all registered agents
                print_warning("No agents detected — installing hooks for all registered agents.");
                registry.list()
            } else {
                detected
            }
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

                // Default to the first available agent (claude-code)
                print_warning(&format!(
                    "No agent detected in this repository. Defaulting to '{}'.",
                    available[0],
                ));
                print_warning(
                    "Create a .claude/ or .gemini/ directory first, or use --agent <name>.",
                );
                vec![available[0]]
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
            let has_opencode = agents_to_install.contains(&"opencode");
            println!();
            println!("Each agent turn will be recorded as an Atomic change with:");
            println!("  • AI provenance (vendor, model, tokens, cost)");
            println!("  • Session metadata (turn number, timing, files)");
            println!("  • Optional transcript (full conversation)");
            if has_opencode {
                println!();
                println!("For the full OpenCode integration (agent, skills, provenance):");
                println!("  npm install -g atomic-opencode && npx atomic-opencode");
            }
            println!();
            println!("Use 'atomic agent status' to check integration status.");
            println!("Use 'atomic log' to view recorded turns.");
        }

        Ok(())
    }
}

impl Enable {
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
                        println!("  • Record each turn as an Atomic change with full provenance");
                        println!("  • Track session metadata (turn number, timing, files)");
                        println!("  • Capture tool calls through pre/post tool hooks");
                    }
                    Ok(_) => {
                        print_success("Global hooks already up to date.");
                    }
                    Err(e) => {
                        print_error(&format!("Failed to install global hooks: {}", e));
                    }
                }
            }

            other => {
                print_warning(&format!(
                    "Global install is not supported for '{}'. Supported agents: claude-code, gemini-cli, codex",
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
}

//! `atomic agent disable` command implementation.
//!
//! Removes Atomic hooks from the AI coding agent's configuration file.
//! Non-Atomic hooks in the agent's config are preserved. Session state
//! files in `.atomic/sessions/` are not deleted.
//!
//! # Examples
//!
//! ```text
//! # Disable for the auto-detected agent
//! atomic agent disable
//!
//! # Disable for a specific agent
//! atomic agent disable --agent claude-code
//!
//! # Disable for all installed agents
//! atomic agent disable --all
//! ```

use clap::Args;

use atomic_agent::hooks::AgentRegistry;

use crate::commands::{find_repository_root, Command};
use crate::error::CliResult;
use crate::output::{print_error, print_success, print_warning};

// Disable Command

/// Remove agent hooks from the repository.
///
/// Removes Atomic hooks from the agent's configuration file while
/// preserving any non-Atomic hooks. Session state files in
/// `.atomic/sessions/` are not deleted — use `atomic agent sessions clean`
/// for that.
#[derive(Debug, Args)]
pub struct Disable {
    /// Which agent to remove hooks for.
    ///
    /// If not specified, removes hooks for all agents that have them
    /// installed.
    #[arg(long, value_name = "NAME")]
    agent: Option<String>,

    /// Remove hooks for all installed agents.
    ///
    /// Equivalent to running `disable` for each agent that has hooks
    /// currently installed.
    #[arg(long)]
    all: bool,

    /// Remove hooks from global ~/.claude/settings.json.
    ///
    /// Removes the globally installed hooks that were added with
    /// `atomic agent enable --global`.
    #[arg(short, long)]
    global: bool,

    /// Remove hooks described by an integration-supplied manifest file.
    ///
    /// Reads the same manifest used by `atomic agent enable --hooks`, and
    /// removes only the entries whose command matches the manifest's
    /// `command_prefix` from its target file. Non-Atomic hooks are preserved.
    #[arg(long, value_name = "FILE")]
    hooks: Option<std::path::PathBuf>,
}

impl Disable {
    /// Create a default instance for testing.
    #[cfg(test)]
    pub(crate) fn default_for_test() -> Self {
        Self {
            agent: None,
            all: false,
            global: false,
            hooks: None,
        }
    }
}

impl Command for Disable {
    fn run(&self) -> CliResult<()> {
        // Data-driven uninstall — mirrors `enable --hooks`.
        if let Some(ref manifest) = self.hooks {
            return self.run_manifest(manifest);
        }

        // Global uninstall — removes from ~/.claude/settings.json
        if self.global {
            return self.run_global();
        }

        let repo_root = find_repository_root()?;

        let registry = AgentRegistry::with_defaults();

        // Determine which agents to uninstall
        let agents_to_disable: Vec<&str> = if self.all {
            // Uninstall from all agents that have hooks installed
            let installed = registry.installed(&repo_root);
            if installed.is_empty() {
                println!("No agent hooks are currently installed.");
                return Ok(());
            }
            installed
        } else if let Some(ref name) = self.agent {
            // Specific agent requested — validate it exists
            registry
                .require(name)
                .map_err(|e| crate::error::CliError::InvalidArgument {
                    message: format!("Unknown agent '{}': {}", name, e),
                })?;
            vec![name.as_str()]
        } else {
            // Auto: uninstall from all agents that have hooks installed
            let installed = registry.installed(&repo_root);
            if installed.is_empty() {
                println!("No agent hooks are currently installed.");
                println!("Use 'atomic agent enable' to install hooks first.");
                return Ok(());
            }
            installed
        };

        // Uninstall hooks for each selected agent
        let mut total_removed = 0;

        for agent_name in &agents_to_disable {
            let agent = match registry.get(agent_name) {
                Some(a) => a,
                None => {
                    print_warning(&format!(
                        "Agent '{}' not found in registry — skipping.",
                        agent_name,
                    ));
                    continue;
                }
            };

            if !agent.is_installed(&repo_root) {
                println!(
                    "  No Atomic hooks installed for {} — skipping.",
                    agent.display_name(),
                );
                continue;
            }

            match agent.uninstall(&repo_root) {
                Ok(()) => {
                    print_success(&format!("Removed hooks for {}", agent.display_name(),));
                    total_removed += 1;
                }
                Err(e) => {
                    print_error(&format!(
                        "Failed to remove hooks for {}: {}",
                        agent.display_name(),
                        e,
                    ));
                }
            }
        }

        if total_removed > 0 {
            println!();
            println!("Agent turns will no longer be recorded automatically.");
            println!("Session state files in .atomic/sessions/ were preserved.");
            println!("Use 'atomic agent enable' to re-enable.");
        }

        Ok(())
    }
}

impl Disable {
    /// Remove hooks described by an integration-supplied manifest file.
    ///
    /// Mirrors `atomic agent enable --hooks`: removes only the entries whose
    /// command matches the manifest's `command_prefix` from its target file.
    fn run_manifest(&self, manifest: &std::path::Path) -> CliResult<()> {
        use atomic_agent::hooks::manifest::uninstall_from_manifest;

        match uninstall_from_manifest(manifest) {
            Ok(outcome) => {
                if outcome.removed > 0 {
                    print_success(&format!(
                        "Removed {} hook command(s) from {}",
                        outcome.removed,
                        outcome.target.display(),
                    ));
                    println!();
                    println!("Agent turns will no longer be recorded automatically.");
                } else {
                    println!("No Atomic hooks found in {}.", outcome.target.display());
                }
            }
            Err(e) => {
                print_error(&format!(
                    "Failed to remove hooks from manifest {}: {}",
                    manifest.display(),
                    e,
                ));
            }
        }
        Ok(())
    }

    /// Remove hooks from global agent settings.
    ///
    /// Supports:
    /// - `claude-code` → `~/.claude/settings.json`
    /// - `gemini-cli` → `~/.gemini/settings.json`
    /// - No `--agent` flag → removes from all agents that have global hooks
    fn run_global(&self) -> CliResult<()> {
        use atomic_agent::hooks::claude_code::ClaudeCodeHook;
        use atomic_agent::hooks::gemini_cli::GeminiCliHook;

        let agents: Vec<&str> = if let Some(ref name) = self.agent {
            vec![name.as_str()]
        } else {
            // No agent specified — check all supported agents
            let mut found = Vec::new();
            if ClaudeCodeHook::new().is_installed_global() {
                found.push("claude-code");
            }
            if GeminiCliHook::new().is_installed_global() {
                found.push("gemini-cli");
            }
            found
        };

        if agents.is_empty() {
            println!("No global hooks installed.");
            return Ok(());
        }

        let mut total_removed = 0;

        for agent_name in &agents {
            match *agent_name {
                "claude-code" => {
                    let hook = ClaudeCodeHook::new();
                    if !hook.is_installed_global() {
                        continue;
                    }
                    match hook.uninstall_global() {
                        Ok(()) => {
                            print_success("Removed global hooks from ~/.claude/settings.json");
                            total_removed += 1;
                        }
                        Err(e) => {
                            print_error(&format!(
                                "Failed to remove Claude Code global hooks: {}",
                                e
                            ));
                        }
                    }
                }
                "gemini-cli" => {
                    let hook = GeminiCliHook::new();
                    if !hook.is_installed_global() {
                        continue;
                    }
                    match hook.uninstall_global() {
                        Ok(()) => {
                            print_success("Removed global hooks from ~/.gemini/settings.json");
                            total_removed += 1;
                        }
                        Err(e) => {
                            print_error(&format!(
                                "Failed to remove Gemini CLI global hooks: {}",
                                e
                            ));
                        }
                    }
                }
                "kiro" => {
                    print_success("Kiro hooks are configured through the IDE panel.");
                    println!(
                        "  Remove hooks manually in Kiro IDE \u{2192} Agent Steering & Skills."
                    );
                    println!("  Uninstall skills/steering: npx atomic-kiro --uninstall");
                }
                other => {
                    print_warning(&format!(
                        "Global disable is not supported for '{}'. Supported agents: claude-code, gemini-cli, kiro",
                        other
                    ));
                }
            }
        }

        if total_removed > 0 {
            println!();
            println!("Agent turns will no longer be recorded automatically.");
            println!("Use 'atomic agent enable --global' to re-enable.");
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disable_default_for_test() {
        let cmd = Disable::default_for_test();
        assert!(cmd.agent.is_none());
        assert!(!cmd.all);
        assert!(!cmd.global);
    }

    #[test]
    fn test_disable_with_agent() {
        let cmd = Disable {
            agent: Some("claude-code".to_string()),
            all: false,
            global: false,
            hooks: None,
        };
        assert_eq!(cmd.agent.as_deref(), Some("claude-code"));
    }

    #[test]
    fn test_disable_with_all() {
        let cmd = Disable {
            agent: None,
            all: true,
            global: false,
            hooks: None,
        };
        assert!(cmd.all);
    }

    #[test]
    fn test_disable_with_global() {
        let cmd = Disable {
            agent: None,
            all: false,
            global: true,
            hooks: None,
        };
        assert!(cmd.global);
    }

    #[test]
    fn test_disable_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomic")).unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();

        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("claude-code").unwrap();

        // Install hooks first
        let count = agent.install(dir.path()).unwrap();
        assert_eq!(count, 8);
        assert!(agent.is_installed(dir.path()));

        // Uninstall
        agent.uninstall(dir.path()).unwrap();
        assert!(!agent.is_installed(dir.path()));

        // Settings file should still exist (with non-Atomic content or empty hooks)
        let settings_path = dir.path().join(".claude").join("settings.json");
        assert!(settings_path.exists());

        // Verify no Atomic hooks remain
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(!content.contains("atomic agent hooks"));
    }

    #[test]
    fn test_disable_preserves_non_atomic_hooks() {
        let dir = tempfile::TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        // Write settings with both Atomic and non-Atomic hooks
        let settings = serde_json::json!({
            "hooks": {
                "Stop": [
                    {
                        "matcher": "",
                        "hooks": [
                            {"type": "command", "command": "my-custom-hook --on-stop"},
                            {"type": "command", "command": "atomic agent hooks claude-code stop"}
                        ]
                    }
                ]
            }
        });
        std::fs::write(
            claude_dir.join("settings.json"),
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        let registry = AgentRegistry::with_defaults();
        let agent = registry.get("claude-code").unwrap();

        // Uninstall
        agent.uninstall(dir.path()).unwrap();

        // Non-Atomic hook should be preserved
        let content = std::fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        assert!(content.contains("my-custom-hook --on-stop"));
        assert!(!content.contains("atomic agent hooks"));
    }
}

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

/// Remove agent hooks.
///
/// Removes Atomic hooks from the agent's configuration file while
/// preserving any non-Atomic hooks — from the repository's config, the
/// home config, and any installed integration package. Session state
/// files in `.atomic/sessions/` are not deleted — use
/// `atomic agent sessions clean` for that.
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
            hooks: None,
        }
    }
}

/// Agents with anything to uninstall: adapter-reported installs (hooks in
/// agent config, at the uninstall base or in the home config) plus agents
/// with an integration receipt (package files).
fn agents_with_installs(registry: &AgentRegistry, install_base: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = registry
        .installed(install_base)
        .iter()
        .map(|s| s.to_string())
        .collect();

    // Hooks also live in the home config: the integration packages target
    // it directly, and the pre-package global installs put them there too.
    if let Some(home) = dirs::home_dir() {
        for name in registry.installed(&home) {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }

    for name in atomic_agent::integrations::registered_integrations() {
        if names.iter().any(|n| n == &name) {
            continue;
        }
        if let Ok(Some(_)) = atomic_agent::integrations::Receipt::load(&name) {
            names.push(name);
        }
    }
    names
}

impl Command for Disable {
    fn run(&self) -> CliResult<()> {
        // Data-driven uninstall — mirrors `enable --hooks`.
        if let Some(ref manifest) = self.hooks {
            return self.run_manifest(manifest);
        }

        // Uninstall base: the repo root inside a repository, or the home
        // directory outside one — mirroring `enable`. Hooks may also live
        // in the home config regardless (integration packages target it
        // directly), so the loop below always cleans both scopes.
        let install_base = match find_repository_root() {
            Ok(root) => root,
            Err(_) => {
                print_warning(
                    "No Atomic repository found — removing hooks from the home directory.",
                );
                dirs::home_dir().ok_or_else(|| {
                    crate::error::CliError::Io(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "Could not determine the home directory",
                    ))
                })?
            }
        };
        let registry = AgentRegistry::with_defaults();

        // Determine which agents to uninstall
        let agents_to_disable: Vec<String> = if self.all {
            // Uninstall from all agents that have hooks installed
            let installed = agents_with_installs(&registry, &install_base);
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
            vec![name.clone()]
        } else {
            // Auto: uninstall from all agents that have hooks installed
            let installed = agents_with_installs(&registry, &install_base);
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

            let mut removed_any = false;

            // Integration package installed via `enable` — remove it per its
            // receipt (files we own only; user-modified files are kept).
            let receipt_exists = atomic_agent::integrations::Receipt::load(agent_name)
                .map(|r| r.is_some())
                .unwrap_or(false);
            if receipt_exists {
                match atomic_agent::integrations::uninstall(agent_name) {
                    Ok(outcome) => {
                        print_success(&format!(
                            "Removed {} integration ({} file{}, {} kept user-modified, {} hook commands stripped)",
                            agent.display_name(),
                            outcome.removed.len(),
                            if outcome.removed.len() == 1 { "" } else { "s" },
                            outcome.kept_modified.len(),
                            outcome.settings_hooks_removed,
                        ));
                        for kept in &outcome.kept_modified {
                            println!("    keep: {} (modified since install)", kept.display());
                        }
                        removed_any = true;
                    }
                    Err(e) => {
                        print_error(&format!(
                            "Failed to remove {} integration: {}",
                            agent.display_name(),
                            e,
                        ));
                    }
                }
            }

            // Built-in hooks live relative to a base directory: the repo
            // root inside a repository, or the home directory outside one.
            // The home config is also always checked — the integration
            // packages and the pre-package global installs put hooks
            // there — so one `disable` cleans every scope Atomic wrote.
            let mut bases: Vec<std::path::PathBuf> = vec![install_base.clone()];
            if let Some(home) = dirs::home_dir() {
                if !bases.iter().any(|b| b == &home) {
                    bases.push(home);
                }
            }
            for base in &bases {
                if agent.is_installed(base) {
                    match agent.uninstall(base) {
                        Ok(()) => {
                            print_success(&format!(
                                "Removed hooks for {} ({})",
                                agent.display_name(),
                                base.display(),
                            ));
                            removed_any = true;
                        }
                        Err(e) => {
                            print_error(&format!(
                                "Failed to remove hooks for {} ({}): {}",
                                agent.display_name(),
                                base.display(),
                                e,
                            ));
                        }
                    }
                }
            }

            if removed_any {
                total_removed += 1;
            } else {
                println!(
                    "  No Atomic hooks installed for {} — skipping.",
                    agent.display_name(),
                );
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
    }

    #[test]
    fn test_disable_with_agent() {
        let cmd = Disable {
            agent: Some("claude-code".to_string()),
            all: false,
            hooks: None,
        };
        assert_eq!(cmd.agent.as_deref(), Some("claude-code"));
    }

    #[test]
    fn test_disable_with_all() {
        let cmd = Disable {
            agent: None,
            all: true,
            hooks: None,
        };
        assert!(cmd.all);
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

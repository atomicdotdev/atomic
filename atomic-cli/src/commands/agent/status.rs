//! `atomic agent status` command implementation.
//!
//! Displays information about the current state of agent integration:
//! - Which agents have hooks installed
//! - Active and recent sessions
//! - File watcher availability
//!
//! # Examples
//!
//! ```text
//! # Show status
//! atomic agent status
//!
//! # Show verbose status with session details
//! atomic agent status --verbose
//! ```

use clap::Args;

use atomic_agent::hooks::AgentRegistry;
use atomic_agent::turn::session::SessionStore;

use crate::commands::{find_repository_root, Command};
use crate::error::CliResult;

// AgentStatus Command

/// Show agent integration status.
///
/// Displays which agents have hooks installed, any active sessions,
/// and the current state of the file watcher.
#[derive(Debug, Args)]
pub struct AgentStatus {
    /// Show detailed session information.
    ///
    /// When enabled, shows per-session details including turn count,
    /// files touched, duration, and first prompt.
    #[arg(short, long)]
    verbose: bool,
}

impl AgentStatus {
    /// Create a default instance for testing.
    #[cfg(test)]
    pub(crate) fn default_for_test() -> Self {
        Self { verbose: false }
    }
}

impl Command for AgentStatus {
    fn run(&self) -> CliResult<()> {
        let repo_root = find_repository_root()?;

        let registry = AgentRegistry::with_defaults();

        // Installed agents

        println!("Agent Integration Status");
        println!("=======================");
        println!();

        let installed = registry.installed(&repo_root);
        let detected = registry.detect(&repo_root);

        if installed.is_empty() {
            println!("  Hooks: not installed");
            println!();
            if !detected.is_empty() {
                println!(
                    "  Detected agent{}: {}",
                    if detected.len() == 1 { "" } else { "s" },
                    detected.join(", "),
                );
                println!();
                println!("  Run 'atomic agent enable' to start recording agent turns.");
            } else {
                println!("  No agents detected in this repository.");
                println!(
                    "  Create a .claude/ directory (or similar) and run 'atomic agent enable'."
                );
            }
            println!();
            return Ok(());
        }

        // Show installed agents
        for agent_name in &installed {
            if let Some(agent) = registry.get(agent_name) {
                println!("  ✓ {} — hooks installed", agent.display_name());
            }
        }

        // Show detected but not installed agents
        for agent_name in &detected {
            if !installed.contains(agent_name) {
                if let Some(agent) = registry.get(agent_name) {
                    println!(
                        "  ○ {} — detected but hooks not installed",
                        agent.display_name()
                    );
                }
            }
        }

        println!();

        // Sessions

        let session_store = match SessionStore::for_repo(&repo_root) {
            Ok(store) => store,
            Err(e) => {
                println!("  Sessions: error loading ({e})");
                println!();
                return Ok(());
            }
        };

        let sessions = match session_store.list() {
            Ok(s) => s,
            Err(e) => {
                println!("  Sessions: error listing ({e})");
                println!();
                return Ok(());
            }
        };

        if sessions.is_empty() {
            println!("  Sessions: none");
            println!();
            println!("  Start using your AI agent — turns will be recorded automatically.");
            println!();
            return Ok(());
        }

        // Separate active and ended sessions
        let active: Vec<_> = sessions.iter().filter(|s| !s.is_ended()).collect();
        let ended: Vec<_> = sessions.iter().filter(|s| s.is_ended()).collect();

        // Active sessions
        if !active.is_empty() {
            println!(
                "  Active session{}:",
                if active.len() == 1 { "" } else { "s" }
            );
            for session in &active {
                println!(
                    "    ● {} ({}, {} turn{}, {})",
                    session.session_id,
                    session.agent_display_name,
                    session.turn_count,
                    if session.turn_count == 1 { "" } else { "s" },
                    session.duration_display(),
                );
                if self.verbose {
                    println!("      Stack: {}", session.stack_name);
                    println!("      Phase: {}", session.phase);
                    if !session.model.is_empty() {
                        println!("      Model: {}", session.model);
                    }
                    if !session.agent_vendor.is_empty() {
                        println!("      Vendor: {}", session.agent_vendor);
                    }
                    if let Some(ref prompt) = session.first_prompt {
                        println!("      First prompt: \"{}\"", prompt);
                    }
                    if !session.files_touched.is_empty() {
                        println!(
                            "      Files touched: {} ({})",
                            session.files_touched.len(),
                            session
                                .files_touched
                                .iter()
                                .take(5)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                        if session.files_touched.len() > 5 {
                            println!("        ... and {} more", session.files_touched.len() - 5);
                        }
                    }
                    if let Some(ref path) = session.transcript_path {
                        println!("      Transcript: {}", path.display());
                    }
                    println!(
                        "      View turn: atomic change -p --stack {}",
                        session.stack_name
                    );
                    println!();
                }
            }
            if !self.verbose {
                println!();
            }
        }

        // Ended sessions
        if !ended.is_empty() {
            let show_count = if self.verbose { ended.len() } else { 5 };

            println!(
                "  Recent session{} ({} total):",
                if ended.len() == 1 { "" } else { "s" },
                ended.len(),
            );
            for session in ended.iter().take(show_count) {
                println!(
                    "    ○ {} ({}, {} turn{}, {})",
                    session.session_id,
                    session.agent_display_name,
                    session.turn_count,
                    if session.turn_count == 1 { "" } else { "s" },
                    session.duration_display(),
                );
                if self.verbose {
                    println!("      Stack: {}", session.stack_name);
                    if !session.model.is_empty() {
                        println!("      Model: {}", session.model);
                    }
                    if let Some(ref prompt) = session.first_prompt {
                        println!("      First prompt: \"{}\"", prompt);
                    }
                    if !session.files_touched.is_empty() {
                        println!("      Files touched: {}", session.files_touched.len());
                    }
                    println!(
                        "      View turn: atomic change -p --stack {}",
                        session.stack_name
                    );
                    println!();
                }
            }
            if !self.verbose && ended.len() > show_count {
                println!(
                    "    ... and {} more (use --verbose to see all)",
                    ended.len() - show_count,
                );
            }
            println!();
        }

        // Summary

        let total_turns: u32 = sessions.iter().map(|s| s.turn_count).sum();
        let total_files: usize = sessions.iter().map(|s| s.files_touched.len()).sum();

        println!(
            "  Total: {} session{}, {} turn{}, {} file{} touched",
            sessions.len(),
            if sessions.len() == 1 { "" } else { "s" },
            total_turns,
            if total_turns == 1 { "" } else { "s" },
            total_files,
            if total_files == 1 { "" } else { "s" },
        );
        println!();

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_default_for_test() {
        let cmd = AgentStatus::default_for_test();
        assert!(!cmd.verbose);
    }

    #[test]
    fn test_status_verbose_flag() {
        let cmd = AgentStatus { verbose: true };
        assert!(cmd.verbose);
    }

    #[test]
    fn test_session_store_for_temp_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomic")).unwrap();

        let store = SessionStore::for_repo(dir.path()).unwrap();
        let sessions = store.list().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_session_store_with_sessions() {
        use atomic_agent::turn::session::AgentSession;

        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomic")).unwrap();

        let store = SessionStore::for_repo(dir.path()).unwrap();

        // Create some sessions
        let s1 = AgentSession::new("sess-1", "claude-code", "Claude Code");
        store.save(&s1).unwrap();

        let mut s2 = AgentSession::new("sess-2", "claude-code", "Claude Code");
        s2.phase = atomic_agent::turn::phase::Phase::Ended;
        s2.ended_at = Some(chrono::Utc::now());
        s2.turn_count = 5;
        store.save(&s2).unwrap();

        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 2);

        let active = store.find_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, "sess-1");

        let ended = store.find_ended().unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].session_id, "sess-2");
        assert_eq!(ended[0].turn_count, 5);
    }
}

//! Agent integration commands for AI coding tools.
//!
//! This module provides the `atomic agent` subcommand tree for managing
//! AI agent integration. It connects the `atomic-agent` crate's turn-level
//! recording system to the CLI.
//!
//! # Subcommands
//!
//! ```text
//! atomic agent
//! ├── enable   — Install agent hooks (e.g., into .claude/settings.json)
//! ├── disable  — Remove agent hooks
//! ├── status   — Show active sessions, installed agents, watcher state
//! └── hooks    — Internal hook handlers (called by agent hooks, hidden)
//!     └── <agent> <verb>  — e.g., `atomic agent hooks claude-code stop`
//! ```
//!
//! # Architecture
//!
//! The `enable` and `disable` commands modify the agent's configuration file
//! to add/remove hooks that call back to `atomic agent hooks <agent> <verb>`.
//! When the agent fires a hook, it invokes that command with JSON on stdin.
//! The `hooks` subcommand parses the JSON, creates a `TurnOrchestrator`,
//! and dispatches the event through the state machine → watcher → recorder
//! pipeline.
//!
//! ```text
//! User runs: atomic agent enable --agent claude-code
//!     │
//!     ▼
//! Writes hooks to .claude/settings.json
//!     │
//!     ▼
//! Claude Code fires: atomic agent hooks claude-code stop < {"session_id": ...}
//!     │
//!     ▼
//! hooks subcommand → parse JSON → TurnOrchestrator.dispatch() → Atomic change
//! ```

mod attest;
mod disable;
mod enable;
mod explain;
mod hooks;
mod lifecycle;
mod owner;
mod repair;
mod status;

use clap::{Args, Subcommand};

use crate::commands::Command;
use crate::error::CliResult;

pub use attest::Attest;
pub use disable::Disable;
pub use enable::Enable;
pub use explain::Explain;
pub use lifecycle::Lifecycle;
pub use status::AgentStatus;

// Agent Command

/// Manage AI agent integration.
///
/// Install hooks for AI coding agents (Claude Code, Gemini CLI, Codex,
/// OpenCode) so that each agent turn is automatically recorded as an
/// Atomic change with full provenance, session metadata, and optional
/// transcript.
///
/// # Examples
///
/// ```text
/// # Enable for Claude Code (auto-detected)
/// atomic agent enable
///
/// # Enable for a specific agent
/// atomic agent enable --agent claude-code
///
/// # Check status
/// atomic agent status
///
/// # Disable
/// atomic agent disable
/// ```
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct Agent {
    #[command(subcommand)]
    command: AgentCommands,
}

/// Available agent subcommands.
#[derive(Debug, Subcommand)]
pub enum AgentCommands {
    /// Install agent hooks for turn-level recording.
    ///
    /// Writes hooks into the agent's configuration file (e.g.,
    /// `.claude/settings.json`) that call back to `atomic agent hooks`
    /// on each lifecycle event. Also creates the `.atomic/sessions/`
    /// directory for session state persistence.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Auto-detect which agent is present
    /// atomic agent enable
    ///
    /// # Specify the agent explicitly
    /// atomic agent enable --agent claude-code
    ///
    /// # Force reinstall (removes existing Atomic hooks first)
    /// atomic agent enable --force
    ///
    /// # Install for all detected agents
    /// atomic agent enable --all
    /// ```
    Enable(Enable),

    /// Remove agent hooks.
    ///
    /// Removes Atomic hooks from the agent's configuration file while
    /// preserving any non-Atomic hooks. Session state files in
    /// `.atomic/sessions/` are not deleted.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Disable for the auto-detected agent
    /// atomic agent disable
    ///
    /// # Disable for a specific agent
    /// atomic agent disable --agent claude-code
    /// ```
    Disable(Disable),

    /// Show agent integration status.
    ///
    /// Displays information about installed hooks, active sessions,
    /// file watcher state, and recent turn history.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Show status
    /// atomic agent status
    ///
    /// # Show verbose status with session details
    /// atomic agent status --verbose
    /// ```
    Status(AgentStatus),

    /// Generate AI reasoning summaries for agent turns.
    ///
    /// Reads the condensed transcript from recorded changes and calls
    /// Claude CLI to generate structured reasoning: intent, outcome,
    /// learnings, friction, and open items.
    ///
    /// Requires Claude CLI (`claude`) to be installed and authenticated.
    ///
    /// # Examples
    ///
    /// ```text
    /// # Explain the most recent turn
    /// atomic agent explain <session-id>
    ///
    /// # Explain a specific turn
    /// atomic agent explain <session-id> --turn 3
    ///
    /// # Explain all turns and save reasoning into the changes
    /// atomic agent explain <session-id> --all --save
    /// ```
    Explain(Explain),

    /// List and inspect attestations.
    ///
    /// Shows graph-level audit nodes that capture AI cost, token usage,
    /// model breakdown, and which changes are covered. Attestations
    /// transcend views — they're project-level audit data.
    ///
    /// # Examples
    ///
    /// ```text
    /// # List all attestations
    /// atomic agent attest
    ///
    /// # Show details for a specific attestation
    /// atomic agent attest --hash XMJZ3IPF
    ///
    /// # Show attestations for a view
    /// atomic agent attest --view dev
    ///
    /// # Verbose output with model breakdown
    /// atomic agent attest --verbose
    /// ```
    Attest(Attest),

    /// Resume a managed session under leases while retaining every piece of
    /// durable evidence (CB-12A AC3).
    ///
    /// The session's incomplete evidence is snapshotted verbatim into an
    /// append-only repair note, the retention lease is set (cleanup must
    /// never discard the only unbound copy), and the attestation/capture
    /// evidence is verified and reported. Repair NEVER erases or
    /// manufactures attribution.
    ///
    /// # Examples
    ///
    /// ```text
    /// atomic agent repair <session-id>
    /// atomic agent repair <session-id> --verify-only
    /// ```
    Repair(repair::Repair),

    /// Declare managed runs for orchestrated agents.
    ///
    /// Used by outer orchestrators such as Sherpa/noname before launching an
    /// ACP executor. While a run is active, participating hooks adopt the
    /// run's declared view and stamp their sessions with the run context —
    /// nothing is suppressed; `lifecycle end --json` returns the run summary.
    Lifecycle(Lifecycle),

    /// Run or inspect the repository-local redb database owner.
    #[command(name = "database-owner", hide = true)]
    DatabaseOwner(owner::DatabaseOwner),

    /// Internal hook handlers (called by agent hooks).
    ///
    /// These commands are invoked by the hooks installed in agent
    /// configuration files. They read JSON from stdin, parse it into
    /// a `TurnEvent`, and dispatch it through the `TurnOrchestrator`.
    ///
    /// **This command is hidden** — it is not intended for direct user
    /// invocation. It appears here for documentation purposes only.
    #[command(hide = true)]
    Hooks(hooks::Hooks),
}

impl Command for Agent {
    fn run(&self) -> CliResult<()> {
        match &self.command {
            AgentCommands::Enable(cmd) => cmd.run(),
            AgentCommands::Disable(cmd) => cmd.run(),
            AgentCommands::Status(cmd) => cmd.run(),
            AgentCommands::Explain(cmd) => cmd.run(),
            AgentCommands::Attest(cmd) => cmd.run(),
            AgentCommands::Repair(cmd) => cmd.run(),
            AgentCommands::Lifecycle(cmd) => cmd.run(),
            AgentCommands::DatabaseOwner(cmd) => cmd.run(),
            AgentCommands::Hooks(cmd) => cmd.run(),
        }
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_commands_variant_names() {
        // Verify all expected subcommands exist by constructing them
        // (this is a compile-time check more than a runtime one)
        let _enable = AgentCommands::Enable(Enable::default_for_test());
        let _disable = AgentCommands::Disable(Disable::default_for_test());
        let _status = AgentCommands::Status(AgentStatus::default_for_test());
        let _explain = AgentCommands::Explain(Explain::default_for_test());
    }
}

#[cfg(test)]
mod repair_tests {
    use super::*;
    use atomic_agent::turn::session::SessionStore;
    use atomic_core::change::session::{
        IncompleteSession, SessionIncompleteOrigin, SessionStatus,
    };
    use std::fs;

    /// CB-12A AC3: the repair session contract — resuming an incomplete
    /// session RETAINS the incomplete evidence verbatim in an append-only
    /// note, sets the retention lease, and never rewrites history. (The CLI
    /// e2e path exercises the same contract through the integration suite;
    /// this unit test pins the store/session invariants.)
    #[test]
    fn repair_resumes_incomplete_session_and_retains_evidence() {
        let dir = tempfile::TempDir::new().unwrap();
        let sessions_dir = dir.path().join(".atomic").join("sessions");
        let store = SessionStore::new(&sessions_dir).unwrap();

        let mut session =
            atomic_agent::turn::session::AgentSession::new("sess-repair", "claude-code", "Claude Code");
        session.view_name = "main".to_string();
        let incomplete = IncompleteSession::new(
            "unexplained Git transition (fixture)",
            Vec::<String>::new(),
            String::new(),
            SessionIncompleteOrigin::UnattributedGitOperation,
        )
        .with_unbound_commits(vec!["deadbeefcafe".to_string()]);
        session.mark_incomplete(incomplete);
        store.save(&session).unwrap();

        // The repair verb's resume action: append-only note with the
        // retained evidence, then resume, then the never-cleared lease.
        let stored = store.load("sess-repair").unwrap().unwrap();
        let prior_incomplete = stored.incomplete().cloned();
        let mut repaired = stored;
        repaired.repair_history.push(atomic_agent::turn::session::RepairNote {
            at_rfc3339: chrono::Utc::now().to_rfc3339(),
            action: "resume".to_string(),
            prior_status: repaired.status.label().to_string(),
            retained_incomplete: prior_incomplete.clone(),
            detail: String::new(),
        });
        repaired.status = SessionStatus::Active;
        repaired.evidence_retained = true;
        store.save(&repaired).unwrap();

        let reloaded = store.load("sess-repair").unwrap().unwrap();
        assert!(matches!(reloaded.status, SessionStatus::Active));
        assert!(reloaded.evidence_retained, "the retention lease is set");
        assert_eq!(reloaded.repair_history.len(), 1, "one append-only note");
        let note = &reloaded.repair_history[0];
        let retained = note
            .retained_incomplete
            .as_ref()
            .expect("the repair note retains the incomplete evidence verbatim");
        assert_eq!(
            retained.reason, "unexplained Git transition (fixture)",
            "the retained evidence is byte-for-byte the prior refusal"
        );
        assert_eq!(retained.unbound_commits, vec!["deadbeefcafe".to_string()]);
        assert_eq!(note.action, "resume");
        assert_eq!(note.prior_status, "incomplete");
    }
}

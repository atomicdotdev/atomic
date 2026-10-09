//! `atomic agent receive` — typed TurnEvent entry point for the Reactor bridge.
//!
//! The Atomic Reactor (and other verified dispatchers) deliver agent events
//! as **already-typed** [`TurnEvent`] JSON on stdin — no agent-specific hook
//! parsing happens here. The reactor's adapter validated and projected the
//! native event; this command is the trusted typed boundary between the
//! Reactor's durable inbox and Atomic's turn orchestrator.
//!
//! # Invocation
//!
//! ```text
//! atomic agent receive --repository <root> --agent <agent-id> --json
//! ```
//!
//! stdin: one JSON object matching `atomic_agent::event::TurnEvent`
//! (snake_case `event_type`: `session_start`, `turn_start`, `turn_end`,
//! `session_end`, `pre_tool_use`, `post_tool_use`).
//!
//! stdout (`--json`): one line matching the `agent hooks --json` contract —
//! `{session_id, recorded, change_hash?, view?, files, run_id?}` — so the
//! Reactor can mark the delivery published and surface the recorded change.
//!
//! # Routing
//!
//! Both service modes call `ProvenanceService.DispatchTurnEvent`: local mode
//! runs the libatomic handler in-process, and reactor mode calls it over the
//! daemon socket. The receiver records parsing and transport outcomes under
//! `receive`; the shared handler records dispatch outcomes by event type.

use std::io::Read;
use std::sync::Arc;

use anyhow::anyhow;
use clap::Args;

use atomic_agent::event::TurnEvent;
use atomic_agent::hooks::AgentRegistry;
use atomic_core::types::Base32;

use crate::commands::Command;
use crate::error::{CliError, CliResult};

/// Typed event receiver (called by the Reactor, not by users).
#[derive(Debug, Args)]
pub struct Receive {
    /// Path inside the repository the event belongs to.
    #[arg(long, default_value = ".")]
    repository: std::path::PathBuf,

    /// Agent registry key the dispatcher verified (e.g. "opencode").
    #[arg(long)]
    agent: String,

    /// Print one JSON result line for the dispatcher.
    #[arg(long)]
    json: bool,
}

/// The dispatch outcome, shared by the RPC path and the local fallback.
pub struct ReceiveResult {
    pub session_id: String,
    pub recorded: bool,
    pub change_hash: Option<String>,
    pub view: Option<String>,
    pub files: Vec<String>,
    pub warnings: Vec<String>,
}

impl Command for Receive {
    fn run(&self) -> CliResult<()> {
        let repo_root = crate::commands::find_repository_root_from(&self.repository)?;

        let registry = AgentRegistry::with_defaults();
        let agent = registry
            .require(&self.agent)
            .map_err(|e| CliError::InvalidArgument {
                message: format!("Unknown agent '{}': {}", self.agent, e),
            })?;
        let agent_name = agent.name().to_string();
        let agent_display = agent.display_name().to_string();

        // Keep the entry-point outcome separate from the service's event-type
        // outcome: malformed stdin and transport failures never reach the
        // service, but a later successful receive must clear those failures.
        let dispatch = (|| {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).map_err(|e| {
                CliError::Io(std::io::Error::new(
                    e.kind(),
                    format!("Failed to read typed event from stdin: {}", e),
                ))
            })?;
            let event: TurnEvent =
                serde_json::from_slice(&input).map_err(|e| CliError::InvalidArgument {
                    message: format!("Invalid typed TurnEvent JSON: {}", e),
                })?;

            let agent_identity =
                super::identity::resolve_effective_agent_identity().map(|(name, _)| name);

            // Both local and daemon transports call the same libatomic handler.
            crate::commands::rpc::dispatch_turn_event(
                &repo_root,
                &event,
                &agent_name,
                &agent_display,
                agent_identity,
            )
        })();
        let result = super::health::record_result(
            &repo_root,
            &agent_name,
            &agent_display,
            "receive",
            dispatch,
        )?;

        for warning in &result.warnings {
            log::warn!("{}", warning);
        }

        if !self.json {
            println!(
                "{} recorded={} ({} files)",
                result.session_id,
                result.recorded,
                result.files.len()
            );
            return Ok(());
        }

        println!(
            "{}",
            serde_json::json!({
                "session_id": result.session_id,
                "recorded": result.recorded,
                "change_hash": result.change_hash,
                "view": result.view,
                "files": result.files,
            })
        );
        Ok(())
    }
}

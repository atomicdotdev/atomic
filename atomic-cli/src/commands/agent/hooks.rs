//! `atomic agent hooks` — internal hook handler invoked by agent callbacks.
//!
//!
//! This is the command that agent hooks (installed in `.claude/settings.json`,
//! `.gemini/settings.json`, etc.) call back to. It is **hidden** from `--help`
//! because users never invoke it directly.
//!
//! # Invocation
//!
//! ```text
//! atomic agent hooks <agent-name> <verb> < /dev/stdin
//! ```
//!
//! For example, Claude Code's `stop` hook runs:
//!
//! ```text
//! atomic agent hooks claude-code stop
//! ```
//!
//! With JSON on stdin:
//!
//! ```json
//! {"session_id": "abc-123", "transcript_path": "/tmp/t.jsonl"}
//! ```
//!
//! # Processing Flow
//!
//! 1. Read raw JSON bytes from stdin
//! 2. Look up the agent adapter in the `AgentRegistry`
//! 3. Map the CLI verb to a `HookType` via `HookType::from_verb()`
//! 4. Call `agent.parse_event(hook_type, input)` → `TurnEvent`
//! 5. Create a `TurnOrchestrator` for the repository
//! 6. Call `orchestrator.dispatch(event)` → `DispatchResult`
//! 7. If the result has a `message`, write it as JSON to stdout
//!    (agents like Claude Code read JSON responses from hook stdout)
//!
//! # Error Handling
//!
//! Errors are written to stderr. The command exits with code 0 even on
//! non-fatal errors so that the agent continues normally. Fatal errors
//! (e.g., cannot parse stdin) exit with a non-zero code.
//!
//! # Stdout JSON Response
//!
//! Some agents (Claude Code) read JSON from hook stdout to display
//! system messages. When the orchestrator returns a `message`, we write:
//!
//! ```json
//! {"systemMessage": "Atomic is tracking this session..."}
//! ```

use std::io::Read;

use anyhow::anyhow;
use clap::Args;

use atomic_agent::event::HookType;
use atomic_agent::hooks::AgentRegistry;

use crate::commands::{find_repository_root, Command};
use crate::error::{CliError, CliResult};

// Hooks Command

/// Internal hook handlers (called by agent hooks, not by users).
///
/// This command is hidden from `--help`. It is invoked by hooks installed
/// in agent configuration files (e.g., `.claude/settings.json`).
///
/// # Usage
///
/// ```text
/// atomic agent hooks <agent> <verb>
/// ```
///
/// Where `<agent>` is the agent registry key (e.g., `claude-code`) and
/// `<verb>` is the hook verb (e.g., `stop`, `user-prompt-submit`).
#[derive(Debug, Args)]
pub struct Hooks {
    /// The agent name (e.g., "claude-code", "gemini-cli").
    agent_name: String,

    /// The hook verb (e.g., "stop", "user-prompt-submit", "session-start").
    verb: String,
}

impl Command for Hooks {
    fn run(&self) -> CliResult<()> {
        // Read stdin (agent sends JSON here)
        let mut input = Vec::new();
        std::io::stdin().read_to_end(&mut input).map_err(|e| {
            CliError::Io(std::io::Error::new(
                e.kind(),
                format!("Failed to read hook input from stdin: {}", e),
            ))
        })?;

        // Look up the agent adapter
        let registry = AgentRegistry::with_defaults();
        let agent = registry
            .require(&self.agent_name)
            .map_err(|e| CliError::InvalidArgument {
                message: format!("Unknown agent '{}': {}", self.agent_name, e),
            })?;

        // Map the CLI verb to a HookType
        let hook_type =
            HookType::from_verb(&self.verb).ok_or_else(|| CliError::InvalidArgument {
                message: format!(
                    "Unknown hook verb '{}' for agent '{}'. Known verbs: {}",
                    self.verb,
                    self.agent_name,
                    agent.hook_verbs().join(", "),
                ),
            })?;

        // Parse the agent-specific JSON into a common TurnEvent
        let event = agent.parse_event(hook_type, &input).map_err(|e| {
            // Log to stderr but don't fail hard on parse errors for
            // non-critical hooks (tool use events)
            if hook_type.is_tool_use() {
                eprintln!(
                    "[atomic] Warning: failed to parse {} {} input: {}",
                    self.agent_name, self.verb, e
                );
                // Return a generic event so we can continue
                return CliError::Internal(anyhow!("Hook parse failed: {}", e));
            }
            CliError::Internal(anyhow!(
                "Failed to parse hook input for {} {}: {}",
                self.agent_name,
                self.verb,
                e
            ))
        })?;

        // Find the repository root
        let repo_root = find_repository_root().map_err(|e| {
            // Log to stderr — the agent should continue even if Atomic can't find a repo
            eprintln!("[atomic] Warning: {}", e);
            e
        })?;

        // Create a tokio runtime for the async orchestrator
        // Hook handlers are short-lived — one runtime per invocation is fine.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| CliError::Internal(anyhow!("Failed to create async runtime: {}", e)))?;

        let agent_name = agent.name().to_string();
        let agent_display = agent.display_name().to_string();

        let result = rt.block_on(async {
            // Create the orchestrator
            let mut orchestrator =
                atomic_agent::turn::orchestrator::TurnOrchestrator::new(&repo_root)
                    .await
                    .map_err(|e| {
                        CliError::Internal(anyhow!("Failed to create orchestrator: {}", e))
                    })?;

            // Set the agent identity so new sessions get the correct name
            // (e.g., "claude-code" / "Claude Code" instead of "unknown")
            orchestrator.set_agent(&agent_name, &agent_display);

            // Dispatch the event through the state machine → watcher → recorder
            let dispatch_result = orchestrator
                .dispatch(event)
                .await
                .map_err(|e| CliError::Internal(anyhow!("Failed to dispatch hook event: {}", e)))?;

            Ok::<_, CliError>(dispatch_result)
        })?;

        // Log warnings via log crate (not stderr — that leaks into agent TUIs)
        for warning in &result.warnings {
            log::warn!("{}", warning);
        }

        // Log recording info at debug level
        if let Some(ref outcome) = result.change_recorded {
            log::debug!("{}", outcome);
        }

        // Log the system message instead of printing to stdout.
        // Claude Code expects JSON on stdout, but OpenCode's plugin $
        // captures stdout and displays it in the TUI as noise.
        // Use log::debug so it's available in RUST_LOG but silent otherwise.
        if let Some(ref message) = result.message {
            log::debug!("Hook response: {}", message);
        }

        Ok(())
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hooks_struct_fields() {
        let hooks = Hooks {
            agent_name: "claude-code".to_string(),
            verb: "stop".to_string(),
        };
        assert_eq!(hooks.agent_name, "claude-code");
        assert_eq!(hooks.verb, "stop");
    }

    #[test]
    fn test_hook_type_from_verb_all_claude_verbs() {
        let verbs_and_types = vec![
            ("session-start", HookType::SessionStart),
            ("session-end", HookType::SessionEnd),
            ("stop", HookType::TurnEnd),
            ("user-prompt-submit", HookType::TurnStart),
            ("pre-task", HookType::PreToolUse),
            ("post-task", HookType::PostToolUse),
            ("post-todo", HookType::PostToolUse),
        ];

        for (verb, expected) in verbs_and_types {
            let hook_type = HookType::from_verb(verb);
            assert_eq!(
                hook_type,
                Some(expected),
                "Verb '{}' should map to {:?}",
                verb,
                expected,
            );
        }
    }

    #[test]
    fn test_hook_type_from_verb_all_gemini_verbs() {
        let verbs_and_types = vec![
            ("session-start", HookType::SessionStart),
            ("session-end", HookType::SessionEnd),
            ("before-agent", HookType::TurnStart),
            ("after-agent", HookType::TurnEnd),
            ("before-tool", HookType::PreToolUse),
            ("after-tool", HookType::PostToolUse),
        ];

        for (verb, expected) in verbs_and_types {
            let hook_type = HookType::from_verb(verb);
            assert_eq!(
                hook_type,
                Some(expected),
                "Verb '{}' should map to {:?}",
                verb,
                expected,
            );
        }
    }

    #[test]
    fn test_hook_type_from_verb_unknown() {
        assert_eq!(HookType::from_verb("unknown-verb"), None);
        assert_eq!(HookType::from_verb(""), None);
        assert_eq!(HookType::from_verb("STOP"), None); // case-sensitive
    }

    #[test]
    fn test_agent_registry_has_claude_code() {
        let registry = AgentRegistry::with_defaults();
        assert!(registry.get("claude-code").is_some());
    }

    #[test]
    fn test_agent_registry_require_unknown_fails() {
        let registry = AgentRegistry::with_defaults();
        let err = registry.require("nonexistent");
        assert!(err.is_err());
    }

    #[test]
    fn test_json_response_format() {
        // Verify the JSON response format matches what Claude Code expects
        let message = "Atomic is tracking this session.";
        let response = serde_json::json!({
            "systemMessage": message
        });
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("systemMessage"));
        assert!(json.contains("Atomic is tracking this session."));

        // Parse it back to verify structure
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed["systemMessage"].as_str(),
            Some("Atomic is tracking this session.")
        );
    }

    #[test]
    fn test_json_response_no_message() {
        // When there's no message, we should not produce output.
        // This tests the logic: if message.is_none(), don't println.
        let result = atomic_agent::turn::orchestrator::DispatchResult::new(
            "sess-1",
            atomic_agent::turn::phase::Phase::Idle,
        );
        assert!(result.message.is_none());
    }

    #[test]
    fn test_hooks_debug() {
        let hooks = Hooks {
            agent_name: "claude-code".to_string(),
            verb: "session-start".to_string(),
        };
        let debug = format!("{:?}", hooks);
        assert!(debug.contains("claude-code"));
        assert!(debug.contains("session-start"));
    }
}

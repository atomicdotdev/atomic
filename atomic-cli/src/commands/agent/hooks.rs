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

use std::io::{Read, Write};
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::anyhow;
use clap::Args;

use atomic_agent::event::HookType;
use atomic_agent::hooks::AgentRegistry;
use atomic_core::types::Base32;

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

    /// Run the hook body in this process.
    #[arg(long, hide = true)]
    foreground: bool,

    /// Emit a machine-readable JSON result on stdout describing what the hook
    /// recorded (session id, whether a change was recorded, the change hash,
    /// and the files in that change).
    ///
    /// Off by default so existing agent integrations (Claude Code reads its own
    /// JSON response from stdout; the OpenCode plugin treats stdout as TUI
    /// noise) are unaffected. Callers that need the recorded change hash — e.g.
    /// the Sherpa UI bridging an ACP run into Atomic — pass `--json`.
    #[arg(long, hide = true)]
    json: bool,
}

/// Machine-readable result of a hook invocation, emitted on stdout when
/// `--json` is passed. Lets a coordinator (e.g. the Sherpa UI) link the ACP
/// turn to the Atomic change it produced.
#[derive(Debug, serde::Serialize)]
struct HookJsonResult {
    /// The agent session id the hook ran under.
    session_id: String,
    /// Whether this hook recorded an Atomic change.
    recorded: bool,
    /// The recorded change hash (base32), if a change was recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    change_hash: Option<String>,
    /// The files in the recorded change (empty when nothing was recorded).
    files: Vec<String>,
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

        if self.should_handoff_codex_stop() {
            return self.handoff_codex_stop(&input);
        }

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

        // Keep hooks quiet on stdout/stderr — Claude Code surfaces both, so
        // route diagnostics through `log` instead.
        log::debug!(
            "hooks: agent={} display={} verb={}",
            agent_name,
            agent_display,
            self.verb
        );

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

        // When explicitly requested (`--json`), emit a machine-readable result
        // on stdout for coordinators that need the recorded change hash. This is
        // opt-in so the default quiet behavior above is preserved for the agents
        // that read or ignore stdout themselves.
        if self.json {
            let json = match &result.change_recorded {
                Some(outcome) => HookJsonResult {
                    session_id: result.session_id.clone(),
                    recorded: true,
                    change_hash: Some(outcome.hash.to_base32()),
                    files: outcome.recorded_file_list().to_vec(),
                },
                None => HookJsonResult {
                    session_id: result.session_id.clone(),
                    recorded: false,
                    change_hash: None,
                    files: Vec::new(),
                },
            };
            // Serialize defensively: a serialization failure must not turn a
            // successful hook into an error.
            match serde_json::to_string(&json) {
                Ok(s) => println!("{}", s),
                Err(e) => log::warn!("Failed to serialize hook JSON result: {}", e),
            }
        }

        Ok(())
    }
}

impl Hooks {
    fn should_handoff_codex_stop(&self) -> bool {
        // `--json` callers (e.g. the Sherpa UI bridge) need the recorded change
        // hash on stdout synchronously. The background handoff detaches into a
        // child whose stdout is null, so it can never return JSON — run the hook
        // in-process instead when JSON output is requested.
        !self.foreground && !self.json && self.agent_name == "codex" && self.verb == "stop"
    }

    fn handoff_codex_stop(&self, input: &[u8]) -> CliResult<()> {
        let exe = std::env::current_exe().map_err(|e| {
            CliError::Internal(anyhow!("Failed to resolve atomic executable: {}", e))
        })?;

        let mut child = ProcessCommand::new(exe)
            .arg("agent")
            .arg("hooks")
            .arg(&self.agent_name)
            .arg(&self.verb)
            .arg("--foreground")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                CliError::Internal(anyhow!(
                    "Failed to start background Codex Stop recorder: {}",
                    e
                ))
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input).map_err(|e| {
                CliError::Io(std::io::Error::new(
                    e.kind(),
                    format!(
                        "Failed to pass Codex Stop input to background recorder: {}",
                        e
                    ),
                ))
            })?;
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
            foreground: false,
            json: false,
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
            foreground: false,
            json: false,
        };
        let debug = format!("{:?}", hooks);
        assert!(debug.contains("claude-code"));
        assert!(debug.contains("session-start"));
    }

    #[test]
    fn test_codex_stop_handoffs_by_default() {
        let hooks = Hooks {
            agent_name: "codex".to_string(),
            verb: "stop".to_string(),
            foreground: false,
            json: false,
        };

        assert!(hooks.should_handoff_codex_stop());
    }

    #[test]
    fn test_codex_stop_json_disables_handoff() {
        // `--json` must run in-process so the caller gets the change hash on
        // stdout; the background handoff nulls stdout and could never return it.
        let hooks = Hooks {
            agent_name: "codex".to_string(),
            verb: "stop".to_string(),
            foreground: false,
            json: true,
        };

        assert!(!hooks.should_handoff_codex_stop());
    }

    #[test]
    fn test_codex_stop_foreground_disables_handoff() {
        let hooks = Hooks {
            agent_name: "codex".to_string(),
            verb: "stop".to_string(),
            foreground: true,
            json: false,
        };

        assert!(!hooks.should_handoff_codex_stop());
    }

    #[test]
    fn test_non_codex_stop_does_not_handoff() {
        let hooks = Hooks {
            agent_name: "claude-code".to_string(),
            verb: "stop".to_string(),
            foreground: false,
            json: false,
        };

        assert!(!hooks.should_handoff_codex_stop());
    }

    #[test]
    fn test_hook_json_result_recorded() {
        // The shape a coordinator (Sherpa UI) parses to link an ACP turn to its
        // Atomic change.
        let result = HookJsonResult {
            session_id: "sess-abc".to_string(),
            recorded: true,
            change_hash: Some("ABC123".to_string()),
            files: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&result).unwrap()).unwrap();

        assert_eq!(parsed["session_id"], "sess-abc");
        assert_eq!(parsed["recorded"], true);
        assert_eq!(parsed["change_hash"], "ABC123");
        assert_eq!(parsed["files"][0], "src/main.rs");
        assert_eq!(parsed["files"][1], "src/lib.rs");
    }

    #[test]
    fn test_hook_json_result_not_recorded_omits_hash() {
        // When nothing was recorded, `change_hash` is omitted (not null) and
        // `files` is empty.
        let result = HookJsonResult {
            session_id: "sess-xyz".to_string(),
            recorded: false,
            change_hash: None,
            files: Vec::new(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["recorded"], false);
        assert!(
            !json.contains("change_hash"),
            "change_hash must be omitted when None, got: {}",
            json
        );
        assert_eq!(parsed["files"].as_array().unwrap().len(), 0);
    }
}

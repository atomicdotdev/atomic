//! Kiro IDE agent hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing from Kiro IDE's hook system, which uses shell
//! command actions that call `atomic agent hooks kiro <verb>` via subprocess
//! at each lifecycle event.
//!
//! # Kiro Hook Architecture
//!
//! Kiro IDE hooks are configured through the IDE's Agent Steering & Skills
//! panel. Each hook uses a "Shell Command" action that invokes the Atomic CLI.
//! The `atomic-kiro` package provides the shell scripts that bridge Kiro's
//! hook triggers to `atomic agent hooks kiro <verb>`.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  Kiro IDE Hooks (configured in IDE panel)                               │
//! │                                                                         │
//! │  PromptSubmit      ──▶  atomic agent hooks kiro prompt-submit          │
//! │  AgentStop         ──▶  atomic agent hooks kiro agent-stop             │
//! │  PreToolUse        ──▶  atomic agent hooks kiro pre-tool-use           │
//! │  PostToolUse       ──▶  atomic agent hooks kiro post-tool-use          │
//! │  PostTaskExecution ──▶  atomic agent hooks kiro post-task              │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Hook Verbs
//!
//! | Verb              | HookType     | Description                          |
//! |-------------------|--------------|--------------------------------------|
//! | `prompt-submit`   | TurnStart    | User submits a prompt                |
//! | `agent-stop`      | TurnEnd      | Agent completes its turn             |
//! | `pre-tool-use`    | PreToolUse   | Before tool execution                |
//! | `post-tool-use`   | PostToolUse  | After tool execution                 |
//! | `post-task`       | PostToolUse  | After spec task completion           |
//!
//! # JSON Input Format
//!
//! All hooks receive JSON via stdin with at minimum:
//!
//! ```json
//! {
//!   "session_id": "kiro-20260521-153000",
//!   "cwd": "/path/to/project"
//! }
//! ```
//!
//! Additional fields vary by verb (see struct definitions below).
//!
//! # Installation
//!
//! Kiro hook configuration is managed through the IDE panel and installed by
//! the `atomic-kiro` package via the integrations engine. This adapter only
//! parses hook events — `install()` and `uninstall()` are no-ops.
//!
//! # Detection
//!
//! Kiro is considered present when a `.kiro/` directory exists at the
//! project root.

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

/// The Kiro config directory name.
const KIRO_DIR: &str = ".kiro";

// ---------------------------------------------------------------------------
// JSON input structs
// ---------------------------------------------------------------------------

/// JSON input for prompt-submit hook (TurnStart).
///
/// Sent when the user submits a prompt in Kiro IDE.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PromptSubmitInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Working directory
    #[serde(default)]
    cwd: Option<String>,

    /// Model identifier (e.g. "claude-sonnet-4-6")
    #[serde(default)]
    model: Option<String>,

    /// The user's prompt text
    #[serde(default)]
    prompt: Option<String>,

    /// ISO 8601 timestamp
    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for agent-stop hook (TurnEnd).
///
/// Sent when the Kiro agent completes its turn.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AgentStopInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Working directory
    #[serde(default)]
    cwd: Option<String>,

    /// Model identifier
    #[serde(default)]
    model: Option<String>,

    /// ISO 8601 timestamp
    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for pre-tool-use and post-tool-use hooks.
///
/// Sent before or after a tool is executed within a Kiro session.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ToolUseInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Name of the tool being invoked
    #[serde(default)]
    tool_name: Option<String>,

    /// Unique call identifier for this tool invocation
    #[serde(default)]
    tool_call_id: Option<String>,

    /// Working directory
    #[serde(default)]
    cwd: Option<String>,

    /// ISO 8601 timestamp
    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for post-task hook.
///
/// Sent after a spec task completes in Kiro. Shares the same shape as
/// tool-use events.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PostTaskInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Name of the tool that completed (if applicable)
    #[serde(default)]
    tool_name: Option<String>,

    /// Unique call identifier
    #[serde(default)]
    tool_call_id: Option<String>,

    /// Working directory
    #[serde(default)]
    cwd: Option<String>,

    /// ISO 8601 timestamp
    #[serde(default)]
    timestamp: Option<String>,
}

// ---------------------------------------------------------------------------
// KiroHook
// ---------------------------------------------------------------------------

/// Kiro IDE agent hook adapter.
///
/// Parses hook JSON from Kiro IDE's shell command hooks. Hook configuration
/// is managed through the IDE panel and the `atomic-kiro` package via the
/// integrations engine.
#[derive(Debug)]
pub struct KiroHook {
    _private: (),
}

impl KiroHook {
    /// Create a new Kiro hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Extract a session ID from an optional field, generating a fallback if missing.
    fn extract_session_id(session_id: Option<String>) -> String {
        session_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("kiro-{}", timestamp_hex()))
    }
}

impl Default for KiroHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHook for KiroHook {
    fn name(&self) -> &str {
        "kiro"
    }

    fn display_name(&self) -> &str {
        "Kiro"
    }

    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
        if input.is_empty() {
            return Err(AgentError::HookInputEmpty {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
            });
        }

        // Preserve raw JSON for debugging and downstream use
        let raw_json: serde_json::Value =
            serde_json::from_slice(input).map_err(|e| AgentError::HookParseFailed {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: e.to_string(),
            })?;

        match hook_type {
            HookType::TurnStart => {
                let parsed: PromptSubmitInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                let mut event =
                    TurnEvent::new(Self::extract_session_id(parsed.session_id), hook_type)
                        .with_raw_json(raw_json);

                if let Some(prompt) = parsed.prompt {
                    event = event.with_prompt(prompt);
                }

                // Store model in raw_json for the orchestrator
                if let Some(model) = parsed.model {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("model".to_string(), serde_json::Value::String(model));
                        }
                    }
                }

                Ok(event)
            }

            HookType::TurnEnd => {
                let parsed: AgentStopInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                let mut event =
                    TurnEvent::new(Self::extract_session_id(parsed.session_id), hook_type)
                        .with_raw_json(raw_json);

                // Store model in raw_json for provenance
                if let Some(model) = parsed.model {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("model".to_string(), serde_json::Value::String(model));
                        }
                    }
                }

                Ok(event)
            }

            HookType::PreToolUse => {
                let parsed: ToolUseInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                let mut event =
                    TurnEvent::new(Self::extract_session_id(parsed.session_id), hook_type)
                        .with_raw_json(raw_json);

                if let Some(name) = parsed.tool_name {
                    event = event.with_tool_name(name);
                }
                if let Some(id) = parsed.tool_call_id {
                    event = event.with_tool_use_id(id);
                }

                Ok(event)
            }

            HookType::PostToolUse => {
                let parsed: ToolUseInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                let mut event =
                    TurnEvent::new(Self::extract_session_id(parsed.session_id), hook_type)
                        .with_raw_json(raw_json);

                if let Some(name) = parsed.tool_name {
                    event = event.with_tool_name(name);
                }
                if let Some(id) = parsed.tool_call_id {
                    event = event.with_tool_use_id(id);
                }

                Ok(event)
            }

            // Kiro doesn't have native SessionStart/SessionEnd — these are
            // synthesized by the shell scripts in the atomic-kiro package.
            // If they arrive, parse the minimal session_id from the raw JSON.
            HookType::SessionStart | HookType::SessionEnd => {
                // Use a lightweight parse — just extract the session_id
                #[derive(Deserialize)]
                struct MinimalInput {
                    #[serde(default)]
                    session_id: Option<String>,
                }

                let parsed: MinimalInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                let event = TurnEvent::new(Self::extract_session_id(parsed.session_id), hook_type)
                    .with_raw_json(raw_json);

                Ok(event)
            }
        }
    }

    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        // Installation is owned by the integrations engine (the atomic-kiro
        // package on Atomic storage); this adapter installs nothing.
        Ok(0)
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(()) // Uninstallation is receipt-driven via the integrations engine.
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        false // Managed by the integrations engine (atomic-kiro package)
    }

    fn supported_hooks(&self) -> Vec<HookType> {
        vec![
            HookType::TurnStart,
            HookType::TurnEnd,
            HookType::PreToolUse,
            HookType::PostToolUse,
        ]
    }

    fn detect_presence(&self, repo_root: &Path) -> bool {
        // Kiro is present if the .kiro directory exists
        repo_root.join(KIRO_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "prompt-submit",
            "agent-stop",
            "pre-tool-use",
            "post-tool-use",
            "post-task",
        ]
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a short hex string for fallback session IDs.
///
/// This is only used when the Kiro hook scripts fail to provide a session_id,
/// which should be rare. Uses timestamp bits for uniqueness.
fn timestamp_hex() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    // Use lower 32 bits of timestamp for a short hex ID
    format!("{:08x}", (now & 0xFFFF_FFFF) as u32)
}

// ---------------------------------------------------------------------------
// Verb mapping
// ---------------------------------------------------------------------------

/// Convert a Kiro-specific verb to a [`HookType`].
///
/// Kiro uses these verbs:
///
/// | Verb              | HookType     |
/// |-------------------|--------------|
/// | `prompt-submit`   | TurnStart    |
/// | `agent-stop`      | TurnEnd      |
/// | `pre-tool-use`    | PreToolUse   |
/// | `post-tool-use`   | PostToolUse  |
/// | `post-task`       | PostToolUse  |
pub fn verb_to_hook_type(verb: &str) -> Option<HookType> {
    match verb {
        "prompt-submit" => Some(HookType::TurnStart),
        "agent-stop" => Some(HookType::TurnEnd),
        "pre-tool-use" => Some(HookType::PreToolUse),
        "post-tool-use" => Some(HookType::PostToolUse),
        "post-task" => Some(HookType::PostToolUse),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookType;

    fn make_hook() -> KiroHook {
        KiroHook::new()
    }

    // --- name / display_name ---

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "kiro");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "Kiro");
    }

    // --- supported_hooks ---

    #[test]
    fn test_supported_hooks() {
        let hook = make_hook();
        let hooks = hook.supported_hooks();
        assert_eq!(hooks.len(), 4);
        assert!(hooks.contains(&HookType::TurnStart));
        assert!(hooks.contains(&HookType::TurnEnd));
        assert!(hooks.contains(&HookType::PreToolUse));
        assert!(hooks.contains(&HookType::PostToolUse));
        // Kiro doesn't have native SessionStart/SessionEnd
        assert!(!hooks.contains(&HookType::SessionStart));
        assert!(!hooks.contains(&HookType::SessionEnd));
    }

    // --- hook_verbs ---

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert_eq!(verbs.len(), 5);
        assert!(verbs.contains(&"prompt-submit"));
        assert!(verbs.contains(&"agent-stop"));
        assert!(verbs.contains(&"pre-tool-use"));
        assert!(verbs.contains(&"post-tool-use"));
        assert!(verbs.contains(&"post-task"));
    }

    // --- install / uninstall are no-ops ---

    #[test]
    fn test_install_is_noop() {
        let hook = make_hook();
        let result = hook.install(Path::new("/tmp"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_uninstall_is_noop() {
        let hook = make_hook();
        let result = hook.uninstall(Path::new("/tmp"));
        assert!(result.is_ok());
    }

    // --- parse_event: empty input ---

    #[test]
    fn test_parse_event_empty_input() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::TurnStart, b"");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AgentError::HookInputEmpty { .. }));
    }

    // --- parse_event: invalid JSON ---

    #[test]
    fn test_parse_event_invalid_json() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::TurnStart, b"not json");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            AgentError::HookParseFailed { .. }
        ));
    }

    // --- parse_event: missing session_id generates fallback ---

    #[test]
    fn test_parse_event_missing_session_id() {
        let hook = make_hook();
        let input = br#"{"cwd": "/tmp"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        // Should generate a fallback session ID
        assert!(event.session_id.starts_with("kiro-"));
    }

    // --- parse_event: empty session_id generates fallback ---

    #[test]
    fn test_parse_event_empty_session_id() {
        let hook = make_hook();
        let input = br#"{"session_id": ""}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(event.session_id.starts_with("kiro-"));
    }

    // --- parse_event: prompt-submit (TurnStart) ---

    #[test]
    fn test_parse_prompt_submit() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "kiro-20260521-153000",
            "cwd": "/path/to/project",
            "model": "claude-sonnet-4-6",
            "prompt": "Fix the bug",
            "timestamp": "2026-05-21T15:30:00Z"
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "kiro-20260521-153000");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(event.prompt.as_deref(), Some("Fix the bug"));
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "claude-sonnet-4-6");
    }

    // --- parse_event: agent-stop (TurnEnd) ---

    #[test]
    fn test_parse_agent_stop() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "kiro-20260521-153000",
            "cwd": "/path/to/project",
            "model": "claude-sonnet-4-6",
            "timestamp": "2026-05-21T15:30:05Z"
        }"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "kiro-20260521-153000");
        assert_eq!(event.event_type, HookType::TurnEnd);
    }

    // --- parse_event: pre-tool-use (PreToolUse) ---

    #[test]
    fn test_parse_pre_tool_use() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "kiro-20260521-153000",
            "tool_name": "write",
            "tool_call_id": "tc_123",
            "cwd": "/path/to/project"
        }"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "kiro-20260521-153000");
        assert_eq!(event.event_type, HookType::PreToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("write"));
        assert_eq!(event.tool_use_id.as_deref(), Some("tc_123"));
    }

    // --- parse_event: post-tool-use (PostToolUse) ---

    #[test]
    fn test_parse_post_tool_use() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "kiro-20260521-153000",
            "tool_name": "read",
            "tool_call_id": "tc_456",
            "cwd": "/path/to/project"
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "kiro-20260521-153000");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("read"));
        assert_eq!(event.tool_use_id.as_deref(), Some("tc_456"));
    }

    // --- parse_event: post-task (PostToolUse) ---

    #[test]
    fn test_parse_post_task() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "kiro-20260521-153000",
            "tool_name": "spec-task",
            "cwd": "/path/to/project"
        }"#;
        // post-task maps to PostToolUse via verb_to_hook_type
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "kiro-20260521-153000");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("spec-task"));
    }

    // --- detect_presence ---

    #[test]
    fn test_detect_presence_with_kiro_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".kiro")).unwrap();
        let hook = make_hook();
        assert!(hook.detect_presence(tmp.path()));
    }

    #[test]
    fn test_detect_presence_without_kiro_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(tmp.path()));
    }

    // --- verb_to_hook_type ---

    #[test]
    fn test_verb_to_hook_type() {
        assert_eq!(
            verb_to_hook_type("prompt-submit"),
            Some(HookType::TurnStart)
        );
        assert_eq!(verb_to_hook_type("agent-stop"), Some(HookType::TurnEnd));
        assert_eq!(
            verb_to_hook_type("pre-tool-use"),
            Some(HookType::PreToolUse)
        );
        assert_eq!(
            verb_to_hook_type("post-tool-use"),
            Some(HookType::PostToolUse)
        );
        assert_eq!(verb_to_hook_type("post-task"), Some(HookType::PostToolUse));
        assert_eq!(verb_to_hook_type("unknown"), None);
    }

    // --- default trait ---

    #[test]
    fn test_default() {
        let hook = KiroHook::default();
        assert_eq!(hook.name(), "kiro");
    }

    // --- debug ---

    #[test]
    fn test_debug() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("KiroHook"));
    }
}

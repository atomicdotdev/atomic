//! Kilo Code agent hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing from Kilo Code's hook system, which uses shell
//! command actions that call `atomic agent hooks kilo <verb>` via subprocess
//! at each lifecycle event.
//!
//! # Kilo Hook Architecture
//!
//! Kilo Code hooks are configured through kilo.jsonc or agent definitions.
//! Each hook uses a shell command that invokes the Atomic CLI.
//! The `atomic-kilo` npm package provides the shell scripts that bridge
//! Kilo's hook triggers to `atomic agent hooks kilo <verb>`.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  Kilo Code Hooks (configured via kilo.jsonc / .kilo/agents/)           │
//! │                                                                         │
//! │  PromptSubmit      ──▶  atomic agent hooks kilo prompt-submit          │
//! │  AgentStop         ──▶  atomic agent hooks kilo agent-stop             │
//! │  PreToolUse        ──▶  atomic agent hooks kilo pre-tool-use           │
//! │  PostToolUse       ──▶  atomic agent hooks kilo post-tool-use          │
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
//!
//! # JSON Input Format
//!
//! All hooks receive JSON via stdin with at minimum:
//!
//! ```json
//! {
//!   "session_id": "kilo-20260715-153000",
//!   "cwd": "/path/to/project"
//! }
//! ```
//!
//! Additional fields vary by verb (see struct definitions below).
//!
//! # Installation
//!
//! Kilo hook configuration is managed through `kilo.jsonc` and the
//! `atomic-kilo` npm package. This adapter only parses hook events —
//! `install()` and `uninstall()` are no-ops.
//!
//! # Detection
//!
//! Kilo is considered present when a `.kilo/` directory exists at the
//! project root.

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

/// The Kilo Code config directory name.
const KILO_DIR: &str = ".kilo";

// ---------------------------------------------------------------------------
// JSON input structs
// ---------------------------------------------------------------------------

/// JSON input for prompt-submit hook (TurnStart).
///
/// Sent when the user submits a prompt in Kilo Code.
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
/// Sent when the agent completes its turn.
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

/// JSON input for pre-tool-use / post-tool-use hooks.
///
/// Sent before/after tool execution.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ToolUseInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Tool name (e.g. "read", "write", "bash")
    #[serde(default)]
    tool_name: Option<String>,

    /// Tool call ID for correlation
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
// KiloHook adapter
// ---------------------------------------------------------------------------

/// Kilo Code agent hook adapter.
///
/// Parses hook JSON from Kilo Code's shell command hooks. Hook configuration
/// is managed through `kilo.jsonc` and the `atomic-kilo` npm package.
#[derive(Debug)]
pub struct KiloHook {
    _private: (),
}

impl KiloHook {
    /// Create a new Kilo hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Extract a session ID from an optional field, generating a fallback if missing.
    fn extract_session_id(session_id: Option<String>) -> String {
        session_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("kilo-{}", timestamp_hex()))
    }
}

impl Default for KiloHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHook for KiloHook {
    fn name(&self) -> &str {
        "kilo"
    }

    fn display_name(&self) -> &str {
        "Kilo Code"
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

            // Kilo doesn't have native SessionStart/SessionEnd — these are
            // synthesized by the shell scripts in the atomic-kilo package.
            // If they arrive, parse the minimal session_id from the raw JSON.
            HookType::SessionStart | HookType::SessionEnd => {
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
        Ok(0) // Installation handled by atomic-kilo package
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(()) // Uninstallation handled by atomic-kilo package
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        false // Managed by atomic-kilo npm package
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
        // Kilo is present if the .kilo directory exists
        repo_root.join(KILO_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "prompt-submit",
            "agent-stop",
            "pre-tool-use",
            "post-tool-use",
        ]
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Generate a short hex string for fallback session IDs.
fn timestamp_hex() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    format!("{:08x}", (now & 0xFFFF_FFFF) as u32)
}

// ---------------------------------------------------------------------------
// Verb mapping
// ---------------------------------------------------------------------------

/// Convert a Kilo-specific verb to a [`HookType`].
///
/// Kilo uses these verbs:
///
/// | Verb              | HookType     |
/// |-------------------|--------------|
/// | `prompt-submit`   | TurnStart    |
/// | `agent-stop`      | TurnEnd      |
/// | `pre-tool-use`    | PreToolUse   |
/// | `post-tool-use`   | PostToolUse  |
pub fn verb_to_hook_type(verb: &str) -> Option<HookType> {
    match verb {
        "prompt-submit" => Some(HookType::TurnStart),
        "agent-stop" => Some(HookType::TurnEnd),
        "pre-tool-use" => Some(HookType::PreToolUse),
        "post-tool-use" => Some(HookType::PostToolUse),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hook() -> KiloHook {
        KiloHook::new()
    }

    // -- Basic trait methods --

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "kilo");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "Kilo Code");
    }

    #[test]
    fn test_supported_hooks() {
        let hook = make_hook();
        let hooks = hook.supported_hooks();
        assert!(hooks.contains(&HookType::TurnStart));
        assert!(hooks.contains(&HookType::TurnEnd));
        assert!(hooks.contains(&HookType::PreToolUse));
        assert!(hooks.contains(&HookType::PostToolUse));
        assert!(!hooks.contains(&HookType::SessionStart));
        assert!(!hooks.contains(&HookType::SessionEnd));
    }

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert!(verbs.contains(&"prompt-submit"));
        assert!(verbs.contains(&"agent-stop"));
        assert!(verbs.contains(&"pre-tool-use"));
        assert!(verbs.contains(&"post-tool-use"));
    }

    #[test]
    fn test_install_is_noop() {
        let hook = make_hook();
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(hook.install(dir.path()).unwrap(), 0);
    }

    #[test]
    fn test_uninstall_is_noop() {
        let hook = make_hook();
        let dir = tempfile::TempDir::new().unwrap();
        assert!(hook.uninstall(dir.path()).is_ok());
    }

    // -- parse_event tests --

    #[test]
    fn test_parse_event_empty_input() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::TurnStart, b"");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_event_invalid_json() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::TurnStart, b"not json");
        assert!(result.is_err());
        if let Err(AgentError::HookParseFailed { agent, .. }) = result {
            assert_eq!(agent, "kilo");
        }
    }

    #[test]
    fn test_parse_event_missing_session_id() {
        let hook = make_hook();
        let input = br#"{"cwd": "/tmp"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        // Should generate a fallback session ID
        assert!(event.session_id.starts_with("kilo-"));
    }

    #[test]
    fn test_parse_event_empty_session_id() {
        let hook = make_hook();
        let input = br#"{"session_id": ""}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(event.session_id.starts_with("kilo-"));
    }

    #[test]
    fn test_parse_prompt_submit() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "abc123-kilo",
            "cwd": "/tmp/project",
            "model": "claude-sonnet-4",
            "prompt": "Fix the bug",
            "timestamp": "2026-07-15T12:00:00Z"
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "abc123-kilo");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(event.prompt.as_deref(), Some("Fix the bug"));
    }

    #[test]
    fn test_parse_agent_stop() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "abc123-kilo",
            "cwd": "/tmp/project",
            "timestamp": "2026-07-15T12:01:00Z"
        }"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "abc123-kilo");
        assert_eq!(event.event_type, HookType::TurnEnd);
    }

    #[test]
    fn test_parse_pre_tool_use() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "abc123-kilo",
            "tool_name": "write",
            "cwd": "/tmp/project"
        }"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "abc123-kilo");
        assert_eq!(event.event_type, HookType::PreToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("write"));
    }

    #[test]
    fn test_parse_post_tool_use() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "abc123-kilo",
            "tool_name": "bash",
            "tool_call_id": "call-42",
            "cwd": "/tmp/project"
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "abc123-kilo");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("bash"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-42"));
    }

    // -- Detection tests --

    #[test]
    fn test_detect_presence_with_kilo_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".kilo")).unwrap();
        let hook = make_hook();
        assert!(hook.detect_presence(dir.path()));
    }

    #[test]
    fn test_detect_presence_without_kilo_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(dir.path()));
    }

    // -- Verb mapping tests --

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
        assert_eq!(verb_to_hook_type("unknown"), None);
    }

    #[test]
    fn test_default() {
        let hook = KiloHook::default();
        assert_eq!(hook.name(), "kilo");
    }

    #[test]
    fn test_debug() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("KiloHook"));
    }
}

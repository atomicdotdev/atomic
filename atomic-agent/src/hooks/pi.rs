//! Pi agent hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing from the Pi extensions system
//! (`atomic-hooks.ts`), which pipes JSON to `atomic agent hooks pi <verb>`
//! via stdin at each lifecycle event.
//!
//! # Pi Extension Architecture
//!
//! Unlike Claude Code and Gemini CLI which have native hook systems in their
//! settings files, Pi uses a **TypeScript extension** approach. The extension
//! (`extensions/atomic-hooks.ts`) subscribes to Pi events and invokes the
//! Atomic CLI:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  Pi Extension (extensions/atomic-hooks.ts)                              │
//! │                                                                         │
//! │  session.created  ──▶  atomic agent hooks pi session-start             │
//! │  chat.message     ──▶  atomic agent hooks pi user-prompt               │
//! │  session.idle     ──▶  atomic agent hooks pi stop                      │
//! │  session.ended    ──▶  atomic agent hooks pi session-end               │
//! │  tool.exec.before ──▶  atomic agent hooks pi before-tool               │
//! │  tool.exec.after  ──▶  atomic agent hooks pi after-tool                │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Hook Verbs
//!
//! | Verb            | HookType       | Description                           |
//! |-----------------|----------------|---------------------------------------|
//! | `session-start` | SessionStart   | New Pi session created                 |
//! | `session-end`   | SessionEnd     | Session ended                          |
//! | `user-prompt`   | TurnStart      | User sends a new prompt                |
//! | `stop`          | TurnEnd        | Agent goes idle (turn complete)        |
//! | `before-tool`   | PreToolUse     | Before tool execution                  |
//! | `after-tool`    | PostToolUse    | After tool execution                   |
//!
//! # JSON Input Format
//!
//! All hooks receive JSON via stdin with at minimum:
//!
//! ```json
//! {
//!   "session_id": "abc-123",
//!   "cwd": "/path/to/project",
//!   "timestamp": "2025-01-15T10:30:00Z"
//! }
//! ```
//!
//! Additional fields vary by verb (see struct definitions below).
//!
//! # Installation
//!
//! Pi extension installation is handled by the standalone `atomic-pi`
//! package. This adapter only parses hook events.

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

/// The Pi config directory name.
const PI_DIR: &str = ".pi";

// Pi JSON Input Types

/// JSON input for session-start hook.
///
/// Sent when a new Pi session is created.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionStartInput {
    #[serde(default)]
    session_id: Option<String>,

    /// How the session started: "startup", "resume"
    #[serde(default)]
    source: Option<String>,

    /// Working directory
    #[serde(default)]
    cwd: Option<String>,

    /// ISO 8601 timestamp
    #[serde(default)]
    timestamp: Option<String>,

    /// Model identifier
    #[serde(default)]
    model: Option<String>,

    /// Provider identifier
    #[serde(default)]
    provider: Option<String>,
}

/// JSON input for session-end hook.
///
/// Sent when a Pi session ends.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionEndInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Reason the session ended: "ended", "error"
    #[serde(default)]
    reason: Option<String>,

    /// Working directory
    #[serde(default)]
    cwd: Option<String>,

    /// ISO 8601 timestamp
    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for user-prompt hook (TurnStart).
///
/// Sent when the user submits a new prompt in a Pi session.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UserPromptInput {
    #[serde(default)]
    session_id: Option<String>,

    /// The user's prompt text
    #[serde(default)]
    prompt: Option<String>,

    /// Model identifier
    #[serde(default)]
    model: Option<String>,

    /// Provider identifier
    #[serde(default)]
    provider: Option<String>,

    /// Sub-agent name, if applicable
    #[serde(default)]
    agent: Option<String>,

    /// Working directory
    #[serde(default)]
    cwd: Option<String>,

    /// ISO 8601 timestamp
    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for stop hook (TurnEnd).
///
/// Sent when the Pi agent goes idle after completing a turn.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StopInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Which turn this was (1-based)
    #[serde(default)]
    turn_number: Option<u32>,

    /// Model identifier
    #[serde(default)]
    model: Option<String>,

    /// Provider identifier
    #[serde(default)]
    provider: Option<String>,

    /// Sub-agent name, if applicable
    #[serde(default)]
    agent: Option<String>,

    /// Whether the turn ended due to an error
    #[serde(default)]
    error: Option<bool>,

    /// Token usage: input tokens consumed
    #[serde(default)]
    input_tokens: Option<u64>,

    /// Token usage: output tokens generated
    #[serde(default)]
    output_tokens: Option<u64>,

    /// Estimated cost in USD
    #[serde(default)]
    cost_usd: Option<f64>,

    /// Turn duration in milliseconds
    #[serde(default)]
    turn_duration_ms: Option<u64>,

    /// Working directory
    #[serde(default)]
    cwd: Option<String>,

    /// ISO 8601 timestamp
    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for before-tool hook (PreToolUse).
///
/// Sent before a tool is executed within a Pi session.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BeforeToolInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Name of the tool being invoked
    #[serde(default)]
    tool_name: Option<String>,

    /// Unique call identifier for this tool invocation
    #[serde(default)]
    tool_call_id: Option<String>,

    /// Tool input arguments (arbitrary JSON)
    #[serde(default)]
    tool_input: Option<serde_json::Value>,

    /// Working directory
    #[serde(default)]
    cwd: Option<String>,

    /// ISO 8601 timestamp
    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for after-tool hook (PostToolUse).
///
/// Sent after a tool finishes executing within a Pi session.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AfterToolInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Name of the tool that executed
    #[serde(default)]
    tool_name: Option<String>,

    /// Unique call identifier for this tool invocation
    #[serde(default)]
    tool_call_id: Option<String>,

    /// Execution status: "completed", "error"
    #[serde(default)]
    status: Option<String>,

    /// Execution duration in milliseconds
    #[serde(default)]
    duration: Option<u64>,

    /// Whether this tool modified files on disk
    #[serde(default)]
    modified_files: Option<bool>,

    /// Truncated tool output (first 500 chars)
    #[serde(default)]
    tool_output: Option<String>,

    /// Working directory
    #[serde(default)]
    cwd: Option<String>,

    /// ISO 8601 timestamp
    #[serde(default)]
    timestamp: Option<String>,
}

/// Pi agent hook adapter.
///
/// Parses hook JSON from the Pi extensions system. Extension installation
/// is handled by the standalone `atomic-pi` package.
#[derive(Debug)]
pub struct PiHook {
    _private: (),
}

impl PiHook {
    /// Create a new Pi hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Extract a session ID from an optional field, generating a fallback if missing.
    fn extract_session_id(session_id: Option<String>) -> String {
        session_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("pi-{}", uuid_short()))
    }
}

impl Default for PiHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHook for PiHook {
    fn name(&self) -> &str {
        "pi"
    }

    fn display_name(&self) -> &str {
        "Pi"
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
            HookType::SessionStart => {
                let parsed: SessionStartInput =
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

                // Store model and provider in raw_json for the orchestrator
                if let Some(model) = parsed.model {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("model".to_string(), serde_json::Value::String(model));
                        }
                    }
                }
                if let Some(source) = parsed.source {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("source".to_string(), serde_json::Value::String(source));
                        }
                    }
                }

                Ok(event)
            }

            HookType::SessionEnd => {
                let parsed: SessionEndInput =
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

            HookType::TurnStart => {
                let parsed: UserPromptInput =
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

                // Store model/provider for the orchestrator to read
                if let Some(model) = parsed.model {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("model".to_string(), serde_json::Value::String(model));
                        }
                    }
                }
                if let Some(provider) = parsed.provider {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("provider".to_string(), serde_json::Value::String(provider));
                        }
                    }
                }

                Ok(event)
            }

            HookType::TurnEnd => {
                let parsed: StopInput = serde_json::from_value(raw_json.clone()).map_err(|e| {
                    AgentError::HookParseFailed {
                        agent: self.name().to_string(),
                        hook_type: hook_type.as_str().to_string(),
                        reason: e.to_string(),
                    }
                })?;

                let mut event =
                    TurnEvent::new(Self::extract_session_id(parsed.session_id), hook_type)
                        .with_raw_json(raw_json);

                // Store model/provider for provenance
                if let Some(model) = parsed.model {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("model".to_string(), serde_json::Value::String(model));
                        }
                    }
                }
                if let Some(provider) = parsed.provider {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("provider".to_string(), serde_json::Value::String(provider));
                        }
                    }
                }

                Ok(event)
            }

            HookType::PreToolUse => {
                let parsed: BeforeToolInput =
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
                let parsed: AfterToolInput =
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
        }
    }

    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        Ok(0) // Installation handled by atomic-pi package
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(()) // Uninstallation handled by atomic-pi package
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        false // Managed by atomic-pi package
    }

    fn supported_hooks(&self) -> Vec<HookType> {
        vec![
            HookType::SessionStart,
            HookType::SessionEnd,
            HookType::TurnStart,
            HookType::TurnEnd,
            HookType::PreToolUse,
            HookType::PostToolUse,
        ]
    }

    fn detect_presence(&self, repo_root: &Path) -> bool {
        // Pi is present if the .pi directory exists
        repo_root.join(PI_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "session-start",
            "session-end",
            "user-prompt",
            "stop",
            "before-tool",
            "after-tool",
        ]
    }
}

// Helper: generate short pseudo-UUID for fallback session IDs

/// Generate a short hex string for fallback session IDs.
///
/// This is only used when the Pi extension fails to provide a session_id,
/// which should be rare. Uses timestamp bits for uniqueness.
fn uuid_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    // Use lower 32 bits of timestamp for a short hex ID
    format!("{:08x}", (now & 0xFFFF_FFFF) as u32)
}

// Pi hook verb → HookType mapping

/// Convert a Pi-specific verb to a [`HookType`].
///
/// Pi uses the same verb set as OpenCode:
///
/// | Verb            | HookType     |
/// |-----------------|--------------|
/// | `session-start` | SessionStart |
/// | `session-end`   | SessionEnd   |
/// | `user-prompt`   | TurnStart    |
/// | `stop`          | TurnEnd      |
/// | `before-tool`   | PreToolUse   |
/// | `after-tool`    | PostToolUse  |
pub fn verb_to_hook_type(verb: &str) -> Option<HookType> {
    match verb {
        "session-start" => Some(HookType::SessionStart),
        "session-end" => Some(HookType::SessionEnd),
        "user-prompt" => Some(HookType::TurnStart),
        "stop" => Some(HookType::TurnEnd),
        "before-tool" => Some(HookType::PreToolUse),
        "after-tool" => Some(HookType::PostToolUse),
        _ => None,
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookType;

    fn make_hook() -> PiHook {
        PiHook::new()
    }

    // Basic trait method tests

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "pi");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "Pi");
    }

    #[test]
    fn test_supported_hooks() {
        let hook = make_hook();
        let hooks = hook.supported_hooks();
        assert_eq!(hooks.len(), 6);
        assert!(hooks.contains(&HookType::SessionStart));
        assert!(hooks.contains(&HookType::SessionEnd));
        assert!(hooks.contains(&HookType::TurnStart));
        assert!(hooks.contains(&HookType::TurnEnd));
        assert!(hooks.contains(&HookType::PreToolUse));
        assert!(hooks.contains(&HookType::PostToolUse));
    }

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert_eq!(verbs.len(), 6);
        assert!(verbs.contains(&"session-start"));
        assert!(verbs.contains(&"session-end"));
        assert!(verbs.contains(&"user-prompt"));
        assert!(verbs.contains(&"stop"));
        assert!(verbs.contains(&"before-tool"));
        assert!(verbs.contains(&"after-tool"));
    }

    #[test]
    fn test_default() {
        let hook = PiHook::default();
        assert_eq!(hook.name(), "pi");
    }

    #[test]
    fn test_debug() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("PiHook"));
    }

    // parse_event tests: session-start

    #[test]
    fn test_parse_session_start() {
        let hook = make_hook();
        let input = br#"{"session_id": "test-123", "cwd": "/tmp"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "test-123");
        assert_eq!(event.event_type, HookType::SessionStart);
    }

    #[test]
    fn test_parse_session_start_with_model() {
        let hook = make_hook();
        let input =
            br#"{"session_id": "s1", "model": "claude-sonnet-4-6", "provider": "anthropic"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "claude-sonnet-4-6");
    }

    // parse_event tests: session-end

    #[test]
    fn test_parse_session_end() {
        let hook = make_hook();
        let input = br#"{"session_id": "test-123", "reason": "ended"}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        assert_eq!(event.session_id, "test-123");
        assert_eq!(event.event_type, HookType::SessionEnd);
    }

    // parse_event tests: user-prompt (TurnStart)

    #[test]
    fn test_parse_user_prompt() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "prompt": "Fix the bug", "model": "claude-sonnet-4-6", "provider": "anthropic"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.prompt.as_deref(), Some("Fix the bug"));
    }

    #[test]
    fn test_parse_user_prompt_no_prompt() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(event.prompt.is_none());
    }

    // parse_event tests: stop (TurnEnd)

    #[test]
    fn test_parse_stop() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "turn_number": 3}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.event_type, HookType::TurnEnd);
    }

    #[test]
    fn test_parse_stop_with_error() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "error": true}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "s1");
    }

    // parse_event tests: before-tool (PreToolUse)

    #[test]
    fn test_parse_before_tool() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "s1",
            "tool_name": "edit",
            "tool_call_id": "call-42",
            "tool_input": {"file": "main.rs"}
        }"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.tool_name.as_deref(), Some("edit"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-42"));
    }

    #[test]
    fn test_parse_before_tool_minimal() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1"}"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert!(event.tool_name.is_none());
    }

    // parse_event tests: after-tool (PostToolUse)

    #[test]
    fn test_parse_after_tool() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "s1",
            "tool_name": "bash",
            "tool_call_id": "call-99",
            "status": "completed",
            "duration": 1234
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.tool_name.as_deref(), Some("bash"));
    }

    // Error handling tests

    #[test]
    fn test_parse_event_empty_input() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::SessionStart, b"");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Empty hook input"));
    }

    #[test]
    fn test_parse_event_invalid_json() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::SessionStart, b"not json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // Fallback session ID tests

    #[test]
    fn test_parse_session_start_missing_session_id() {
        let hook = make_hook();
        let input = br#"{"cwd": "/tmp"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        // Should generate a fallback session ID
        assert!(event.session_id.starts_with("pi-"));
    }

    #[test]
    fn test_parse_session_start_empty_session_id() {
        let hook = make_hook();
        let input = br#"{"session_id": ""}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("pi-"));
    }

    // Extra fields should be ignored (serde default behavior)

    #[test]
    fn test_parse_extra_fields_ignored() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "unknown_field": "whatever", "extra": 42}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
    }

    // verb_to_hook_type tests

    #[test]
    fn test_verb_to_hook_type() {
        assert_eq!(
            verb_to_hook_type("session-start"),
            Some(HookType::SessionStart)
        );
        assert_eq!(verb_to_hook_type("session-end"), Some(HookType::SessionEnd));
        assert_eq!(verb_to_hook_type("user-prompt"), Some(HookType::TurnStart));
        assert_eq!(verb_to_hook_type("stop"), Some(HookType::TurnEnd));
        assert_eq!(verb_to_hook_type("before-tool"), Some(HookType::PreToolUse));
        assert_eq!(verb_to_hook_type("after-tool"), Some(HookType::PostToolUse));
        assert_eq!(verb_to_hook_type("unknown"), None);
    }

    // detect_presence tests

    #[test]
    fn test_detect_presence_with_pi_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".pi")).unwrap();
        let hook = make_hook();
        assert!(hook.detect_presence(tmp.path()));
    }

    #[test]
    fn test_detect_presence_without_pi_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(tmp.path()));
    }

    // Full roundtrip test

    #[test]
    fn test_full_roundtrip() {
        let hook = make_hook();

        // Parse all event types
        let input = br#"{"session_id": "roundtrip-test"}"#;
        for ht in hook.supported_hooks() {
            let event = hook.parse_event(ht, input).unwrap();
            assert_eq!(event.session_id, "roundtrip-test");
        }
    }

    // uuid_short helper tests

    #[test]
    fn test_uuid_short_format() {
        let id = uuid_short();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_uuid_short_not_all_zeros() {
        let id = uuid_short();
        assert_ne!(id, "00000000");
    }
}

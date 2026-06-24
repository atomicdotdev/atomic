//! Devin Desktop agent hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing from the Devin Desktop (Cascade) bridge scripts,
//! which pipe JSON to `atomic agent hooks devin <verb>` via stdin at each
//! lifecycle event.
//!
//! # Devin Desktop Hook Architecture
//!
//! Devin Desktop (Cascade) has a native hook system that fires shell commands
//! at workflow points, passing JSON context via stdin. The `atomic-devin`
//! package provides bridge scripts that translate Cascade events into Atomic
//! agent hook calls:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  Devin Desktop (Cascade) Hooks → Bridge Scripts                        │
//! │                                                                         │
//! │  pre_user_prompt (first)  ──▶  atomic agent hooks devin session-start  │
//! │  pre_user_prompt          ──▶  atomic agent hooks devin prompt-submit  │
//! │  post_cascade_response    ──▶  atomic agent hooks devin stop           │
//! │  post_write_code          ──▶  atomic agent hooks devin post-tool      │
//! │  post_run_command         ──▶  atomic agent hooks devin post-tool      │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Hook Verbs
//!
//! | Verb             | HookType     | Description                            |
//! |------------------|--------------|----------------------------------------|
//! | `session-start`  | SessionStart | First prompt creates draft view         |
//! | `session-end`    | SessionEnd   | Session cleanup                         |
//! | `prompt-submit`  | TurnStart    | User sends a new prompt                 |
//! | `stop`           | TurnEnd      | Cascade response complete (turn end)    |
//! | `post-tool`      | PostToolUse  | After file write or command execution   |
//!
//! # JSON Input Format
//!
//! All hooks receive JSON via stdin with at minimum:
//!
//! ```json
//! {
//!   "session_id": "abc-123",
//!   "cwd": "/path/to/project"
//! }
//! ```
//!
//! Additional fields vary by verb (see struct definitions below).
//!
//! # Installation
//!
//! Devin Desktop hook installation is handled by the standalone `atomic-devin`
//! package. This adapter only parses hook events.

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

/// The Devin Desktop config directory name.
const DEVIN_DIR: &str = ".devin";

// Devin JSON Input Types

/// JSON input for session-start hook.
///
/// Sent on the first `pre_user_prompt` event to create a draft view.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionStartInput {
    /// Session identifier (derived from Cascade's trajectory_id).
    #[serde(default)]
    session_id: Option<String>,
    /// Working directory.
    #[serde(default)]
    cwd: Option<String>,
    /// Model name (e.g. "claude-sonnet-4-20250514").
    #[serde(default)]
    model: Option<String>,
    /// User's first prompt.
    #[serde(default)]
    prompt: Option<String>,
}

/// JSON input for session-end hook.
///
/// Sent when the Devin Desktop session ends.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionEndInput {
    /// Session identifier.
    #[serde(default)]
    session_id: Option<String>,
    /// Optional reason for ending.
    #[serde(default)]
    reason: Option<String>,
    /// Working directory.
    #[serde(default)]
    cwd: Option<String>,
}

/// JSON input for prompt-submit hook (TurnStart).
///
/// Sent on each `pre_user_prompt` after the first.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PromptSubmitInput {
    /// Session identifier.
    #[serde(default)]
    session_id: Option<String>,
    /// The user's prompt text.
    #[serde(default)]
    prompt: Option<String>,
    /// Model name.
    #[serde(default)]
    model: Option<String>,
    /// Working directory.
    #[serde(default)]
    cwd: Option<String>,
}

/// JSON input for stop hook (TurnEnd).
///
/// Sent on `post_cascade_response` — triggers recording with provenance.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StopInput {
    /// Session identifier.
    #[serde(default)]
    session_id: Option<String>,
    /// Model name.
    #[serde(default)]
    model: Option<String>,
    /// Cascade trajectory ID.
    #[serde(default)]
    trajectory_id: Option<String>,
    /// Working directory.
    #[serde(default)]
    cwd: Option<String>,
}

/// JSON input for post-tool hook (PostToolUse).
///
/// Sent on `post_write_code` and `post_run_command`.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PostToolInput {
    /// Session identifier.
    #[serde(default)]
    session_id: Option<String>,
    /// Tool action type (e.g. "write", "command").
    #[serde(default)]
    action: Option<String>,
    /// File path for write actions.
    #[serde(default)]
    file: Option<String>,
    /// Command string for command actions.
    #[serde(default)]
    command: Option<String>,
    /// Cascade trajectory ID.
    #[serde(default)]
    trajectory_id: Option<String>,
    /// Working directory.
    #[serde(default)]
    cwd: Option<String>,
}

/// Devin Desktop agent hook adapter.
///
/// Parses hook JSON from the Devin Desktop (Cascade) bridge scripts.
/// Hook installation is handled by the standalone `atomic-devin` package.
#[derive(Debug)]
pub struct DevinHook {
    _private: (),
}

impl DevinHook {
    /// Create a new Devin Desktop hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Extract a session ID from an optional field, generating a fallback if missing.
    fn extract_session_id(session_id: Option<String>) -> String {
        session_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("devin-{}", uuid_short()))
    }
}

impl Default for DevinHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHook for DevinHook {
    fn name(&self) -> &str {
        "devin"
    }

    fn display_name(&self) -> &str {
        "Devin Desktop"
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

                if let Some(model) = parsed.model {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("model".to_string(), serde_json::Value::String(model));
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
                // Devin Desktop doesn't currently emit pre-tool events,
                // but we handle it for forward compatibility.
                let event = TurnEvent::new(format!("devin-{}", uuid_short()), hook_type)
                    .with_raw_json(raw_json);

                Ok(event)
            }

            HookType::PostToolUse => {
                let parsed: PostToolInput =
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

                // Map Devin's action field to tool_name
                if let Some(action) = parsed.action {
                    event = event.with_tool_name(action);
                }

                Ok(event)
            }
        }
    }

    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        Ok(0) // Installation handled by atomic-devin package
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(()) // Uninstallation handled by atomic-devin package
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        false // Managed by atomic-devin package
    }

    fn supported_hooks(&self) -> Vec<HookType> {
        vec![
            HookType::SessionStart,
            HookType::SessionEnd,
            HookType::TurnStart,
            HookType::TurnEnd,
            HookType::PostToolUse,
        ]
    }

    fn detect_presence(&self, repo_root: &Path) -> bool {
        // Devin is present if the .devin directory exists
        repo_root.join(DEVIN_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "session-start",
            "session-end",
            "prompt-submit",
            "stop",
            "post-tool",
        ]
    }
}

// Helper: generate short pseudo-UUID for fallback session IDs

/// Generate a short hex string for fallback session IDs.
///
/// This is only used when the bridge scripts fail to provide a session_id,
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

// Devin hook verb → HookType mapping

/// Convert a Devin-specific verb to a [`HookType`].
///
/// | Verb             | HookType     |
/// |------------------|--------------|
/// | `session-start`  | SessionStart |
/// | `session-end`    | SessionEnd   |
/// | `prompt-submit`  | TurnStart    |
/// | `stop`           | TurnEnd      |
/// | `post-tool`      | PostToolUse  |
pub fn verb_to_hook_type(verb: &str) -> Option<HookType> {
    match verb {
        "session-start" => Some(HookType::SessionStart),
        "session-end" => Some(HookType::SessionEnd),
        "prompt-submit" => Some(HookType::TurnStart),
        "stop" => Some(HookType::TurnEnd),
        "post-tool" => Some(HookType::PostToolUse),
        _ => None,
    }
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hook() -> DevinHook {
        DevinHook::new()
    }

    // --- Basic trait method tests ---

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "devin");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "Devin Desktop");
    }

    #[test]
    fn test_supported_hooks() {
        let hook = make_hook();
        let supported = hook.supported_hooks();
        assert!(supported.contains(&HookType::SessionStart));
        assert!(supported.contains(&HookType::SessionEnd));
        assert!(supported.contains(&HookType::TurnStart));
        assert!(supported.contains(&HookType::TurnEnd));
        assert!(supported.contains(&HookType::PostToolUse));
    }

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert!(verbs.contains(&"session-start"));
        assert!(verbs.contains(&"session-end"));
        assert!(verbs.contains(&"prompt-submit"));
        assert!(verbs.contains(&"stop"));
        assert!(verbs.contains(&"post-tool"));
    }

    #[test]
    fn test_default() {
        let hook = DevinHook::default();
        assert_eq!(hook.name(), "devin");
    }

    #[test]
    fn test_debug() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("DevinHook"));
    }

    // --- parse_event tests ---

    #[test]
    fn test_parse_session_start() {
        let hook = make_hook();
        let input = br#"{"session_id":"devin-123","cwd":"/tmp","model":"claude-sonnet-4"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "devin-123");
        assert_eq!(event.event_type, HookType::SessionStart);
    }

    #[test]
    fn test_parse_session_start_with_model() {
        let hook = make_hook();
        let input = br#"{"session_id":"s1","model":"claude-sonnet-4"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "claude-sonnet-4");
    }

    #[test]
    fn test_parse_session_end() {
        let hook = make_hook();
        let input = br#"{"session_id":"devin-123","reason":"user_closed"}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        assert_eq!(event.session_id, "devin-123");
        assert_eq!(event.event_type, HookType::SessionEnd);
    }

    #[test]
    fn test_parse_prompt_submit() {
        let hook = make_hook();
        let input =
            br#"{"session_id":"devin-123","prompt":"fix the bug","model":"claude-sonnet-4"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "devin-123");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(event.prompt.as_deref(), Some("fix the bug"));
    }

    #[test]
    fn test_parse_prompt_submit_no_prompt() {
        let hook = make_hook();
        let input = br#"{"session_id":"devin-123"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "devin-123");
        assert!(event.prompt.is_none());
    }

    #[test]
    fn test_parse_stop() {
        let hook = make_hook();
        let input =
            br#"{"session_id":"devin-123","model":"claude-sonnet-4","trajectory_id":"traj-1"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "devin-123");
        assert_eq!(event.event_type, HookType::TurnEnd);
    }

    #[test]
    fn test_parse_post_tool() {
        let hook = make_hook();
        let input = br#"{"session_id":"devin-123","action":"write","file":"src/main.rs"}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "devin-123");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("write"));
    }

    #[test]
    fn test_parse_post_tool_command() {
        let hook = make_hook();
        let input = br#"{"session_id":"devin-123","action":"command","command":"cargo test"}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("command"));
    }

    #[test]
    fn test_parse_event_empty_input() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::SessionStart, b"");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_event_invalid_json() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::SessionStart, b"not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_session_start_missing_session_id() {
        let hook = make_hook();
        let input = br#"{"cwd":"/tmp"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        // Should generate a fallback session ID
        assert!(event.session_id.starts_with("devin-"));
    }

    #[test]
    fn test_parse_session_start_empty_session_id() {
        let hook = make_hook();
        let input = br#"{"session_id":"","cwd":"/tmp"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("devin-"));
    }

    #[test]
    fn test_parse_extra_fields_ignored() {
        let hook = make_hook();
        let input = br#"{"session_id":"s1","unknown_field":"value","extra":42}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
    }

    // --- verb_to_hook_type tests ---

    #[test]
    fn test_verb_to_hook_type() {
        assert_eq!(
            verb_to_hook_type("session-start"),
            Some(HookType::SessionStart)
        );
        assert_eq!(verb_to_hook_type("session-end"), Some(HookType::SessionEnd));
        assert_eq!(
            verb_to_hook_type("prompt-submit"),
            Some(HookType::TurnStart)
        );
        assert_eq!(verb_to_hook_type("stop"), Some(HookType::TurnEnd));
        assert_eq!(verb_to_hook_type("post-tool"), Some(HookType::PostToolUse));
        assert_eq!(verb_to_hook_type("unknown"), None);
    }

    // --- detect_presence tests ---

    #[test]
    fn test_detect_presence_with_devin_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(DEVIN_DIR)).unwrap();
        let hook = make_hook();
        assert!(hook.detect_presence(dir.path()));
    }

    #[test]
    fn test_detect_presence_without_devin_dir() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(dir.path()));
    }

    // --- install/uninstall tests ---

    #[test]
    fn test_install_is_noop() {
        let hook = make_hook();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(hook.install(dir.path()).unwrap(), 0);
    }

    #[test]
    fn test_uninstall_is_noop() {
        let hook = make_hook();
        let dir = tempfile::tempdir().unwrap();
        hook.uninstall(dir.path()).unwrap();
    }

    #[test]
    fn test_is_installed_always_false() {
        let hook = make_hook();
        let dir = tempfile::tempdir().unwrap();
        assert!(!hook.is_installed(dir.path()));
    }

    // --- uuid_short tests ---

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

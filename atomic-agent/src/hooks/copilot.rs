//! GitHub Copilot hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing, installation into `.github/hooks/atomic-hooks.json`,
//! and presence detection via the `.github/` directory.
//!
//! # Copilot Hooks Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  GitHub Copilot Hooks (.github/hooks/*.json)                            │
//! │                                                                         │
//! │  sessionStart        ──▶  atomic agent hooks copilot session-start     │
//! │  sessionEnd          ──▶  atomic agent hooks copilot session-end       │
//! │  userPromptSubmitted ──▶  atomic agent hooks copilot user-prompt       │
//! │  postToolUse         ──▶  atomic agent hooks copilot post-tool         │
//! │  preToolUse          ──▶  atomic agent hooks copilot pre-tool          │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Config Format
//!
//! Copilot uses `.github/hooks/*.json` (project-level). Any JSON file in that
//! directory is loaded. User-level hooks may live at `~/.copilot/hooks.json`
//! but this is undocumented.
//!
//! The format uses camelCase event names and `bash`/`powershell` keys instead
//! of `command`:
//!
//! ```json
//! {
//!   "version": 1,
//!   "hooks": {
//!     "sessionStart": [
//!       {
//!         "type": "command",
//!         "bash": "atomic agent hooks copilot session-start",
//!         "timeoutSec": 30
//!       }
//!     ]
//!   }
//! }
//! ```
//!
//! # Hook Verbs
//!
//! | Verb            | HookType       | Description                            |
//! |-----------------|----------------|----------------------------------------|
//! | `session-start` | SessionStart   | New Copilot session created             |
//! | `session-end`   | SessionEnd     | Session ended                           |
//! | `user-prompt`   | TurnStart      | User submits a new prompt               |
//! | `post-tool`     | PostToolUse    | After tool execution                    |
//! | `pre-tool`      | PreToolUse     | Before tool execution                   |
//!
//! # Differences from Other Adapters
//!
//! | Aspect          | Claude Code / Codex          | Copilot                       |
//! |-----------------|------------------------------|-------------------------------|
//! | Config dir      | `.claude/` / `.codex/`       | `.github/hooks/`              |
//! | Config file     | `settings.json` / `hooks.json` | `atomic-hooks.json`        |
//! | Event names     | PascalCase                   | camelCase                     |
//! | Command key     | `command`                    | `bash` / `powershell`         |
//! | TurnEnd / Stop  | `Stop` event                 | No `stop` event               |
//! | JSON fields     | snake_case                   | camelCase with snake aliases  |

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

// =============================================================================
// Constants
// =============================================================================

/// The `.github` directory at the repo root.
const GITHUB_DIR: &str = ".github";

// =============================================================================
// Copilot JSON Input Types
// =============================================================================

/// JSON input for session-start hook.
///
/// Sent when a new Copilot session is created.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionStartInput {
    #[serde(default)]
    timestamp: Option<u64>,

    #[serde(default)]
    cwd: Option<String>,

    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,

    #[serde(default)]
    model: Option<String>,
}

/// JSON input for session-end hook.
///
/// Sent when a Copilot session ends.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionEndInput {
    #[serde(default)]
    timestamp: Option<u64>,

    #[serde(default)]
    cwd: Option<String>,

    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,

    #[serde(default)]
    model: Option<String>,

    #[serde(default)]
    reason: Option<String>,
}

/// JSON input for user-prompt hook (TurnStart).
///
/// Sent when the user submits a prompt to the AI.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UserPromptInput {
    #[serde(default)]
    timestamp: Option<u64>,

    #[serde(default)]
    cwd: Option<String>,

    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,

    #[serde(default)]
    model: Option<String>,

    #[serde(default)]
    prompt: Option<String>,
}

/// JSON input for pre-tool hook (PreToolUse).
///
/// Sent before Copilot executes a tool.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PreToolUseInput {
    #[serde(default)]
    timestamp: Option<u64>,

    #[serde(default)]
    cwd: Option<String>,

    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,

    #[serde(default)]
    model: Option<String>,

    #[serde(default, alias = "toolName")]
    tool_name: Option<String>,

    #[serde(default, alias = "toolArgs")]
    tool_args: Option<serde_json::Value>,

    #[serde(default, alias = "toolUseId")]
    tool_use_id: Option<String>,
}

/// JSON input for post-tool hook (PostToolUse).
///
/// Sent after Copilot executes a tool.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PostToolUseInput {
    #[serde(default)]
    timestamp: Option<u64>,

    #[serde(default)]
    cwd: Option<String>,

    #[serde(default, alias = "sessionId")]
    session_id: Option<String>,

    #[serde(default)]
    model: Option<String>,

    #[serde(default, alias = "toolName")]
    tool_name: Option<String>,

    #[serde(default, alias = "toolArgs")]
    tool_args: Option<serde_json::Value>,

    #[serde(default, alias = "toolUseId")]
    tool_use_id: Option<String>,

    #[serde(default, alias = "toolResponse")]
    tool_response: Option<serde_json::Value>,

    #[serde(default)]
    duration: Option<u64>,
}

// =============================================================================
// CopilotHook
// =============================================================================

/// GitHub Copilot agent hook adapter.
///
/// Handles hook JSON parsing from Copilot's hook system, installation into
/// `.github/hooks/atomic-hooks.json`, and presence detection via `.github/`.
///
/// # Differences from Claude Code / Codex
///
/// | Aspect          | Claude Code / Codex      | Copilot                     |
/// |-----------------|-------------------------|-----------------------------|
/// | Config location | `.claude/` / `.codex/`   | `.github/hooks/`            |
/// | Event names     | PascalCase               | camelCase                   |
/// | Command key     | `command`                | `bash` / `powershell`       |
/// | Turn boundary   | `Stop` / `session.idle`  | No stop event               |
/// | Prompt capture  | `UserPromptSubmit`       | `userPromptSubmitted`       |
#[derive(Debug)]
pub struct CopilotHook {
    _private: (),
}

impl CopilotHook {
    /// Create a new Copilot hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Extract a session ID from an optional field, generating a fallback if missing.
    fn extract_session_id(session_id: Option<String>) -> String {
        session_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("copilot-{}", uuid_short()))
    }
}

impl Default for CopilotHook {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// AgentHook Trait Implementation
// =============================================================================

impl AgentHook for CopilotHook {
    fn name(&self) -> &str {
        "copilot"
    }

    fn display_name(&self) -> &str {
        "GitHub Copilot"
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

                // Store model for the orchestrator to read
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
                let parsed: PreToolUseInput =
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
                if let Some(id) = parsed.tool_use_id {
                    event = event.with_tool_use_id(id);
                }

                Ok(event)
            }

            HookType::PostToolUse => {
                let parsed: PostToolUseInput =
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
                if let Some(id) = parsed.tool_use_id {
                    event = event.with_tool_use_id(id);
                }

                Ok(event)
            }

            // Copilot does NOT have a TurnEnd / stop event — we record on
            // sessionEnd instead. Return an error if TurnEnd is requested.
            HookType::TurnEnd => Err(AgentError::HookParseFailed {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: "Copilot does not support TurnEnd hooks; recording happens on sessionEnd"
                    .to_string(),
            }),
        }
    }

    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        Ok(0) // Installation handled by atomic-copilot package
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(()) // Uninstallation handled by atomic-copilot package
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        false // Managed by atomic-copilot package
    }

    fn supported_hooks(&self) -> Vec<HookType> {
        vec![
            HookType::SessionStart,
            HookType::SessionEnd,
            HookType::TurnStart,
            HookType::PreToolUse,
            HookType::PostToolUse,
        ]
    }

    fn detect_presence(&self, repo_root: &Path) -> bool {
        // Copilot works in any GitHub repo — detect via .github/ directory
        repo_root.join(GITHUB_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "session-start",
            "session-end",
            "user-prompt",
            "post-tool",
            "pre-tool",
        ]
    }
}

// =============================================================================
// Verb Mapping
// =============================================================================

/// Convert a Copilot-specific verb to a [`HookType`].
///
/// | Verb            | HookType     |
/// |-----------------|--------------|
/// | `session-start` | SessionStart |
/// | `session-end`   | SessionEnd   |
/// | `user-prompt`   | TurnStart    |
/// | `post-tool`     | PostToolUse  |
/// | `pre-tool`      | PreToolUse   |
pub fn verb_to_hook_type(verb: &str) -> Option<HookType> {
    match verb {
        "session-start" => Some(HookType::SessionStart),
        "session-end" => Some(HookType::SessionEnd),
        "user-prompt" => Some(HookType::TurnStart),
        "post-tool" => Some(HookType::PostToolUse),
        "pre-tool" => Some(HookType::PreToolUse),
        _ => None,
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Generate a short hex string for fallback session IDs.
///
/// Only used when Copilot fails to provide a session_id, which should be
/// rare. Uses timestamp bits for uniqueness.
fn uuid_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    // Use lower 32 bits of timestamp for a short hex ID
    format!("{:08x}", (now & 0xFFFF_FFFF) as u32)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookType;

    fn make_hook() -> CopilotHook {
        CopilotHook::new()
    }

    // ── Basic trait method tests ────────────────────────────────────

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "copilot");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "GitHub Copilot");
    }

    #[test]
    fn test_supported_hooks() {
        let hook = make_hook();
        let hooks = hook.supported_hooks();
        assert_eq!(hooks.len(), 5);
        assert!(hooks.contains(&HookType::SessionStart));
        assert!(hooks.contains(&HookType::SessionEnd));
        assert!(hooks.contains(&HookType::TurnStart));
        assert!(hooks.contains(&HookType::PreToolUse));
        assert!(hooks.contains(&HookType::PostToolUse));
        // No TurnEnd for Copilot
        assert!(!hooks.contains(&HookType::TurnEnd));
    }

    #[test]
    fn test_supported_hooks_count() {
        let hook = make_hook();
        assert_eq!(hook.supported_hooks().len(), 5);
    }

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert_eq!(verbs.len(), 5);
        assert!(verbs.contains(&"session-start"));
        assert!(verbs.contains(&"session-end"));
        assert!(verbs.contains(&"user-prompt"));
        assert!(verbs.contains(&"post-tool"));
        assert!(verbs.contains(&"pre-tool"));
    }

    #[test]
    fn test_default() {
        let hook = CopilotHook::default();
        assert_eq!(hook.name(), "copilot");
    }

    #[test]
    fn test_debug() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("CopilotHook"));
    }

    // ── parse_event tests: session-start ────────────────────────────

    #[test]
    fn test_parse_session_start() {
        let hook = make_hook();
        let input = br#"{"session_id": "cp-abc123", "timestamp": 1700000000, "cwd": "/tmp/proj"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "cp-abc123");
        assert_eq!(event.event_type, HookType::SessionStart);
    }

    #[test]
    fn test_parse_session_start_camel_case_session_id() {
        let hook = make_hook();
        let input = br#"{"sessionId": "cp-camel", "timestamp": 1700000000}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "cp-camel");
    }

    #[test]
    fn test_parse_session_start_with_model() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "model": "gpt-4o"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "gpt-4o");
    }

    #[test]
    fn test_parse_session_start_missing_session_id() {
        let hook = make_hook();
        let input = br#"{"timestamp": 1700000000}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("copilot-"));
    }

    #[test]
    fn test_parse_session_start_empty_session_id() {
        let hook = make_hook();
        let input = br#"{"session_id": ""}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("copilot-"));
    }

    // ── parse_event tests: session-end ──────────────────────────────

    #[test]
    fn test_parse_session_end() {
        let hook = make_hook();
        let input = br#"{"session_id": "cp-abc123", "reason": "user_exit"}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        assert_eq!(event.session_id, "cp-abc123");
        assert_eq!(event.event_type, HookType::SessionEnd);
    }

    #[test]
    fn test_parse_session_end_with_reason() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "reason": "timeout"}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["reason"], "timeout");
    }

    // ── parse_event tests: user-prompt (TurnStart) ──────────────────

    #[test]
    fn test_parse_user_prompt() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "prompt": "Fix the auth bug in login.rs"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(
            event.prompt.as_deref(),
            Some("Fix the auth bug in login.rs")
        );
    }

    #[test]
    fn test_parse_user_prompt_no_prompt() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(event.prompt.is_none());
    }

    #[test]
    fn test_parse_user_prompt_with_model() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "prompt": "hello", "model": "gpt-4o"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "gpt-4o");
    }

    // ── parse_event tests: pre-tool (PreToolUse) ────────────────────

    #[test]
    fn test_parse_pre_tool() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "s1",
            "toolName": "edit",
            "toolArgs": {"filePath": "src/main.rs"},
            "tool_use_id": "call-42"
        }"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.event_type, HookType::PreToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("edit"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-42"));
    }

    #[test]
    fn test_parse_pre_tool_snake_case() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "tool_name": "bash", "tool_use_id": "call-1"}"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("bash"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn test_parse_pre_tool_minimal() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1"}"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert!(event.tool_name.is_none());
        assert!(event.tool_use_id.is_none());
    }

    // ── parse_event tests: post-tool (PostToolUse) ──────────────────

    #[test]
    fn test_parse_post_tool() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "s1",
            "toolName": "bash",
            "toolArgs": {"command": "ls -la"},
            "tool_use_id": "call-99",
            "toolResponse": {"exitCode": 0},
            "duration": 1500
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("bash"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-99"));
    }

    #[test]
    fn test_parse_post_tool_snake_case_fields() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "s1",
            "tool_name": "edit",
            "tool_use_id": "call-5",
            "tool_response": {"success": true}
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("edit"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-5"));
    }

    #[test]
    fn test_parse_post_tool_no_tool_name() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1"}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert!(event.tool_name.is_none());
    }

    // ── parse_event tests: TurnEnd not supported ────────────────────

    #[test]
    fn test_parse_turn_end_unsupported() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1"}"#;
        let result = hook.parse_event(HookType::TurnEnd, input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AgentError::HookParseFailed { .. }));
    }

    // ── parse_event tests: error cases ──────────────────────────────

    #[test]
    fn test_parse_event_empty_input() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::SessionStart, b"");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AgentError::HookInputEmpty { .. }));
    }

    #[test]
    fn test_parse_event_invalid_json() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::SessionStart, b"not json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AgentError::HookParseFailed { .. }));
    }

    #[test]
    fn test_parse_extra_fields_ignored() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "unknown_field": 42, "another": "value"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
    }

    #[test]
    fn test_parse_minimal_input() {
        let hook = make_hook();
        let input = br#"{}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        // Should generate a fallback session ID
        assert!(event.session_id.starts_with("copilot-"));
    }

    // ── verb_to_hook_type tests ─────────────────────────────────────

    #[test]
    fn test_verb_to_hook_type() {
        assert_eq!(
            verb_to_hook_type("session-start"),
            Some(HookType::SessionStart)
        );
        assert_eq!(verb_to_hook_type("session-end"), Some(HookType::SessionEnd));
        assert_eq!(verb_to_hook_type("user-prompt"), Some(HookType::TurnStart));
        assert_eq!(verb_to_hook_type("post-tool"), Some(HookType::PostToolUse));
        assert_eq!(verb_to_hook_type("pre-tool"), Some(HookType::PreToolUse));
        assert_eq!(verb_to_hook_type("unknown"), None);
        assert_eq!(verb_to_hook_type("stop"), None); // No stop verb for Copilot
    }

    // ── detect_presence tests ───────────────────────────────────────

    #[test]
    fn test_detect_presence_with_github_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".github")).unwrap();
        let hook = make_hook();
        assert!(hook.detect_presence(tmp.path()));
    }

    #[test]
    fn test_detect_presence_without_github_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(tmp.path()));
    }

    #[test]
    fn test_detect_presence_file_not_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".github"), "not a dir").unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(tmp.path()));
    }

    // ── Installation is a no-op (managed by atomic-copilot package) ──

    #[test]
    fn test_install_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = make_hook();
        let count = hook.install(tmp.path()).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_uninstall_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(hook.uninstall(tmp.path()).is_ok());
    }

    #[test]
    fn test_is_installed_always_false() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.is_installed(tmp.path()));
    }

    // ── Session ID extraction tests ─────────────────────────────────

    #[test]
    fn test_extract_session_id_with_value() {
        let id = CopilotHook::extract_session_id(Some("cp-123".to_string()));
        assert_eq!(id, "cp-123");
    }

    #[test]
    fn test_extract_session_id_empty_generates_fallback() {
        let id = CopilotHook::extract_session_id(Some("".to_string()));
        assert!(id.starts_with("copilot-"));
    }

    #[test]
    fn test_extract_session_id_none_generates_fallback() {
        let id = CopilotHook::extract_session_id(None);
        assert!(id.starts_with("copilot-"));
    }

    // ── uuid_short tests ────────────────────────────────────────────

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

    // ── Parse all events roundtrip ──────────────────────────────────

    #[test]
    fn test_parse_all_events_roundtrip() {
        let hook = make_hook();

        let test_cases = vec![
            (HookType::SessionStart, br#"{"session_id":"s1"}"#.as_slice()),
            (HookType::SessionEnd, br#"{"session_id":"s1"}"#.as_slice()),
            (
                HookType::TurnStart,
                br#"{"session_id":"s1","prompt":"fix bug"}"#.as_slice(),
            ),
            (
                HookType::PreToolUse,
                br#"{"session_id":"s1","toolName":"edit"}"#.as_slice(),
            ),
            (
                HookType::PostToolUse,
                br#"{"session_id":"s1","toolName":"bash","duration":500}"#.as_slice(),
            ),
        ];

        for (hook_type, input) in test_cases {
            let event = hook
                .parse_event(hook_type, input)
                .unwrap_or_else(|e| panic!("Failed to parse {:?}: {}", hook_type, e));
            assert_eq!(event.session_id, "s1");
            assert_eq!(event.event_type, hook_type);
        }
    }

    #[test]
    fn test_debug_impl() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("CopilotHook"));
    }
}

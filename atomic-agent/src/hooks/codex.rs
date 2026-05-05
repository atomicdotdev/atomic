//! Codex hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing, installation into `.codex/hooks.json`,
//! and presence detection via the `.codex/` directory.
//!
//! # Codex Hooks Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  Codex Hooks (.codex/hooks.json)                                        │
//! │                                                                         │
//! │  SessionStart      ──▶  atomic agent hooks codex session-start         │
//! │  UserPromptSubmit  ──▶  atomic agent hooks codex user-prompt-submit    │
//! │  Stop              ──▶  atomic agent hooks codex stop                  │
//! │  PostToolUse       ──▶  atomic agent hooks codex post-tool             │
//! │  PreToolUse        ──▶  atomic agent hooks codex pre-tool              │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Config Format
//!
//! Codex uses `.codex/hooks.json` (project-level) or `~/.codex/hooks.json`
//! (user-level). The format uses PascalCase event names:
//!
//! ```json
//! {
//!   "hooks": {
//!     "SessionStart": [{ "hooks": [{ "type": "command", "command": "..." }] }],
//!     "PostToolUse": [{ "hooks": [{ "type": "command", "command": "..." }] }]
//!   }
//! }
//! ```
//!
//! # Exit Code Behavior
//!
//! Same as Claude Code: exit 0 = success, exit 2 = block.
//!
//! # Limitations
//!
//! - No `SessionEnd` event (Codex does not fire one)
//! - `PostToolUse` currently only fires for Bash tool (WIP)
//! - Feature flag `[features] codex_hooks = true` required in config.toml

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};

use super::AgentHook;

// =============================================================================
// Constants
// =============================================================================

/// The directory where Codex stores per-project configuration.
const CODEX_DIR: &str = ".codex";

// =============================================================================
// Codex JSON Input Types
// =============================================================================

/// JSON input for the `SessionStart` hook.
///
/// Fires when Codex begins a new session.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionStartInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
}

/// JSON input for the `UserPromptSubmit` hook (maps to TurnStart).
///
/// Fires after the user submits a prompt, before the model begins responding.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UserPromptSubmitInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
}

/// JSON input for the `Stop` hook (maps to TurnEnd).
///
/// Fires when the model finishes responding for the turn.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StopInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    stop_hook_active: Option<bool>,
    #[serde(default)]
    last_assistant_message: Option<String>,
}

/// JSON input for the `PostToolUse` hook.
///
/// Currently only fires for the Bash tool (WIP for other tools).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PostToolUseInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_response: Option<serde_json::Value>,
}

/// JSON input for the `PreToolUse` hook.
///
/// Fires before a tool is invoked.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PreToolUseInput {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
}

// =============================================================================
// =============================================================================
// CodexHook
// =============================================================================

/// Codex agent hook adapter.
///
/// Handles hook JSON parsing, installation into `.codex/hooks.json`,
/// and presence detection via the `.codex/` directory.
///
/// Codex is OpenAI's coding agent. It uses a hooks system with PascalCase
/// event names and a simpler config format than Claude Code.
#[derive(Debug)]
pub struct CodexHook {
    _private: (), // prevent construction outside of new()
}

impl CodexHook {
    /// Create a new Codex hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Extract a session ID from an optional field, generating a fallback if missing.
    ///
    /// Codex provides stable session IDs, so we use them directly. If missing
    /// (shouldn't happen in practice), fall back to a generated ID.
    fn extract_session_id(session_id: Option<String>) -> String {
        session_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("codex-{}", uuid_short()))
    }
}

impl Default for CodexHook {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// AgentHook Trait Implementation
// =============================================================================

impl AgentHook for CodexHook {
    fn name(&self) -> &str {
        "codex"
    }

    fn display_name(&self) -> &str {
        "Codex"
    }

    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
        if input.is_empty() {
            return Err(AgentError::HookInputEmpty {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
            });
        }

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

                let session_id = Self::extract_session_id(parsed.session_id);
                let mut event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }

                Ok(event)
            }

            HookType::TurnStart => {
                // Codex: UserPromptSubmit → TurnStart
                let parsed: UserPromptSubmitInput = serde_json::from_value(raw_json.clone())
                    .map_err(|e| AgentError::HookParseFailed {
                        agent: self.name().to_string(),
                        hook_type: hook_type.as_str().to_string(),
                        reason: e.to_string(),
                    })?;

                let session_id = Self::extract_session_id(parsed.session_id);
                let mut event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                if let Some(prompt) = parsed.prompt {
                    event = event.with_prompt(prompt);
                }

                Ok(event)
            }

            HookType::TurnEnd => {
                // Codex: Stop → TurnEnd
                let parsed: StopInput = serde_json::from_value(raw_json.clone()).map_err(|e| {
                    AgentError::HookParseFailed {
                        agent: self.name().to_string(),
                        hook_type: hook_type.as_str().to_string(),
                        reason: e.to_string(),
                    }
                })?;

                let session_id = Self::extract_session_id(parsed.session_id);
                let mut event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
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

                let session_id = Self::extract_session_id(parsed.session_id);
                let mut event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
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

                let session_id = Self::extract_session_id(parsed.session_id);
                let mut event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);

                if let Some(path) = parsed.transcript_path {
                    event = event.with_transcript_path(path);
                }
                if let Some(name) = parsed.tool_name {
                    event = event.with_tool_name(name);
                }
                if let Some(id) = parsed.tool_use_id {
                    event = event.with_tool_use_id(id);
                }

                Ok(event)
            }

            // Codex does NOT have a SessionEnd event
            HookType::SessionEnd => Err(AgentError::HookParseFailed {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
                reason: "Codex does not support SessionEnd hooks".to_string(),
            }),
        }
    }

    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        Ok(0) // Installation handled by atomic-codex package
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(()) // Uninstallation handled by atomic-codex package
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        false // Managed by atomic-codex package
    }

    fn supported_hooks(&self) -> Vec<HookType> {
        vec![
            HookType::SessionStart,
            HookType::TurnStart,
            HookType::TurnEnd,
            HookType::PreToolUse,
            HookType::PostToolUse,
        ]
    }

    fn detect_presence(&self, repo_root: &Path) -> bool {
        repo_root.join(CODEX_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "session-start",
            "user-prompt-submit",
            "stop",
            "post-tool",
            "pre-tool",
        ]
    }
}

// =============================================================================
// Verb Mapping
// =============================================================================

/// Map Codex hook verbs to Atomic HookTypes.
///
/// These are registered in addition to the standard verbs in
/// [`HookType::from_verb`]. The CLI dispatch layer checks both.
pub fn verb_to_hook_type(verb: &str) -> Option<HookType> {
    match verb {
        "session-start" => Some(HookType::SessionStart),
        "user-prompt-submit" => Some(HookType::TurnStart),
        "stop" => Some(HookType::TurnEnd),
        "post-tool" => Some(HookType::PostToolUse),
        "pre-tool" => Some(HookType::PreToolUse),
        _ => None,
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Generate a short hex ID from the current timestamp.
///
/// Used as a fallback session ID when Codex doesn't provide one.
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

    fn make_hook() -> CodexHook {
        CodexHook::new()
    }

    // =========================================================================
    // Identity tests
    // =========================================================================

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "codex");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "Codex");
    }

    #[test]
    fn test_default() {
        let hook = CodexHook::default();
        assert_eq!(hook.name(), "codex");
    }

    #[test]
    fn test_supported_hooks() {
        let hook = make_hook();
        let supported = hook.supported_hooks();
        assert!(supported.contains(&HookType::SessionStart));
        assert!(supported.contains(&HookType::TurnStart));
        assert!(supported.contains(&HookType::TurnEnd));
        assert!(supported.contains(&HookType::PreToolUse));
        assert!(supported.contains(&HookType::PostToolUse));
        // Codex does NOT support SessionEnd
        assert!(!supported.contains(&HookType::SessionEnd));
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
        assert!(verbs.contains(&"user-prompt-submit"));
        assert!(verbs.contains(&"stop"));
        assert!(verbs.contains(&"post-tool"));
        assert!(verbs.contains(&"pre-tool"));
    }

    // =========================================================================
    // Verb mapping tests
    // =========================================================================

    #[test]
    fn test_verb_to_hook_type() {
        assert_eq!(
            verb_to_hook_type("session-start"),
            Some(HookType::SessionStart)
        );
        assert_eq!(
            verb_to_hook_type("user-prompt-submit"),
            Some(HookType::TurnStart)
        );
        assert_eq!(verb_to_hook_type("stop"), Some(HookType::TurnEnd));
        assert_eq!(verb_to_hook_type("post-tool"), Some(HookType::PostToolUse));
        assert_eq!(verb_to_hook_type("pre-tool"), Some(HookType::PreToolUse));
        assert_eq!(verb_to_hook_type("unknown"), None);
        assert_eq!(verb_to_hook_type(""), None);
    }

    // =========================================================================
    // Parse event tests
    // =========================================================================

    #[test]
    fn test_parse_session_start() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "sess-abc-123",
            "transcript_path": "/tmp/codex-transcript.json",
            "cwd": "/home/user/project",
            "model": "o3-mini",
            "source": "startup",
            "hook_event_name": "SessionStart"
        }"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "sess-abc-123");
        assert_eq!(event.event_type, HookType::SessionStart);
        assert!(event.transcript_path.is_some());
        assert!(event.raw_json.is_some());
    }

    #[test]
    fn test_parse_session_start_extracts_model_in_raw_json() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "model": "o3-mini"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw.get("model").and_then(|v| v.as_str()), Some("o3-mini"));
    }

    #[test]
    fn test_parse_user_prompt_submit() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "sess-abc-123",
            "transcript_path": "/tmp/t.json",
            "cwd": "/home/user/project",
            "model": "o3-mini",
            "prompt": "Fix the login bug in auth.rs",
            "turn_id": "turn-001"
        }"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "sess-abc-123");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(
            event.prompt.as_deref(),
            Some("Fix the login bug in auth.rs")
        );
    }

    #[test]
    fn test_parse_stop() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "sess-abc-123",
            "transcript_path": "/tmp/t.json",
            "cwd": "/home/user/project",
            "model": "o3-mini",
            "turn_id": "turn-001",
            "stop_hook_active": true,
            "last_assistant_message": "Done! I fixed the bug."
        }"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "sess-abc-123");
        assert_eq!(event.event_type, HookType::TurnEnd);
    }

    #[test]
    fn test_parse_pre_tool_use() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "sess-abc-123",
            "tool_name": "Bash",
            "tool_use_id": "tool-42",
            "tool_input": {"command": "ls -la"}
        }"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "sess-abc-123");
        assert_eq!(event.event_type, HookType::PreToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(event.tool_use_id.as_deref(), Some("tool-42"));
    }

    #[test]
    fn test_parse_post_tool_use() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "sess-abc-123",
            "tool_name": "Bash",
            "tool_use_id": "tool-42",
            "tool_input": {"command": "ls -la"},
            "tool_response": {"output": "total 0\ndrwxr-xr-x 2 user user 64 Jan 1 00:00 ."}
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "sess-abc-123");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(event.tool_use_id.as_deref(), Some("tool-42"));
    }

    #[test]
    fn test_parse_session_end_unsupported() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1"}"#;
        let result = hook.parse_event(HookType::SessionEnd, input);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("SessionEnd"));
    }

    #[test]
    fn test_parse_empty_input() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::SessionStart, b"");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_invalid_json() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::SessionStart, b"not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_missing_session_id_generates_fallback() {
        let hook = make_hook();
        let input = br#"{"transcript_path": "/tmp/t.json"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("codex-"));
    }

    #[test]
    fn test_parse_empty_session_id_generates_fallback() {
        let hook = make_hook();
        let input = br#"{"session_id": ""}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("codex-"));
    }

    #[test]
    fn test_parse_minimal_input() {
        let hook = make_hook();
        // Codex sends at least {} — all fields are optional with serde(default)
        let input = br#"{}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("codex-"));
        assert_eq!(event.event_type, HookType::SessionStart);
    }

    #[test]
    fn test_parse_turn_start_no_prompt() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert!(event.prompt.is_none());
    }

    #[test]
    fn test_parse_post_tool_no_tool_name() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1"}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert!(event.tool_name.is_none());
    }

    #[test]
    fn test_parse_pre_tool_no_tool_use_id() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "tool_name": "Bash"}"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
        assert!(event.tool_use_id.is_none());
    }

    // =========================================================================
    // Detection tests
    // =========================================================================

    #[test]
    fn test_detect_presence_with_codex_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".codex")).unwrap();

        let hook = make_hook();
        assert!(hook.detect_presence(dir.path()));
    }

    #[test]
    fn test_detect_presence_without_codex_dir() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(dir.path()));
    }

    // =========================================================================
    // Install / uninstall are no-ops (managed by atomic-codex package)
    // =========================================================================

    #[test]
    fn test_install_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        let count = hook.install(dir.path()).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_uninstall_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(hook.uninstall(dir.path()).is_ok());
    }

    #[test]
    fn test_is_installed_always_false() {
        let dir = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.is_installed(dir.path()));
    }

    // =========================================================================
    // extract_session_id tests
    // =========================================================================

    #[test]
    fn test_extract_session_id_with_value() {
        let id = CodexHook::extract_session_id(Some("sess-123".to_string()));
        assert_eq!(id, "sess-123");
    }

    #[test]
    fn test_extract_session_id_empty_generates_fallback() {
        let id = CodexHook::extract_session_id(Some("".to_string()));
        assert!(id.starts_with("codex-"));
    }

    #[test]
    fn test_extract_session_id_none_generates_fallback() {
        let id = CodexHook::extract_session_id(None);
        assert!(id.starts_with("codex-"));
    }

    // =========================================================================
    // hook_command_exists tests
    // =========================================================================

    // =========================================================================
    // uuid_short helper tests
    // =========================================================================

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

    // =========================================================================
    // Roundtrip / integration tests
    // =========================================================================

    #[test]
    fn test_parse_all_events_roundtrip() {
        let hook = make_hook();

        // SessionStart
        let input = br#"{"session_id": "s1", "model": "o3-mini"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "s1");

        // TurnStart (UserPromptSubmit)
        let input = br#"{"session_id": "s1", "prompt": "do something"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.prompt.as_deref(), Some("do something"));

        // TurnEnd (Stop)
        let input = br#"{"session_id": "s1", "stop_hook_active": true}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "s1");

        // PreToolUse
        let input = br#"{"session_id": "s1", "tool_name": "Bash"}"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));

        // PostToolUse
        let input =
            br#"{"session_id": "s1", "tool_name": "Bash", "tool_response": {"output": "ok"}}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn test_debug_impl() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("CodexHook"));
    }
}

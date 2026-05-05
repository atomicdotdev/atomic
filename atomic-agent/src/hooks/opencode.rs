//! OpenCode agent hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing from the OpenCode hooks plugin
//! (`atomic-hooks.ts`), which pipes JSON to `atomic agent hooks opencode <verb>`
//! via stdin at each lifecycle event.
//!
//! # OpenCode Plugin Architecture
//!
//! Unlike Claude Code and Gemini CLI which have native hook systems in their
//! settings files, OpenCode uses a **plugin-based** approach. The TypeScript
//! plugin (`.opencode/plugins/atomic-hooks.ts`) subscribes to OpenCode events
//! and invokes the Atomic CLI:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  OpenCode Plugin (.opencode/plugins/atomic-hooks.ts)                    │
//! │                                                                         │
//! │  session.created  ──▶  atomic agent hooks opencode session-start       │
//! │  chat.message     ──▶  atomic agent hooks opencode user-prompt         │
//! │  session.idle     ──▶  atomic agent hooks opencode stop                │
//! │  session.deleted  ──▶  atomic agent hooks opencode session-end         │
//! │  tool.exec.before ──▶  atomic agent hooks opencode before-tool         │
//! │  tool.exec.after  ──▶  atomic agent hooks opencode after-tool          │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Hook Verbs
//!
//! | Verb            | HookType       | Description                           |
//! |-----------------|----------------|---------------------------------------|
//! | `session-start` | SessionStart   | New OpenCode session created           |
//! | `session-end`   | SessionEnd     | Session deleted or ended               |
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
//! Plugin installation is handled by the standalone `atomic-opencode` package,
//! which places the `atomic-hooks.ts` plugin file in `.opencode/plugins/`.
//! This adapter only handles parsing hook events from the installed plugin.

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

/// The OpenCode config directory name.
const OPENCODE_DIR: &str = ".opencode";

// OpenCode JSON Input Types

/// JSON input for session-start hook.
///
/// Sent when a new OpenCode session is created.
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

    /// ISO timestamp
    #[serde(default)]
    timestamp: Option<String>,

    /// Model identifier if known at session start
    #[serde(default)]
    model: Option<String>,

    /// Provider identifier if known at session start
    #[serde(default)]
    provider: Option<String>,
}

/// JSON input for session-end hook.
///
/// Sent when an OpenCode session is deleted or ended.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionEndInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Why the session ended: "deleted", "exit", "error"
    #[serde(default)]
    reason: Option<String>,

    #[serde(default)]
    cwd: Option<String>,

    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for user-prompt hook (TurnStart).
///
/// Sent when the user submits a new prompt to the AI.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UserPromptInput {
    #[serde(default)]
    session_id: Option<String>,

    /// The user's prompt text
    #[serde(default)]
    prompt: Option<String>,

    /// Model identifier (e.g., "claude-sonnet-4-20250514")
    #[serde(default)]
    model: Option<String>,

    /// Provider identifier (e.g., "anthropic", "openai")
    #[serde(default)]
    provider: Option<String>,

    /// Agent mode: "build", "code", "ask"
    #[serde(default)]
    agent: Option<String>,

    #[serde(default)]
    cwd: Option<String>,

    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for stop hook (TurnEnd).
///
/// Sent when the agent goes idle after completing a turn.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StopInput {
    #[serde(default)]
    session_id: Option<String>,

    /// Which turn number this is (1-based)
    #[serde(default)]
    turn_number: Option<u32>,

    /// Model identifier
    #[serde(default)]
    model: Option<String>,

    /// Provider identifier
    #[serde(default)]
    provider: Option<String>,

    /// Agent mode: "build", "code", "ask"
    #[serde(default)]
    agent: Option<String>,

    /// Whether the turn ended due to an error
    #[serde(default)]
    error: Option<bool>,

    /// Input/prompt tokens used in this turn
    #[serde(default)]
    input_tokens: Option<u64>,

    /// Output/completion tokens generated in this turn
    #[serde(default)]
    output_tokens: Option<u64>,

    /// Reasoning/thinking tokens (extended thinking, o1/o3)
    #[serde(default)]
    reasoning_tokens: Option<u64>,

    /// Cache read tokens
    #[serde(default)]
    cache_read_tokens: Option<u64>,

    /// Cache write tokens
    #[serde(default)]
    cache_write_tokens: Option<u64>,

    /// Cost in USD for this turn
    #[serde(default)]
    cost_usd: Option<f64>,

    /// Actual wall-clock turn duration in milliseconds.
    /// Computed by the plugin as the time from chat.message to session.idle.
    /// More accurate than the Rust-side computation which only measures the
    /// gap between the user-prompt and stop CLI invocations.
    #[serde(default)]
    turn_duration_ms: Option<u64>,

    /// Number of LLM steps (model invocations) in this turn
    #[serde(default)]
    step_count: Option<u32>,

    /// Why the model stopped on the final step: "stop", "tool-calls", "length"
    #[serde(default)]
    finish_reason: Option<String>,

    /// Human-readable session slug (e.g., "mighty-rocket")
    #[serde(default)]
    session_slug: Option<String>,

    /// Concatenated reasoning text from all thinking blocks in this turn
    #[serde(default)]
    reasoning_text: Option<String>,

    /// Cryptographic signature from the model provider on the last reasoning block
    #[serde(default)]
    reasoning_signature: Option<String>,

    /// Agent's structured task plan at turn completion (JSON array of todos)
    #[serde(default)]
    todos: Option<serde_json::Value>,

    #[serde(default)]
    cwd: Option<String>,

    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for before-tool hook (PreToolUse).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BeforeToolInput {
    #[serde(default)]
    session_id: Option<String>,

    #[serde(default)]
    tool_name: Option<String>,

    #[serde(default)]
    tool_call_id: Option<String>,

    #[serde(default)]
    tool_input: Option<serde_json::Value>,

    #[serde(default)]
    cwd: Option<String>,

    #[serde(default)]
    timestamp: Option<String>,
}

/// JSON input for after-tool hook (PostToolUse).
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AfterToolInput {
    #[serde(default)]
    session_id: Option<String>,

    #[serde(default)]
    tool_name: Option<String>,

    #[serde(default)]
    tool_call_id: Option<String>,

    /// "completed" or "error"
    #[serde(default)]
    status: Option<String>,

    /// Duration of tool execution in milliseconds
    #[serde(default)]
    duration: Option<u64>,

    /// Whether the tool modified files
    #[serde(default)]
    modified_files: Option<bool>,

    /// Truncated tool output (up to 500 chars from the plugin)
    #[serde(default)]
    tool_output: Option<String>,

    /// Human-readable title (e.g., "Install TypeScript as dev dependency")
    #[serde(default)]
    title: Option<String>,

    /// Absolute file path for write/edit tools
    #[serde(default)]
    file_path: Option<String>,

    /// Structured file diff: { file, before, after, additions, deletions }
    #[serde(default)]
    filediff: Option<serde_json::Value>,

    /// LSP diagnostics at time of edit: { "/path/file.ts": [{ range, message }] }
    #[serde(default)]
    diagnostics: Option<serde_json::Value>,

    /// Exit code for bash tools
    #[serde(default)]
    exit_code: Option<i32>,

    #[serde(default)]
    cwd: Option<String>,

    #[serde(default)]
    timestamp: Option<String>,
}

// OpenCodeHook

/// OpenCode agent hook adapter.
///
/// Handles hook JSON parsing from the OpenCode hooks plugin.
/// Plugin installation is managed by the standalone `atomic-opencode` package.
///
/// # Differences from Claude Code / Gemini CLI
///
/// | Aspect          | Claude Code / Gemini CLI | OpenCode              |
/// |-----------------|-------------------------|-----------------------|
/// | Hook system     | Native settings.json     | Plugin-based (.ts)    |
/// | Installation    | Modify JSON config       | Copy plugin file      |
/// | Invocation      | Agent calls CLI directly | Plugin calls CLI      |
/// | Config dir      | `.claude/` / `.gemini/`  | `.opencode/plugins/`  |
/// | Turn boundary   | `stop` / `AfterAgent`    | `session.idle` event  |
/// | Prompt capture  | `UserPromptSubmit`       | `chat.message` hook   |
#[derive(Debug)]
pub struct OpenCodeHook {
    _private: (),
}

impl OpenCodeHook {
    /// Create a new OpenCode hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Extract a session ID from an optional field, generating a fallback if missing.
    fn extract_session_id(session_id: Option<String>) -> String {
        session_id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("opencode-{}", uuid_short()))
    }
}

impl Default for OpenCodeHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHook for OpenCodeHook {
    fn name(&self) -> &str {
        "opencode"
    }

    fn display_name(&self) -> &str {
        "OpenCode"
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
        Ok(0) // Installation handled by atomic-opencode package
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(()) // Uninstallation handled by atomic-opencode package
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        false // Managed by atomic-opencode package
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
        // OpenCode is present if the .opencode directory exists
        repo_root.join(OPENCODE_DIR).is_dir()
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
/// This is only used when the OpenCode plugin fails to provide a session_id,
/// which should be rare. Uses timestamp + random bits for uniqueness.
fn uuid_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    // Use lower 32 bits of timestamp for a short hex ID
    format!("{:08x}", (now & 0xFFFF_FFFF) as u32)
}

// OpenCode hook verb → HookType mapping (registered in event.rs)

/// Convert an OpenCode-specific verb to a [`HookType`].
///
/// This function is called from [`HookType::from_verb`] for OpenCode verbs.
///
/// | Verb            | HookType     |
/// |-----------------|--------------|
/// | `user-prompt`   | TurnStart    |
/// | `stop`          | TurnEnd      |
/// | `before-tool`   | PreToolUse   |
/// | `after-tool`    | PostToolUse  |
/// | `session-start` | SessionStart |
/// | `session-end`   | SessionEnd   |
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
    use tempfile::TempDir;

    fn make_hook() -> OpenCodeHook {
        OpenCodeHook::new()
    }

    // Basic trait method tests

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "opencode");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "OpenCode");
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
        let hook = OpenCodeHook::default();
        assert_eq!(hook.name(), "opencode");
    }

    #[test]
    fn test_debug() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("OpenCodeHook"));
    }

    // parse_event tests: session-start

    #[test]
    fn test_parse_session_start() {
        let hook = make_hook();
        let input = br#"{"session_id": "oc-abc123", "source": "startup", "cwd": "/tmp/proj"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "oc-abc123");
        assert_eq!(event.event_type, HookType::SessionStart);
    }

    #[test]
    fn test_parse_session_start_with_model() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "model": "claude-sonnet-4", "provider": "anthropic"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "s1");
        // Model should be in raw_json
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "claude-sonnet-4");
    }

    // parse_event tests: session-end

    #[test]
    fn test_parse_session_end() {
        let hook = make_hook();
        let input = br#"{"session_id": "oc-abc123", "reason": "deleted"}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        assert_eq!(event.session_id, "oc-abc123");
        assert_eq!(event.event_type, HookType::SessionEnd);
    }

    // parse_event tests: user-prompt (TurnStart)

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
        let input =
            br#"{"session_id": "s1", "prompt": "hello", "model": "gpt-4o", "provider": "openai"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "gpt-4o");
        assert_eq!(raw["provider"], "openai");
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
        let input = br#"{"session_id": "s1", "turn_number": 2, "error": true}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["error"], true);
    }

    // parse_event tests: before-tool (PreToolUse)

    #[test]
    fn test_parse_before_tool() {
        let hook = make_hook();
        let input = br#"{
            "session_id": "s1",
            "tool_name": "edit",
            "tool_call_id": "call-42",
            "tool_input": {"filePath": "src/main.rs"}
        }"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.event_type, HookType::PreToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("edit"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-42"));
    }

    #[test]
    fn test_parse_before_tool_minimal() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1"}"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert!(event.tool_name.is_none());
        assert!(event.tool_use_id.is_none());
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
            "duration": 1500,
            "modified_files": true
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "s1");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("bash"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-99"));
    }

    // parse_event tests: error cases

    #[test]
    fn test_parse_event_empty_input() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::TurnEnd, b"");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AgentError::HookInputEmpty { .. }));
    }

    #[test]
    fn test_parse_event_invalid_json() {
        let hook = make_hook();
        let result = hook.parse_event(HookType::TurnEnd, b"not json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AgentError::HookParseFailed { .. }));
    }

    #[test]
    fn test_parse_session_start_missing_session_id() {
        let hook = make_hook();
        let input = br#"{"source": "startup"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        // Should generate a fallback session ID
        assert!(event.session_id.starts_with("opencode-"));
    }

    #[test]
    fn test_parse_session_start_empty_session_id() {
        let hook = make_hook();
        let input = br#"{"session_id": ""}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("opencode-"));
    }

    #[test]
    fn test_parse_extra_fields_ignored() {
        let hook = make_hook();
        let input = br#"{"session_id": "s1", "unknown_field": 42, "another": "value"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
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

    // Installation tests

    #[test]
    fn test_detect_presence_with_opencode_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".opencode")).unwrap();
        let hook = make_hook();
        assert!(hook.detect_presence(tmp.path()));
    }

    #[test]
    fn test_detect_presence_without_opencode_dir() {
        let tmp = TempDir::new().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(tmp.path()));
    }

    #[test]
    fn test_detect_presence_file_not_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".opencode"), "not a dir").unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(tmp.path()));
    }

    #[test]
    fn test_install_is_noop() {
        let tmp = TempDir::new().unwrap();
        let hook = make_hook();
        let count = hook.install(tmp.path()).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_uninstall_is_noop() {
        let tmp = TempDir::new().unwrap();
        let hook = make_hook();
        hook.uninstall(tmp.path()).unwrap();
    }

    #[test]
    fn test_is_installed_returns_false() {
        let tmp = TempDir::new().unwrap();
        let hook = make_hook();
        assert!(!hook.is_installed(tmp.path()));
    }

    // uuid_short tests

    #[test]
    fn test_uuid_short_format() {
        let id = uuid_short();
        assert_eq!(id.len(), 8);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_uuid_short_not_all_zeros() {
        // Unless system clock is exactly at epoch, should not be all zeros
        let id = uuid_short();
        assert_ne!(id, "00000000");
    }
}

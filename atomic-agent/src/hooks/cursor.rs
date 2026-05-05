//! Cursor agent hook adapter for Atomic Agent.
//!
//! Handles hook JSON parsing from Cursor's hooks system
//! (`.cursor/hooks.json`), which pipes JSON to
//! `atomic agent hooks cursor <verb>` via stdin at each lifecycle event.
//!
//! # Cursor Hooks Architecture
//!
//! Cursor uses a JSON-based hooks configuration file (`.cursor/hooks.json`)
//! rather than a settings.json or TypeScript extension. Each hook event
//! receives JSON via stdin with common fields plus event-specific fields.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │  Cursor Hooks (.cursor/hooks.json)                                      │
//! │                                                                         │
//! │  sessionStart        ──▶  atomic agent hooks cursor session-start      │
//! │  sessionEnd          ──▶  atomic agent hooks cursor session-end        │
//! │  beforeSubmitPrompt  ──▶  atomic agent hooks cursor user-prompt-submit │
//! │  stop                ──▶  atomic agent hooks cursor stop               │
//! │  postToolUse         ──▶  atomic agent hooks cursor post-tool          │
//! │  afterAgentThought   ──▶  atomic agent hooks cursor after-thought      │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Hook Verbs
//!
//! | Verb                  | HookType     | Cursor Event          | Description                    |
//! |-----------------------|--------------|-----------------------|--------------------------------|
//! | `session-start`       | SessionStart | sessionStart          | New Cursor session created     |
//! | `session-end`         | SessionEnd   | sessionEnd            | Session ended                  |
//! | `user-prompt-submit`  | TurnStart    | beforeSubmitPrompt    | User sends prompt              |
//! | `stop`                | TurnEnd      | stop                  | Agent finishes responding      |
//! | `post-tool`           | PostToolUse  | postToolUse           | After tool execution           |
//! | `after-thought`       | PostToolUse  | afterAgentThought     | Agent thinking block           |
//!
//! # JSON Input Format
//!
//! All hooks receive JSON via stdin with common fields:
//!
//! ```json
//! {
//!   "conversation_id": "string",
//!   "generation_id": "string",
//!   "model": "string",
//!   "hook_event_name": "string",
//!   "cursor_version": "string",
//!   "workspace_roots": ["<path>"],
//!   "user_email": "string | null",
//!   "transcript_path": "string | null"
//! }
//! ```
//!
//! **Key difference from Claude Code**: Cursor uses `conversation_id` (stable
//! across turns) rather than `session_id` (except in sessionStart/sessionEnd
//! which also carry `session_id`). The `model` field is present in EVERY hook
//! input.
//!
//! # Installation
//!
//! Cursor hooks are installed by writing entries into `.cursor/hooks.json`.
//! The `install()` method reads the existing file (or creates it), adds
//! atomic hook commands for each event, and writes it back — preserving
//! any existing non-atomic hooks.

use std::path::Path;

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

/// The Cursor config directory name.
const CURSOR_DIR: &str = ".cursor";

// ─────────────────────────────────────────────────────────────────────────────
// Cursor JSON Input Types
// ─────────────────────────────────────────────────────────────────────────────

/// Fields present in ALL Cursor hook inputs.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct CommonInput {
    /// Stable across turns within a conversation.
    #[serde(default)]
    conversation_id: Option<String>,

    /// Unique per generation (model response).
    #[serde(default)]
    generation_id: Option<String>,

    /// Model identifier (present in every hook).
    #[serde(default)]
    model: Option<String>,

    /// The Cursor hook event name (e.g. "sessionStart").
    #[serde(default)]
    hook_event_name: Option<String>,

    /// Cursor application version.
    #[serde(default)]
    cursor_version: Option<String>,

    /// Workspace root directories.
    #[serde(default)]
    workspace_roots: Option<Vec<String>>,

    /// User email (may be null for anonymous).
    #[serde(default)]
    user_email: Option<String>,

    /// Path to the conversation transcript file.
    #[serde(default)]
    transcript_path: Option<String>,
}

/// JSON input for sessionStart hook.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionStartInput {
    // Common fields
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    generation_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    cursor_version: Option<String>,
    #[serde(default)]
    workspace_roots: Option<Vec<String>>,
    #[serde(default)]
    user_email: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,

    // Session-specific fields
    /// Session identifier (present in sessionStart/sessionEnd).
    #[serde(default)]
    session_id: Option<String>,

    /// Whether this is a background agent session.
    #[serde(default)]
    is_background_agent: Option<bool>,

    /// Composer mode (e.g. "normal", "agent").
    #[serde(default)]
    composer_mode: Option<String>,
}

/// JSON input for sessionEnd hook.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SessionEndInput {
    // Common fields
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    generation_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    cursor_version: Option<String>,
    #[serde(default)]
    workspace_roots: Option<Vec<String>>,
    #[serde(default)]
    user_email: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,

    // Session-specific fields
    #[serde(default)]
    session_id: Option<String>,

    /// Reason the session ended (e.g. "user_quit", "timeout").
    #[serde(default)]
    reason: Option<String>,

    /// Duration of the session in milliseconds.
    #[serde(default)]
    duration_ms: Option<u64>,

    /// Whether this was a background agent session.
    #[serde(default)]
    is_background_agent: Option<bool>,

    /// Error message if the session ended abnormally.
    #[serde(default)]
    error_message: Option<String>,
}

/// JSON input for beforeSubmitPrompt hook.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BeforeSubmitPromptInput {
    // Common fields
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    generation_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    cursor_version: Option<String>,
    #[serde(default)]
    workspace_roots: Option<Vec<String>>,
    #[serde(default)]
    user_email: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,

    // Prompt-specific fields
    /// The user's prompt text.
    #[serde(default)]
    prompt: Option<String>,
}

/// JSON input for stop hook.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StopInput {
    // Common fields
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    generation_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    cursor_version: Option<String>,
    #[serde(default)]
    workspace_roots: Option<Vec<String>>,
    #[serde(default)]
    user_email: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,

    // Stop-specific fields
    /// Status of the agent response (e.g. "completed", "error").
    #[serde(default)]
    status: Option<String>,

    /// Number of agentic loops completed.
    #[serde(default)]
    loop_count: Option<u32>,
}

/// JSON input for postToolUse hook.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PostToolUseInput {
    // Common fields
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    generation_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    cursor_version: Option<String>,
    #[serde(default)]
    workspace_roots: Option<Vec<String>>,
    #[serde(default)]
    user_email: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,

    // Tool-specific fields
    /// Name of the tool that was used.
    #[serde(default)]
    tool_name: Option<String>,

    /// Tool input parameters.
    #[serde(default)]
    tool_input: Option<serde_json::Value>,

    /// Tool output text.
    #[serde(default)]
    tool_output: Option<String>,

    /// Unique ID for this tool use.
    #[serde(default)]
    tool_use_id: Option<String>,

    /// Duration of tool execution in milliseconds.
    #[serde(default)]
    duration: Option<u64>,

    /// Working directory for the tool.
    #[serde(default)]
    cwd: Option<String>,
}

/// JSON input for afterAgentThought hook.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AfterAgentThoughtInput {
    // Common fields
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    generation_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    cursor_version: Option<String>,
    #[serde(default)]
    workspace_roots: Option<Vec<String>>,
    #[serde(default)]
    user_email: Option<String>,
    #[serde(default)]
    transcript_path: Option<String>,

    // Thought-specific fields
    /// The thinking/reasoning text.
    #[serde(default)]
    text: Option<String>,

    /// Duration of the thinking block in milliseconds.
    #[serde(default)]
    duration_ms: Option<u64>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Cursor hook event names in hooks.json
// ─────────────────────────────────────────────────────────────────────────────
// CursorHook
// ─────────────────────────────────────────────────────────────────────────────

/// Cursor agent hook adapter.
///
/// Handles hook JSON parsing from Cursor's hooks system and manages
/// hook installation in `.cursor/hooks.json`.
///
/// # Differences from Claude Code
///
/// | Aspect            | Claude Code              | Cursor                    |
/// |-------------------|--------------------------|---------------------------|
/// | Hook config       | `.claude/settings.json`  | `.cursor/hooks.json`      |
/// | Config format     | Settings with hooks list | Dedicated hooks JSON      |
/// | Session ID        | `session_id` field       | `conversation_id` (stable)|
/// | Model field       | Only in some hooks       | Present in ALL hooks      |
/// | Thinking blocks   | Not exposed              | `afterAgentThought` hook  |
/// | Turn boundary     | `stop`                   | `stop`                    |
/// | Prompt capture    | `UserPromptSubmit`       | `beforeSubmitPrompt`      |
#[derive(Debug)]
pub struct CursorHook {
    _private: (),
}

impl CursorHook {
    /// Create a new Cursor hook adapter.
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Extract a session ID from conversation_id and session_id fields.
    ///
    /// Prefers `conversation_id` (stable across turns), falls back to
    /// `session_id`, then generates a fallback `cursor-{uuid_short()}`.
    fn extract_session_id(conversation_id: Option<String>, session_id: Option<String>) -> String {
        conversation_id
            .filter(|s| !s.is_empty())
            .or_else(|| session_id.filter(|s| !s.is_empty()))
            .unwrap_or_else(|| format!("cursor-{}", uuid_short()))
    }
}

impl Default for CursorHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHook for CursorHook {
    fn name(&self) -> &str {
        "cursor"
    }

    fn display_name(&self) -> &str {
        "Cursor"
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

                // SessionStart: prefer session_id, fall back to conversation_id
                let session_id =
                    Self::extract_session_id(parsed.conversation_id, parsed.session_id);

                let mut event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);

                if let Some(transcript) = parsed.transcript_path {
                    event = event.with_transcript_path(transcript);
                }

                // Store model, composer_mode, is_background_agent in raw_json
                if let Some(model) = parsed.model {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("model".to_string(), serde_json::Value::String(model));
                        }
                    }
                }
                if let Some(mode) = parsed.composer_mode {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert(
                                "composer_mode".to_string(),
                                serde_json::Value::String(mode),
                            );
                        }
                    }
                }
                if let Some(bg) = parsed.is_background_agent {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert(
                                "is_background_agent".to_string(),
                                serde_json::Value::Bool(bg),
                            );
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

                // SessionEnd: prefer session_id, fall back to conversation_id
                let session_id =
                    Self::extract_session_id(parsed.conversation_id, parsed.session_id);

                let mut event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);

                if let Some(transcript) = parsed.transcript_path {
                    event = event.with_transcript_path(transcript);
                }

                // Store reason, duration_ms in raw_json
                if let Some(reason) = parsed.reason {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("reason".to_string(), serde_json::Value::String(reason));
                        }
                    }
                }
                if let Some(dur) = parsed.duration_ms {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert(
                                "duration_ms".to_string(),
                                serde_json::Value::Number(dur.into()),
                            );
                        }
                    }
                }

                Ok(event)
            }

            HookType::TurnStart => {
                let parsed: BeforeSubmitPromptInput = serde_json::from_value(raw_json.clone())
                    .map_err(|e| AgentError::HookParseFailed {
                        agent: self.name().to_string(),
                        hook_type: hook_type.as_str().to_string(),
                        reason: e.to_string(),
                    })?;

                // TurnStart: use conversation_id as session_id
                let session_id = Self::extract_session_id(parsed.conversation_id, None);

                let mut event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);

                if let Some(transcript) = parsed.transcript_path {
                    event = event.with_transcript_path(transcript);
                }
                if let Some(prompt) = parsed.prompt {
                    event = event.with_prompt(prompt);
                }

                // Store model in raw_json
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

                // TurnEnd: use conversation_id as session_id
                let session_id = Self::extract_session_id(parsed.conversation_id, None);

                let mut event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);

                if let Some(transcript) = parsed.transcript_path {
                    event = event.with_transcript_path(transcript);
                }

                // Store model, status in raw_json
                if let Some(model) = parsed.model {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("model".to_string(), serde_json::Value::String(model));
                        }
                    }
                }
                if let Some(status) = parsed.status {
                    if let Some(ref mut raw) = event.raw_json {
                        if let Some(obj) = raw.as_object_mut() {
                            obj.insert("status".to_string(), serde_json::Value::String(status));
                        }
                    }
                }

                Ok(event)
            }

            HookType::PreToolUse => {
                // Cursor doesn't have a pre-tool hook, but handle gracefully
                let common: CommonInput =
                    serde_json::from_value(raw_json.clone()).map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                let session_id = Self::extract_session_id(common.conversation_id, None);
                let event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);
                Ok(event)
            }

            HookType::PostToolUse => {
                // PostToolUse handles both postToolUse and afterAgentThought.
                // We differentiate by checking for the "text" field (thought)
                // vs "tool_name" field (tool use).

                // Try afterAgentThought first (has "text" field)
                if raw_json.get("text").is_some() {
                    let parsed: AfterAgentThoughtInput = serde_json::from_value(raw_json.clone())
                        .map_err(|e| {
                        AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        }
                    })?;

                    let session_id = Self::extract_session_id(parsed.conversation_id, None);

                    let mut event = TurnEvent::new(session_id, hook_type)
                        .with_tool_name("AgentThought".to_string())
                        .with_raw_json(raw_json);

                    if let Some(transcript) = parsed.transcript_path {
                        event = event.with_transcript_path(transcript);
                    }

                    // Store reasoning_text and duration for inject_reasoning_nodes
                    if let Some(text) = parsed.text {
                        if let Some(ref mut raw) = event.raw_json {
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert(
                                    "reasoning_text".to_string(),
                                    serde_json::Value::String(text),
                                );
                            }
                        }
                    }
                    if let Some(dur) = parsed.duration_ms {
                        if let Some(ref mut raw) = event.raw_json {
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert(
                                    "duration".to_string(),
                                    serde_json::Value::Number(dur.into()),
                                );
                            }
                        }
                    }
                    if let Some(model) = parsed.model {
                        if let Some(ref mut raw) = event.raw_json {
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert("model".to_string(), serde_json::Value::String(model));
                            }
                        }
                    }

                    Ok(event)
                } else {
                    // Standard postToolUse
                    let parsed: PostToolUseInput = serde_json::from_value(raw_json.clone())
                        .map_err(|e| AgentError::HookParseFailed {
                            agent: self.name().to_string(),
                            hook_type: hook_type.as_str().to_string(),
                            reason: e.to_string(),
                        })?;

                    let session_id = Self::extract_session_id(parsed.conversation_id, None);

                    let mut event = TurnEvent::new(session_id, hook_type).with_raw_json(raw_json);

                    if let Some(transcript) = parsed.transcript_path {
                        event = event.with_transcript_path(transcript);
                    }
                    if let Some(name) = parsed.tool_name {
                        event = event.with_tool_name(name);
                    }
                    if let Some(id) = parsed.tool_use_id {
                        event = event.with_tool_use_id(id);
                    }

                    // Store model, duration, tool_output in raw_json
                    if let Some(model) = parsed.model {
                        if let Some(ref mut raw) = event.raw_json {
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert("model".to_string(), serde_json::Value::String(model));
                            }
                        }
                    }
                    if let Some(dur) = parsed.duration {
                        if let Some(ref mut raw) = event.raw_json {
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert(
                                    "duration".to_string(),
                                    serde_json::Value::Number(dur.into()),
                                );
                            }
                        }
                    }
                    if let Some(output) = parsed.tool_output {
                        if let Some(ref mut raw) = event.raw_json {
                            if let Some(obj) = raw.as_object_mut() {
                                obj.insert(
                                    "tool_output".to_string(),
                                    serde_json::Value::String(output),
                                );
                            }
                        }
                    }

                    Ok(event)
                }
            }
        }
    }

    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        Ok(0) // Installation handled by atomic-cursor package
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(()) // Uninstallation handled by atomic-cursor package
    }

    fn is_installed(&self, _repo_root: &Path) -> bool {
        false // Managed by atomic-cursor package
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
        repo_root.join(CURSOR_DIR).is_dir()
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "session-start",
            "session-end",
            "user-prompt-submit",
            "stop",
            "post-tool",
            "after-thought",
        ]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Generate a short hex string for fallback session IDs.
///
/// Only used when Cursor fails to provide a conversation_id or session_id,
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

/// Convert a Cursor-specific verb to a [`HookType`].
///
/// | Verb                  | HookType     |
/// |-----------------------|--------------|
/// | `session-start`       | SessionStart |
/// | `session-end`         | SessionEnd   |
/// | `user-prompt-submit`  | TurnStart    |
/// | `stop`                | TurnEnd      |
/// | `post-tool`           | PostToolUse  |
/// | `after-thought`       | PostToolUse  |
pub fn verb_to_hook_type(verb: &str) -> Option<HookType> {
    match verb {
        "session-start" => Some(HookType::SessionStart),
        "session-end" => Some(HookType::SessionEnd),
        "user-prompt-submit" => Some(HookType::TurnStart),
        "stop" => Some(HookType::TurnEnd),
        "post-tool" | "after-thought" => Some(HookType::PostToolUse),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::HookType;

    fn make_hook() -> CursorHook {
        CursorHook::new()
    }

    // Basic trait method tests

    #[test]
    fn test_name() {
        let hook = make_hook();
        assert_eq!(hook.name(), "cursor");
    }

    #[test]
    fn test_display_name() {
        let hook = make_hook();
        assert_eq!(hook.display_name(), "Cursor");
    }

    #[test]
    fn test_supported_hooks() {
        let hook = make_hook();
        let hooks = hook.supported_hooks();
        assert_eq!(hooks.len(), 5);
        assert!(hooks.contains(&HookType::SessionStart));
        assert!(hooks.contains(&HookType::SessionEnd));
        assert!(hooks.contains(&HookType::TurnStart));
        assert!(hooks.contains(&HookType::TurnEnd));
        assert!(hooks.contains(&HookType::PostToolUse));
    }

    #[test]
    fn test_hook_verbs() {
        let hook = make_hook();
        let verbs = hook.hook_verbs();
        assert_eq!(verbs.len(), 6);
        assert!(verbs.contains(&"session-start"));
        assert!(verbs.contains(&"session-end"));
        assert!(verbs.contains(&"user-prompt-submit"));
        assert!(verbs.contains(&"stop"));
        assert!(verbs.contains(&"post-tool"));
        assert!(verbs.contains(&"after-thought"));
    }

    #[test]
    fn test_default() {
        let hook = CursorHook::default();
        assert_eq!(hook.name(), "cursor");
    }

    #[test]
    fn test_debug() {
        let hook = make_hook();
        let debug = format!("{:?}", hook);
        assert!(debug.contains("CursorHook"));
    }

    // parse_event tests: session-start

    #[test]
    fn test_parse_session_start() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "conv-123", "session_id": "sess-456"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        // conversation_id is preferred
        assert_eq!(event.session_id, "conv-123");
        assert_eq!(event.event_type, HookType::SessionStart);
    }

    #[test]
    fn test_parse_session_start_falls_back_to_session_id() {
        let hook = make_hook();
        let input = br#"{"session_id": "sess-456"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "sess-456");
    }

    #[test]
    fn test_parse_session_start_with_model() {
        let hook = make_hook();
        let input =
            br#"{"conversation_id": "c1", "model": "claude-sonnet-4-6", "composer_mode": "agent"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "c1");
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "claude-sonnet-4-6");
        assert_eq!(raw["composer_mode"], "agent");
    }

    #[test]
    fn test_parse_session_start_with_background_agent() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1", "is_background_agent": true}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["is_background_agent"], true);
    }

    #[test]
    fn test_parse_session_start_with_transcript_path() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1", "transcript_path": "/tmp/transcript.jsonl"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(
            event.transcript_path.as_deref(),
            Some(std::path::Path::new("/tmp/transcript.jsonl"))
        );
    }

    // parse_event tests: session-end

    #[test]
    fn test_parse_session_end() {
        let hook = make_hook();
        let input =
            br#"{"conversation_id": "conv-123", "session_id": "sess-456", "reason": "user_quit"}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        assert_eq!(event.session_id, "conv-123");
        assert_eq!(event.event_type, HookType::SessionEnd);
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["reason"], "user_quit");
    }

    #[test]
    fn test_parse_session_end_with_duration() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1", "reason": "timeout", "duration_ms": 120000}"#;
        let event = hook.parse_event(HookType::SessionEnd, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["duration_ms"], 120000);
    }

    // parse_event tests: beforeSubmitPrompt (TurnStart)

    #[test]
    fn test_parse_turn_start() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1", "prompt": "Fix the bug", "model": "gpt-4o"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "c1");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(event.prompt.as_deref(), Some("Fix the bug"));
    }

    #[test]
    fn test_parse_turn_start_no_prompt() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(event.prompt.is_none());
    }

    #[test]
    fn test_parse_turn_start_model_in_raw_json() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1", "model": "claude-sonnet-4-6"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "claude-sonnet-4-6");
    }

    // parse_event tests: stop (TurnEnd)

    #[test]
    fn test_parse_turn_end() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1", "status": "completed"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        assert_eq!(event.session_id, "c1");
        assert_eq!(event.event_type, HookType::TurnEnd);
    }

    #[test]
    fn test_parse_turn_end_with_model_and_status() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1", "model": "gpt-4o", "status": "error"}"#;
        let event = hook.parse_event(HookType::TurnEnd, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["model"], "gpt-4o");
        assert_eq!(raw["status"], "error");
    }

    // parse_event tests: postToolUse (PostToolUse)

    #[test]
    fn test_parse_post_tool() {
        let hook = make_hook();
        let input = br#"{
            "conversation_id": "c1",
            "tool_name": "edit_file",
            "tool_use_id": "tu-42",
            "model": "claude-sonnet-4-6",
            "duration": 1500
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "c1");
        assert_eq!(event.tool_name.as_deref(), Some("edit_file"));
        assert_eq!(event.tool_use_id.as_deref(), Some("tu-42"));
    }

    #[test]
    fn test_parse_post_tool_with_output() {
        let hook = make_hook();
        let input = br#"{
            "conversation_id": "c1",
            "tool_name": "bash",
            "tool_output": "Hello world\n",
            "duration": 200
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["tool_output"], "Hello world\n");
        assert_eq!(raw["duration"], 200);
    }

    #[test]
    fn test_parse_post_tool_minimal() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1"}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert!(event.tool_name.is_none());
        assert!(event.tool_use_id.is_none());
    }

    // parse_event tests: afterAgentThought (PostToolUse with "text")

    #[test]
    fn test_parse_after_thought() {
        let hook = make_hook();
        let input = br#"{
            "conversation_id": "c1",
            "text": "I need to check the file structure first",
            "duration_ms": 500,
            "model": "claude-sonnet-4-6"
        }"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.session_id, "c1");
        assert_eq!(event.event_type, HookType::PostToolUse);
        assert_eq!(event.tool_name.as_deref(), Some("AgentThought"));
        let raw = event.raw_json.unwrap();
        assert_eq!(
            raw["reasoning_text"],
            "I need to check the file structure first"
        );
        assert_eq!(raw["duration"], 500);
        assert_eq!(raw["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn test_parse_after_thought_minimal() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1", "text": "thinking..."}"#;
        let event = hook.parse_event(HookType::PostToolUse, input).unwrap();
        assert_eq!(event.tool_name.as_deref(), Some("AgentThought"));
        let raw = event.raw_json.unwrap();
        assert_eq!(raw["reasoning_text"], "thinking...");
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
    fn test_parse_session_start_missing_ids() {
        let hook = make_hook();
        let input = br#"{"model": "gpt-4o"}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("cursor-"));
    }

    #[test]
    fn test_parse_session_start_empty_conversation_id() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "", "session_id": ""}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert!(event.session_id.starts_with("cursor-"));
    }

    #[test]
    fn test_parse_turn_start_missing_conversation_id() {
        let hook = make_hook();
        let input = br#"{"prompt": "hello"}"#;
        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert!(event.session_id.starts_with("cursor-"));
        assert_eq!(event.prompt.as_deref(), Some("hello"));
    }

    // Extra fields should be ignored (serde default behavior)

    #[test]
    fn test_parse_extra_fields_ignored() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1", "unknown_field": "whatever", "extra": 42}"#;
        let event = hook.parse_event(HookType::SessionStart, input).unwrap();
        assert_eq!(event.session_id, "c1");
    }

    // verb_to_hook_type tests

    #[test]
    fn test_verb_to_hook_type() {
        assert_eq!(
            verb_to_hook_type("session-start"),
            Some(HookType::SessionStart)
        );
        assert_eq!(verb_to_hook_type("session-end"), Some(HookType::SessionEnd));
        assert_eq!(
            verb_to_hook_type("user-prompt-submit"),
            Some(HookType::TurnStart)
        );
        assert_eq!(verb_to_hook_type("stop"), Some(HookType::TurnEnd));
        assert_eq!(verb_to_hook_type("post-tool"), Some(HookType::PostToolUse));
        assert_eq!(
            verb_to_hook_type("after-thought"),
            Some(HookType::PostToolUse)
        );
        assert_eq!(verb_to_hook_type("unknown"), None);
    }

    // detect_presence tests

    #[test]
    fn test_detect_presence_with_cursor_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cursor")).unwrap();
        let hook = make_hook();
        assert!(hook.detect_presence(tmp.path()));
    }

    #[test]
    fn test_detect_presence_without_cursor_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = make_hook();
        assert!(!hook.detect_presence(tmp.path()));
    }

    // install / uninstall / is_installed are no-ops (managed by atomic-cursor package)

    #[test]
    fn test_install_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = make_hook();
        let result = hook.install(tmp.path()).unwrap();
        assert_eq!(result, 0);
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

    // extract_session_id tests

    #[test]
    fn test_extract_session_id_prefers_conversation_id() {
        let id =
            CursorHook::extract_session_id(Some("conv-1".to_string()), Some("sess-1".to_string()));
        assert_eq!(id, "conv-1");
    }

    #[test]
    fn test_extract_session_id_falls_back_to_session_id() {
        let id = CursorHook::extract_session_id(None, Some("sess-1".to_string()));
        assert_eq!(id, "sess-1");
    }

    #[test]
    fn test_extract_session_id_empty_conversation_id_falls_back() {
        let id = CursorHook::extract_session_id(Some("".to_string()), Some("sess-1".to_string()));
        assert_eq!(id, "sess-1");
    }

    #[test]
    fn test_extract_session_id_generates_fallback() {
        let id = CursorHook::extract_session_id(None, None);
        assert!(id.starts_with("cursor-"));
    }

    #[test]
    fn test_extract_session_id_both_empty_generates_fallback() {
        let id = CursorHook::extract_session_id(Some("".to_string()), Some("".to_string()));
        assert!(id.starts_with("cursor-"));
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

    // hooks_path tests

    // PreToolUse passthrough (not a Cursor event, but handled gracefully)
    #[test]
    fn test_parse_pre_tool_use_passthrough() {
        let hook = make_hook();
        let input = br#"{"conversation_id": "c1"}"#;
        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "c1");
        assert_eq!(event.event_type, HookType::PreToolUse);
    }

    // Conversation ID stability across hook types

    #[test]
    fn test_conversation_id_stable_across_hooks() {
        let hook = make_hook();
        let conv_id = "stable-conv-id-123";

        let types_and_inputs: Vec<(HookType, Vec<u8>)> = vec![
            (
                HookType::TurnStart,
                format!(r#"{{"conversation_id": "{}", "prompt": "hello"}}"#, conv_id).into_bytes(),
            ),
            (
                HookType::PostToolUse,
                format!(
                    r#"{{"conversation_id": "{}", "tool_name": "bash"}}"#,
                    conv_id
                )
                .into_bytes(),
            ),
            (
                HookType::TurnEnd,
                format!(
                    r#"{{"conversation_id": "{}", "status": "completed"}}"#,
                    conv_id
                )
                .into_bytes(),
            ),
        ];

        for (ht, input) in &types_and_inputs {
            let event = hook.parse_event(*ht, input).unwrap();
            assert_eq!(
                event.session_id, conv_id,
                "Session ID should be stable for {:?}",
                ht
            );
        }
    }
}

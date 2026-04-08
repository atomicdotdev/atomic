//! Event parsing for Claude Code hooks.
//!
//! This module contains the JSON input types that Claude Code sends to hooks
//! and the core parsing logic for converting raw JSON into [`TurnEvent`]s.

use serde::Deserialize;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};
use crate::hooks::AgentHook;

// ============================================================================
// CLAUDE CODE JSON INPUT TYPES
// ============================================================================

/// JSON input for session-end and stop hooks.
#[derive(Debug, Deserialize)]
pub(super) struct SessionInfoInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
}

/// JSON input for session-start hook.
///
/// Unlike other session hooks, SessionStart includes `model` (the Claude model
/// identifier, e.g. "claude-sonnet-4-5-20250929") and `source` (how the session
/// started: "startup", "resume", "clear", "compact").
///
/// See: https://code.claude.com/docs/en/hooks#sessionstart-input
#[derive(Debug, Deserialize)]
pub(super) struct SessionStartInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// The model identifier (e.g., "claude-sonnet-4-5-20250929").
    #[serde(default)]
    pub model: Option<String>,
    /// How the session started: "startup", "resume", "clear", "compact".
    #[serde(default)]
    pub source: Option<String>,
}

/// JSON input for user-prompt-submit hook.
#[derive(Debug, Deserialize)]
pub(super) struct UserPromptInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// JSON input for pre-task (PreToolUse[Task]) hook.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(super) struct PreToolInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
}

/// JSON input for post-task and post-todo (PostToolUse) hooks.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(super) struct PostToolInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,
}

// ============================================================================
// PARSE HOOK EVENT
// ============================================================================

/// Parse raw JSON input from a Claude Code hook into a [`TurnEvent`].
///
/// This is the core parsing logic extracted from the `AgentHook::parse_event`
/// implementation. Each hook type has a different JSON schema.
pub(super) fn parse_hook_event(
    hook: &super::ClaudeCodeHook,
    hook_type: HookType,
    input: &[u8],
) -> AgentResult<TurnEvent> {
    let agent_name = hook.name();
    if input.is_empty() {
        return Err(AgentError::HookInputEmpty {
            agent: agent_name.to_string(),
            hook_type: hook_type.as_str().to_string(),
        });
    }

    // Preserve raw JSON for debugging
    let raw_json: serde_json::Value =
        serde_json::from_slice(input).map_err(|e| AgentError::HookParseFailed {
            agent: agent_name.to_string(),
            hook_type: hook_type.as_str().to_string(),
            reason: e.to_string(),
        })?;

    match hook_type {
        HookType::SessionStart => parse_session_start(agent_name, hook_type, raw_json),
        HookType::SessionEnd | HookType::TurnEnd => {
            parse_session_info(agent_name, hook_type, raw_json)
        }
        HookType::TurnStart => parse_user_prompt(agent_name, hook_type, raw_json),
        HookType::PreToolUse => parse_pre_tool(agent_name, hook_type, raw_json),
        HookType::PostToolUse => parse_post_tool(agent_name, hook_type, raw_json),
    }
}

/// Extract the session_id from parsed input, returning a default if missing.
fn extract_session_id(session_id: Option<String>) -> String {
    session_id
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn parse_session_start(
    agent_name: &str,
    hook_type: HookType,
    raw_json: serde_json::Value,
) -> AgentResult<TurnEvent> {
    let parsed: SessionStartInput =
        serde_json::from_value(raw_json.clone()).map_err(|e| AgentError::HookParseFailed {
            agent: agent_name.to_string(),
            hook_type: hook_type.as_str().to_string(),
            reason: e.to_string(),
        })?;

    let mut event =
        TurnEvent::new(extract_session_id(parsed.session_id), hook_type).with_raw_json(raw_json);

    if let Some(path) = parsed.transcript_path {
        event = event.with_transcript_path(path);
    }

    // SessionStart includes model and source — store in raw_data
    // so the orchestrator can set session.model from the real value
    // rather than guessing.
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

fn parse_session_info(
    agent_name: &str,
    hook_type: HookType,
    raw_json: serde_json::Value,
) -> AgentResult<TurnEvent> {
    let parsed: SessionInfoInput =
        serde_json::from_value(raw_json.clone()).map_err(|e| AgentError::HookParseFailed {
            agent: agent_name.to_string(),
            hook_type: hook_type.as_str().to_string(),
            reason: e.to_string(),
        })?;

    let mut event =
        TurnEvent::new(extract_session_id(parsed.session_id), hook_type).with_raw_json(raw_json);

    if let Some(path) = parsed.transcript_path {
        event = event.with_transcript_path(path);
    }

    Ok(event)
}

fn parse_user_prompt(
    agent_name: &str,
    hook_type: HookType,
    raw_json: serde_json::Value,
) -> AgentResult<TurnEvent> {
    let parsed: UserPromptInput =
        serde_json::from_value(raw_json.clone()).map_err(|e| AgentError::HookParseFailed {
            agent: agent_name.to_string(),
            hook_type: hook_type.as_str().to_string(),
            reason: e.to_string(),
        })?;

    let mut event =
        TurnEvent::new(extract_session_id(parsed.session_id), hook_type).with_raw_json(raw_json);

    if let Some(path) = parsed.transcript_path {
        event = event.with_transcript_path(path);
    }
    if let Some(prompt) = parsed.prompt {
        event = event.with_prompt(prompt);
    }

    Ok(event)
}

fn parse_pre_tool(
    agent_name: &str,
    hook_type: HookType,
    raw_json: serde_json::Value,
) -> AgentResult<TurnEvent> {
    let parsed: PreToolInput =
        serde_json::from_value(raw_json.clone()).map_err(|e| AgentError::HookParseFailed {
            agent: agent_name.to_string(),
            hook_type: hook_type.as_str().to_string(),
            reason: e.to_string(),
        })?;

    let mut event =
        TurnEvent::new(extract_session_id(parsed.session_id), hook_type).with_raw_json(raw_json);

    if let Some(path) = parsed.transcript_path {
        event = event.with_transcript_path(path);
    }
    if let Some(id) = parsed.tool_use_id {
        event = event.with_tool_use_id(id);
    }
    // For PreToolUse, the tool name comes from the matcher context,
    // not from the JSON input. It will be set by the CLI dispatch layer.

    Ok(event)
}

fn parse_post_tool(
    agent_name: &str,
    hook_type: HookType,
    raw_json: serde_json::Value,
) -> AgentResult<TurnEvent> {
    let parsed: PostToolInput =
        serde_json::from_value(raw_json.clone()).map_err(|e| AgentError::HookParseFailed {
            agent: agent_name.to_string(),
            hook_type: hook_type.as_str().to_string(),
            reason: e.to_string(),
        })?;

    let mut event =
        TurnEvent::new(extract_session_id(parsed.session_id), hook_type).with_raw_json(raw_json);

    if let Some(path) = parsed.transcript_path {
        event = event.with_transcript_path(path);
    }
    if let Some(id) = parsed.tool_use_id {
        event = event.with_tool_use_id(id);
    }
    if let Some(name) = parsed.tool_name {
        event = event.with_tool_name(name);
    }

    Ok(event)
}

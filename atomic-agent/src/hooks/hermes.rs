//! Hermes Agent hook adapter for Atomic Agent.
//!
//! Hermes can run shell hooks for CLI and gateway sessions. Hook commands
//! receive a JSON payload on stdin with fields such as `hook_event_name`,
//! `session_id`, `cwd`, `tool_name`, `tool_input`, and `extra`.
//!
//! This adapter normalizes those shell-hook payloads into Atomic [`TurnEvent`]
//! records. Installation is intentionally left to Hermes-specific packages or
//! documentation so Atomic does not clobber an existing `~/.hermes/config.yaml`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::{AgentError, AgentResult};
use crate::event::{HookType, TurnEvent};

use super::AgentHook;

const HERMES_DIR: &str = ".hermes";

#[derive(Debug)]
pub struct HermesHook {
    _private: (),
}

impl HermesHook {
    pub fn new() -> Self {
        Self { _private: () }
    }

    fn parse_json(&self, hook_type: HookType, input: &[u8]) -> AgentResult<Value> {
        if input.is_empty() {
            return Err(AgentError::HookInputEmpty {
                agent: self.name().to_string(),
                hook_type: hook_type.as_str().to_string(),
            });
        }

        serde_json::from_slice(input).map_err(|e| AgentError::HookParseFailed {
            agent: self.name().to_string(),
            hook_type: hook_type.as_str().to_string(),
            reason: e.to_string(),
        })
    }

    fn extract_session_id(raw: &Value) -> String {
        value_string(raw, "session_id")
            .or_else(|| value_string(raw, "conversation_id"))
            .or_else(|| value_string(raw, "thread_id"))
            .or_else(|| value_at(raw, &["extra", "session_id"]))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "hermes-session".to_string())
    }

    fn extract_prompt(raw: &Value) -> Option<String> {
        value_string(raw, "prompt")
            .or_else(|| value_string(raw, "user_message"))
            .or_else(|| value_string(raw, "message"))
            .or_else(|| value_at(raw, &["extra", "prompt"]))
            .or_else(|| value_at(raw, &["extra", "user_message"]))
            .filter(|s| !s.is_empty())
    }

    fn extract_transcript_path(raw: &Value) -> Option<String> {
        value_string(raw, "transcript_path")
            .or_else(|| value_at(raw, &["extra", "transcript_path"]))
            .or_else(|| value_at(raw, &["extra", "log_path"]))
            .filter(|s| !s.is_empty())
    }

    fn extract_tool_name(raw: &Value) -> Option<String> {
        value_string(raw, "tool_name")
            .or_else(|| value_at(raw, &["extra", "tool_name"]))
            .filter(|s| !s.is_empty())
    }

    fn extract_tool_use_id(raw: &Value) -> Option<String> {
        value_string(raw, "tool_use_id")
            .or_else(|| value_string(raw, "tool_call_id"))
            .or_else(|| value_string(raw, "call_id"))
            .or_else(|| value_string(raw, "id"))
            .or_else(|| value_at(raw, &["extra", "tool_use_id"]))
            .or_else(|| value_at(raw, &["extra", "tool_call_id"]))
            .or_else(|| value_at(raw, &["extra", "call_id"]))
            .filter(|s| !s.is_empty())
    }

    fn config_path() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(HERMES_DIR).join("config.yaml"))
    }
}

impl Default for HermesHook {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentHook for HermesHook {
    fn name(&self) -> &str {
        "hermes"
    }

    fn display_name(&self) -> &str {
        "Hermes Agent"
    }

    fn parse_event(&self, hook_type: HookType, input: &[u8]) -> AgentResult<TurnEvent> {
        let raw_json = self.parse_json(hook_type, input)?;
        let mut event = TurnEvent::new(Self::extract_session_id(&raw_json), hook_type)
            .with_raw_json(with_hermes_provider(raw_json.clone()));

        if let Some(path) = Self::extract_transcript_path(&raw_json) {
            event = event.with_transcript_path(path);
        }

        if hook_type == HookType::TurnStart {
            if let Some(prompt) = Self::extract_prompt(&raw_json) {
                event = event.with_prompt(prompt);
            }
        }

        if matches!(hook_type, HookType::PreToolUse | HookType::PostToolUse) {
            if let Some(name) = Self::extract_tool_name(&raw_json) {
                event = event.with_tool_name(name);
            }
            if let Some(id) = Self::extract_tool_use_id(&raw_json) {
                event = event.with_tool_use_id(id);
            }
        }

        Ok(event)
    }

    fn install(&self, _repo_root: &Path) -> AgentResult<usize> {
        // Hermes stores hooks in YAML config and often prompts for shell-hook
        // consent. Avoid mutating user config from Atomic core; the dedicated
        // Hermes integration package can merge config safely.
        Ok(0)
    }

    fn uninstall(&self, _repo_root: &Path) -> AgentResult<()> {
        Ok(())
    }

    fn is_installed(&self, repo_root: &Path) -> bool {
        repo_root.join(HERMES_DIR).is_dir() || Self::config_path().is_some_and(|path| path.exists())
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
        self.is_installed(repo_root)
    }

    fn hook_verbs(&self) -> Vec<&str> {
        vec![
            "on_session_start",
            "on_session_end",
            "pre_llm_call",
            "post_llm_call",
            "pre_tool_call",
            "post_tool_call",
        ]
    }
}

fn value_string(raw: &Value, key: &str) -> Option<String> {
    raw.get(key).and_then(Value::as_str).map(ToOwned::to_owned)
}

fn value_at(raw: &Value, path: &[&str]) -> Option<String> {
    let mut current = raw;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_str().map(ToOwned::to_owned)
}

fn with_hermes_provider(mut raw: Value) -> Value {
    if let Value::Object(ref mut map) = raw {
        map.entry("provider".to_string())
            .or_insert_with(|| Value::String("hermes".to_string()));
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_turn_start_prompt_from_hermes_payload() {
        let hook = HermesHook::new();
        let input = br#"{
            "hook_event_name": "pre_llm_call",
            "session_id": "sess-123",
            "cwd": "/repo",
            "extra": {"prompt": "ship this"}
        }"#;

        let event = hook.parse_event(HookType::TurnStart, input).unwrap();
        assert_eq!(event.session_id, "sess-123");
        assert_eq!(event.event_type, HookType::TurnStart);
        assert_eq!(event.prompt.as_deref(), Some("ship this"));
        assert_eq!(
            event.raw_json.as_ref().unwrap()["provider"].as_str(),
            Some("hermes")
        );
    }

    #[test]
    fn parses_tool_payload_fields() {
        let hook = HermesHook::new();
        let input = br#"{
            "hook_event_name": "pre_tool_call",
            "session_id": "sess-456",
            "tool_name": "terminal",
            "tool_call_id": "call-1",
            "tool_input": {"command": "cargo test"}
        }"#;

        let event = hook.parse_event(HookType::PreToolUse, input).unwrap();
        assert_eq!(event.session_id, "sess-456");
        assert_eq!(event.tool_name.as_deref(), Some("terminal"));
        assert_eq!(event.tool_use_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn supports_hermes_hook_verbs() {
        let hook = HermesHook::new();
        assert_eq!(hook.name(), "hermes");
        assert!(hook.hook_verbs().contains(&"post_tool_call"));
        assert!(hook.supported_hooks().contains(&HookType::SessionEnd));
    }
}
